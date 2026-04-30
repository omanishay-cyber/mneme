//! Sub-layer 4: QUERY — typed reads (multi-reader) + writes (single-writer per shard).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params_from_iter;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::error::SendTimeoutError;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::error;
use uuid::Uuid;

use common::{
    error::{DbError, DtError, DtResult},
    ids::{ProjectId, RowId},
    layer::DbLayer,
    paths::PathManager,
    response::{Response, ResponseMeta},
    time::Timestamp,
};

use crate::schema::SCHEMA_VERSION;

// M15 — bounded per-shard writer channel.
//
// `WRITER_CHANNEL_CAP` is the depth that backpressures upstream callers
// when the writer task is healthy. `WRITER_SEND_TIMEOUT_SECS` is the
// upper bound a caller will wait for the writer to drain when the
// channel is full — beyond that we surface `DbError::Timeout` instead
// of blocking forever (which is what `.send().await` would do if the
// per-shard writer task wedges on slow disk / migration / OS lock).
//
// AI-DNA pace: cap bumped from 256 → 1024 (4×). Per-shard, that's
// 4× the AI-burst-rate headroom; across 26 shards that's 26 624 in-flight
// writes the supervisor can absorb without back-pressuring the watcher
// pipeline. The single-writer-per-shard invariant is preserved (still
// one writer task draining the channel) — only the input buffer grows.
// The `send_timeout(WRITER_SEND_TIMEOUT_SECS)` fallthrough already
// guarantees no caller blocks forever, so a deeper buffer never trades
// liveness for throughput. See `feedback_mneme_ai_dna_pace.md` Principle
// B: "every queue depth tuned for AI-rate, not human-rate".
pub(crate) const WRITER_CHANNEL_CAP: usize = 1024;
pub(crate) const WRITER_SEND_TIMEOUT_SECS: u64 = 30;

/// Read query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub project: ProjectId,
    pub layer: DbLayer,
    pub sql: String,
    pub params: Vec<serde_json::Value>,
}

/// Write request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Write {
    pub project: ProjectId,
    pub layer: DbLayer,
    pub sql: String,
    pub params: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteSummary {
    pub rows_affected: usize,
    pub last_insert_rowid: Option<RowId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSummary {
    pub total_rows_affected: usize,
}

#[async_trait]
pub trait DbQuery {
    async fn query_rows(&self, q: Query) -> Response<Vec<serde_json::Value>>;
    async fn write(&self, w: Write) -> Response<WriteSummary>;
    async fn write_batch(&self, ws: Vec<Write>) -> Response<BatchSummary>;
}

/// Default impl: per-shard MPSC writer task + per-shard r2d2 read pool.
pub struct DefaultQuery {
    paths: Arc<PathManager>,
    writers: Arc<RwLock<HashMap<ShardKey, mpsc::Sender<WriteCmd>>>>,
    readers: Arc<RwLock<HashMap<ShardKey, Pool<SqliteConnectionManager>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShardKey {
    project: ProjectId,
    layer: DbLayer,
}

enum WriteCmd {
    Single {
        sql: String,
        params: Vec<serde_json::Value>,
        reply: oneshot::Sender<Result<WriteSummary, DbError>>,
    },
    Batch {
        items: Vec<(String, Vec<serde_json::Value>)>,
        reply: oneshot::Sender<Result<BatchSummary, DbError>>,
    },
}

impl DefaultQuery {
    pub fn new(paths: Arc<PathManager>) -> Self {
        Self {
            paths,
            writers: Arc::new(RwLock::new(HashMap::new())),
            readers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn writer(&self, project: &ProjectId, layer: DbLayer) -> mpsc::Sender<WriteCmd> {
        let key = ShardKey { project: project.clone(), layer };
        {
            let map = self.writers.read().await;
            if let Some(tx) = map.get(&key) {
                return tx.clone();
            }
        }
        let mut map = self.writers.write().await;
        if let Some(tx) = map.get(&key) {
            return tx.clone();
        }
        let path = self.paths.shard_db(project, layer);
        let (tx, rx) = mpsc::channel::<WriteCmd>(WRITER_CHANNEL_CAP);
        spawn_writer_task(path, rx);
        map.insert(key, tx.clone());
        tx
    }

    async fn read_pool(&self, project: &ProjectId, layer: DbLayer) -> DtResult<Pool<SqliteConnectionManager>> {
        let key = ShardKey { project: project.clone(), layer };
        {
            let map = self.readers.read().await;
            if let Some(p) = map.get(&key) {
                return Ok(p.clone());
            }
        }
        let mut map = self.readers.write().await;
        if let Some(p) = map.get(&key) {
            return Ok(p.clone());
        }
        let path = self.paths.shard_db(project, layer);
        let mgr = SqliteConnectionManager::file(&path).with_init(|c| {
            c.pragma_update(None, "query_only", true)?;
            c.pragma_update(None, "temp_store", "MEMORY")?;
            Ok(())
        });
        let pool = Pool::builder()
            .max_size((num_cpus_or(4) * 2) as u32)
            .build(mgr)
            .map_err(|e| DtError::Internal(format!("r2d2: {}", e)))?;
        map.insert(key, pool.clone());
        Ok(pool)
    }
}

#[async_trait]
impl DbQuery for DefaultQuery {
    async fn query_rows(&self, q: Query) -> Response<Vec<serde_json::Value>> {
        let start = std::time::Instant::now();
        let meta = |layer| ResponseMeta {
            latency_ms: start.elapsed().as_millis() as u64,
            cache_hit: false,
            source_db: layer,
            query_id: Uuid::new_v4(),
            schema_version: SCHEMA_VERSION,
        };
        let pool = match self.read_pool(&q.project, q.layer).await {
            Ok(p) => p,
            Err(DtError::Db(e)) => return Response::err(e, meta(q.layer)),
            Err(e) => return Response::err(DbError::Sqlite(e.to_string()), meta(q.layer)),
        };
        let layer = q.layer;
        let res = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>, DbError> {
            let conn = pool.get().map_err(|e| DbError::Sqlite(e.to_string()))?;
            let mut stmt = conn.prepare_cached(&q.sql)?;
            let column_names: Vec<String> =
                stmt.column_names().iter().map(|s| s.to_string()).collect();
            let params = json_params(&q.params);
            let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
                let mut obj = serde_json::Map::new();
                for (i, name) in column_names.iter().enumerate() {
                    let v: rusqlite::types::Value = r.get(i)?;
                    obj.insert(name.clone(), value_to_json(v));
                }
                Ok(serde_json::Value::Object(obj))
            })?;
            let mut out = vec![];
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await;
        match res {
            Ok(Ok(rows)) => Response::ok(rows, meta(layer)),
            Ok(Err(e)) => Response::err(e, meta(layer)),
            Err(e) => Response::err(DbError::Sqlite(format!("join: {}", e)), meta(layer)),
        }
    }

    async fn write(&self, w: Write) -> Response<WriteSummary> {
        let start = std::time::Instant::now();
        let meta = |layer| ResponseMeta {
            latency_ms: start.elapsed().as_millis() as u64,
            cache_hit: false,
            source_db: layer,
            query_id: Uuid::new_v4(),
            schema_version: SCHEMA_VERSION,
        };
        let tx = self.writer(&w.project, w.layer).await;
        let (rtx, rrx) = oneshot::channel();
        let layer = w.layer;
        let cmd = WriteCmd::Single {
            sql: w.sql,
            params: w.params,
            reply: rtx,
        };
        // M15 — bounded channel, time-bounded send. If the writer task is
        // wedged the caller surfaces DbError::Timeout instead of blocking
        // forever.
        match tx
            .send_timeout(cmd, Duration::from_secs(WRITER_SEND_TIMEOUT_SECS))
            .await
        {
            Ok(()) => {}
            Err(SendTimeoutError::Timeout(_)) => {
                return Response::err(
                    DbError::Timeout {
                        elapsed_ms: WRITER_SEND_TIMEOUT_SECS * 1000,
                    },
                    meta(layer),
                );
            }
            Err(SendTimeoutError::Closed(_)) => {
                return Response::err(
                    DbError::Sqlite("writer channel closed".into()),
                    meta(layer),
                );
            }
        }
        match rrx.await {
            Ok(Ok(s)) => Response::ok(s, meta(layer)),
            Ok(Err(e)) => Response::err(e, meta(layer)),
            Err(_) => Response::err(
                DbError::Sqlite("writer dropped reply".into()),
                meta(layer),
            ),
        }
    }

    async fn write_batch(&self, ws: Vec<Write>) -> Response<BatchSummary> {
        let start = std::time::Instant::now();
        let layer = ws.first().map(|w| w.layer).unwrap_or(DbLayer::Audit);
        let project = match ws.first() {
            Some(w) => w.project.clone(),
            None => {
                return Response::ok(
                    BatchSummary { total_rows_affected: 0 },
                    ResponseMeta {
                        latency_ms: 0,
                        cache_hit: false,
                        source_db: layer,
                        query_id: Uuid::new_v4(),
                        schema_version: SCHEMA_VERSION,
                    },
                )
            }
        };
        let meta = |layer| ResponseMeta {
            latency_ms: start.elapsed().as_millis() as u64,
            cache_hit: false,
            source_db: layer,
            query_id: Uuid::new_v4(),
            schema_version: SCHEMA_VERSION,
        };
        // Group by layer; each batch goes to its writer task
        let mut by_layer: HashMap<DbLayer, Vec<(String, Vec<serde_json::Value>)>> = HashMap::new();
        for w in ws {
            by_layer.entry(w.layer).or_default().push((w.sql, w.params));
        }
        let mut total = 0usize;
        for (layer, items) in by_layer {
            let tx = self.writer(&project, layer).await;
            let (rtx, rrx) = oneshot::channel();
            // M15 — same time-bounded send for batch writes.
            match tx
                .send_timeout(
                    WriteCmd::Batch { items, reply: rtx },
                    Duration::from_secs(WRITER_SEND_TIMEOUT_SECS),
                )
                .await
            {
                Ok(()) => {}
                Err(SendTimeoutError::Timeout(_)) => {
                    return Response::err(
                        DbError::Timeout {
                            elapsed_ms: WRITER_SEND_TIMEOUT_SECS * 1000,
                        },
                        meta(layer),
                    );
                }
                Err(SendTimeoutError::Closed(_)) => {
                    return Response::err(
                        DbError::Sqlite("writer channel closed".into()),
                        meta(layer),
                    );
                }
            }
            match rrx.await {
                Ok(Ok(b)) => total += b.total_rows_affected,
                Ok(Err(e)) => return Response::err(e, meta(layer)),
                Err(_) => return Response::err(DbError::Sqlite("writer dropped reply".into()), meta(layer)),
            }
        }
        Response::ok(BatchSummary { total_rows_affected: total }, meta(layer))
    }
}

fn spawn_writer_task(path: PathBuf, mut rx: mpsc::Receiver<WriteCmd>) {
    tokio::task::spawn_blocking(move || {
        let conn = match rusqlite::Connection::open(&path) {
            Ok(c) => c,
            Err(e) => {
                error!(path = %path.display(), error = %e, "writer cannot open shard");
                return;
            }
        };
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let _ = conn.pragma_update(None, "foreign_keys", "ON");

        // K10 chaos-test hook (compiled out of release binaries):
        // when `MNEME_TEST_FAIL_FS_AT_BYTES` is set, install
        // update_hook + commit_hook that count writes and return
        // rollback once the budget is exhausted — simulating
        // `SQLITE_FULL` semantics without a real custom VFS.
        // Production builds without `--features test-hooks` skip this
        // entirely (the cfg-gated `crate::test_fs_full` module isn't
        // compiled in).
        #[cfg(any(test, feature = "test-hooks"))]
        let _fs_full_counter = crate::test_fs_full::install_full_disk_hook(&conn);

        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                WriteCmd::Single { sql, params, reply } => {
                    let res = (|| -> Result<WriteSummary, DbError> {
                        let mut stmt = conn.prepare_cached(&sql)?;
                        let p = json_params(&params);
                        let n = stmt.execute(params_from_iter(p.iter()))?;
                        Ok(WriteSummary {
                            rows_affected: n,
                            last_insert_rowid: Some(RowId(conn.last_insert_rowid())),
                        })
                    })();
                    let _ = reply.send(res);
                }
                WriteCmd::Batch { items, reply } => {
                    let res = (|| -> Result<BatchSummary, DbError> {
                        let tx = conn.unchecked_transaction()?;
                        let mut total = 0;
                        for (sql, params) in items {
                            let mut stmt = tx.prepare_cached(&sql)?;
                            let p = json_params(&params);
                            total += stmt.execute(params_from_iter(p.iter()))?;
                        }
                        tx.commit()?;
                        Ok(BatchSummary { total_rows_affected: total })
                    })();
                    let _ = reply.send(res);
                }
            }
        }
    });
}

fn json_params(values: &[serde_json::Value]) -> Vec<rusqlite::types::Value> {
    values.iter().map(json_to_value).collect()
}

fn json_to_value(v: &serde_json::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Integer(if *b { 1 } else { 0 }),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Real(f)
            } else {
                Value::Text(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn value_to_json(v: rusqlite::types::Value) -> serde_json::Value {
    use rusqlite::types::Value;
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Integer(i) => serde_json::Value::Number(i.into()),
        Value::Real(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Text(s) => serde_json::Value::String(s),
        Value::Blob(b) => serde_json::Value::String(hex(&b)),
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

fn num_cpus_or(default: usize) -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(default)
}

// Used by Timestamp imports indirectly; keep a no-op anchor so unused import warnings stay quiet.
#[allow(dead_code)]
fn _t() -> Timestamp { Timestamp::now() }

#[cfg(test)]
mod tests {
    //! M15 — writer channel must not block forever when the per-shard
    //! writer task stalls. We assert that once the bounded channel is
    //! full and the writer is not draining, callers see a structured
    //! `DbError::Timeout` instead of hanging indefinitely.
    use super::*;
    use std::time::{Duration, Instant};

    /// Build a `DefaultQuery`, override its writer task with a stalled
    /// drain, fill the 256-cap channel, and assert the 257th write
    /// returns `DbError::Timeout` within the configured budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn write_returns_timeout_when_writer_channel_full() {
        // Fresh sandboxed paths so we don't touch the real ~/.mneme.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Arc::new(PathManager::with_root(dir.path().to_path_buf()));
        let query = DefaultQuery::new(paths.clone());

        // Pre-install a stalled writer for the shard we're about to hit.
        // The "writer task" here just holds the receiver and never
        // drains — emulating a wedged disk / migration / OS lock.
        let project = ProjectId::from_path(dir.path()).unwrap();
        let layer = DbLayer::Audit;
        let key = ShardKey { project: project.clone(), layer };
        let (tx, rx) = mpsc::channel::<WriteCmd>(super::WRITER_CHANNEL_CAP);
        // Hold the rx forever. Move it into a spawned task that just
        // sits there — nothing ever calls `rx.recv()`.
        let _stall = tokio::spawn(async move {
            let _hold = rx;
            // Park until the test ends.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        query.writers.write().await.insert(key, tx.clone());

        // Saturate the channel: send_timeout returns immediately while
        // there is still capacity. After WRITER_CHANNEL_CAP successful
        // sends every further `send_timeout` must trip the timeout.
        for i in 0..super::WRITER_CHANNEL_CAP {
            let (rtx, _rrx) = oneshot::channel();
            tx.try_send(WriteCmd::Single {
                sql: format!("-- saturate {}", i),
                params: vec![],
                reply: rtx,
            })
            .expect("channel still has capacity during saturation");
        }
        // Sanity: channel is now full.
        let (rtx, _rrx) = oneshot::channel();
        let try_full = tx.try_send(WriteCmd::Single {
            sql: "-- overflow probe".into(),
            params: vec![],
            reply: rtx,
        });
        assert!(
            try_full.is_err(),
            "expected the writer channel to be saturated before the timeout probe"
        );

        // The (cap+1)-th caller should NOT hang. It must surface a
        // structured timeout within ~WRITER_SEND_TIMEOUT seconds.
        let start = Instant::now();
        let resp = tokio::time::timeout(
            Duration::from_secs(super::WRITER_SEND_TIMEOUT_SECS + 5),
            query.write(Write {
                project: project.clone(),
                layer,
                sql: "INSERT INTO unused VALUES (?1)".into(),
                params: vec![serde_json::Value::Null],
            }),
        )
        .await
        .expect("write() must return within budget; hanging means M15 is unfixed");
        let elapsed = start.elapsed();

        assert!(
            !resp.success,
            "stalled writer must produce an error response, got success"
        );
        let kind = resp
            .error
            .as_ref()
            .map(|e| e.kind.clone())
            .unwrap_or_default();
        assert_eq!(
            kind, "timeout",
            "stalled writer must surface DbError::Timeout (kind=\"timeout\"), got kind={kind:?}"
        );
        assert!(
            elapsed >= Duration::from_secs(super::WRITER_SEND_TIMEOUT_SECS),
            "timeout fired too early: elapsed={elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(super::WRITER_SEND_TIMEOUT_SECS + 5),
            "timeout fired too late: elapsed={elapsed:?}"
        );
    }
}
