//! Sub-layer 6: INJECTION — typed insert/update/delete with idempotency,
//! audit trail, and live-bus event emission.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use common::{
    error::DbError,
    ids::{ProjectId, RowId},
    layer::DbLayer,
    paths::PathManager,
    response::{Response, ResponseMeta},
};

use crate::query::{DbQuery, Write};
use crate::schema::SCHEMA_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectOptions {
    pub idempotency_key: Option<String>,
    pub emit_event: bool,
    pub audit: bool,
    pub timeout_ms: Option<u64>,
}

impl Default for InjectOptions {
    fn default() -> Self {
        Self {
            idempotency_key: None,
            emit_event: true,
            audit: true,
            timeout_ms: Some(5000),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertResult {
    pub inserted: bool,
    pub row_id: Option<RowId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    pub total_rows_affected: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InjectOp {
    Insert {
        project: ProjectId,
        layer: DbLayer,
        sql: String,
        params: Vec<serde_json::Value>,
    },
    Update {
        project: ProjectId,
        layer: DbLayer,
        sql: String,
        params: Vec<serde_json::Value>,
    },
    Delete {
        project: ProjectId,
        layer: DbLayer,
        sql: String,
        params: Vec<serde_json::Value>,
    },
}

#[async_trait]
pub trait DbInject {
    async fn insert(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<RowId>;

    async fn upsert(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<UpsertResult>;

    async fn update(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<()>;

    async fn delete(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<()>;

    async fn batch_inject(&self, ops: Vec<InjectOp>, opts: InjectOptions) -> Response<BatchResult>;
}

pub struct DefaultInject {
    #[allow(dead_code)]
    paths: Arc<PathManager>,
    query: Arc<dyn DbQuery + Send + Sync>,
}

impl DefaultInject {
    pub fn new(paths: Arc<PathManager>, query: Arc<dyn DbQuery + Send + Sync>) -> Self {
        Self { paths, query }
    }

    fn meta(&self, layer: DbLayer, latency_ms: u64, cache_hit: bool) -> ResponseMeta {
        ResponseMeta {
            latency_ms,
            cache_hit,
            source_db: layer,
            query_id: uuid::Uuid::new_v4(),
            schema_version: SCHEMA_VERSION,
        }
    }

    async fn write_with_audit(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: String,
        params: Vec<serde_json::Value>,
        action: &str,
        opts: &InjectOptions,
    ) -> Response<RowId> {
        let timeout = opts.timeout_ms.map(Duration::from_millis);
        let start = std::time::Instant::now();

        // Idempotency check
        if let Some(key) = &opts.idempotency_key {
            let q = self
                .query
                .query_rows(crate::query::Query {
                    project: project.clone(),
                    layer: DbLayer::Audit,
                    sql: "SELECT new_value_hash FROM audit_log
                      WHERE actor = 'idempotency' AND target = ?1 LIMIT 1"
                        .into(),
                    params: vec![serde_json::Value::String(key.clone())],
                })
                .await;
            if q.success && q.data.as_ref().is_some_and(|v| !v.is_empty()) {
                // already applied; return synthetic OK
                return Response::ok(
                    RowId(0),
                    self.meta(layer, start.elapsed().as_millis() as u64, true),
                );
            }
        }

        // Run actual write
        let fut = self.query.write(Write {
            project: project.clone(),
            layer,
            sql,
            params: params.clone(),
        });
        let resp = match timeout {
            Some(t) => match tokio::time::timeout(t, fut).await {
                Ok(r) => r,
                Err(_) => {
                    return Response::err(
                        DbError::Timeout {
                            elapsed_ms: t.as_millis() as u64,
                        },
                        self.meta(layer, t.as_millis() as u64, false),
                    );
                }
            },
            None => fut.await,
        };

        if !resp.success {
            return Response {
                success: false,
                data: None,
                error: resp.error,
                meta: resp.meta,
            };
        }
        let row_id = resp
            .data
            .and_then(|w| w.last_insert_rowid)
            .unwrap_or(RowId(0));

        // Audit
        if opts.audit {
            let new_hash = blake3::hash(
                serde_json::to_string(&params)
                    .unwrap_or_default()
                    .as_bytes(),
            )
            .to_hex()
            .to_string();
            // Bug G-4 (2026-05-01): surface audit-log write failures.
            // Previously `let _ =` swallowed errors, which left audit
            // trail gaps invisible. `mneme history` then showed
            // unexplained gaps with no path to the cause.
            // NOTE: query.write() returns `Response<WriteSummary>` (not
            // Result), so check `.success` and read `.error` instead of
            // pattern-matching Err.
            let resp = self.query.write(Write {
                project: project.clone(),
                layer: DbLayer::Audit,
                sql: "INSERT INTO audit_log(actor, action, layer, target, prev_value_hash, new_value_hash)
                      VALUES('inject', ?1, ?2, ?3, NULL, ?4)".into(),
                params: vec![
                    serde_json::Value::String(action.into()),
                    serde_json::Value::String(format!("{:?}", layer)),
                    serde_json::Value::String(opts.idempotency_key.clone().unwrap_or_default()),
                    serde_json::Value::String(new_hash),
                ],
            }).await;
            if !resp.success {
                let msg = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.as_str())
                    .unwrap_or("unknown");
                tracing::warn!(error = %msg, action = %action, layer = ?layer, "audit_log inject write failed; audit trail will have a gap");
            }
        }

        // Idempotency record
        // Bug G-4 (2026-05-01): same as above — surface failures via
        // Response.success / Response.error (not Result).
        if let Some(key) = &opts.idempotency_key {
            let resp = self
                .query
                .write(Write {
                    project: project.clone(),
                    layer: DbLayer::Audit,
                    sql: "INSERT INTO audit_log(actor, action, layer, target, new_value_hash)
                      VALUES('idempotency', ?1, ?2, ?3, ?4)"
                        .into(),
                    params: vec![
                        serde_json::Value::String(action.into()),
                        serde_json::Value::String(format!("{:?}", layer)),
                        serde_json::Value::String(key.clone()),
                        serde_json::Value::String("done".into()),
                    ],
                })
                .await;
            if !resp.success {
                let msg = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.as_str())
                    .unwrap_or("unknown");
                tracing::warn!(error = %msg, key = %key, "audit_log idempotency write failed; replay safety degraded");
            }
        }

        // (live bus emit handled by caller via supervisor IPC; opts.emit_event is advisory)

        Response::ok(
            row_id,
            self.meta(layer, start.elapsed().as_millis() as u64, false),
        )
    }
}

#[async_trait]
impl DbInject for DefaultInject {
    async fn insert(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<RowId> {
        self.write_with_audit(project, layer, sql.to_string(), params, "insert", &opts)
            .await
    }

    async fn upsert(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<UpsertResult> {
        let r = self
            .write_with_audit(project, layer, sql.to_string(), params, "upsert", &opts)
            .await;
        if r.success {
            Response::ok(
                UpsertResult {
                    inserted: true,
                    row_id: r.data,
                },
                r.meta,
            )
        } else {
            Response {
                success: false,
                data: None,
                error: r.error,
                meta: r.meta,
            }
        }
    }

    async fn update(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<()> {
        let r = self
            .write_with_audit(project, layer, sql.to_string(), params, "update", &opts)
            .await;
        Response {
            success: r.success,
            data: if r.success { Some(()) } else { None },
            error: r.error,
            meta: r.meta,
        }
    }

    async fn delete(
        &self,
        project: &ProjectId,
        layer: DbLayer,
        sql: &str,
        params: Vec<serde_json::Value>,
        opts: InjectOptions,
    ) -> Response<()> {
        let r = self
            .write_with_audit(project, layer, sql.to_string(), params, "delete", &opts)
            .await;
        Response {
            success: r.success,
            data: if r.success { Some(()) } else { None },
            error: r.error,
            meta: r.meta,
        }
    }

    async fn batch_inject(
        &self,
        ops: Vec<InjectOp>,
        _opts: InjectOptions,
    ) -> Response<BatchResult> {
        let mut writes = vec![];
        for op in ops {
            let (project, layer, sql, params) = match op {
                InjectOp::Insert {
                    project,
                    layer,
                    sql,
                    params,
                }
                | InjectOp::Update {
                    project,
                    layer,
                    sql,
                    params,
                }
                | InjectOp::Delete {
                    project,
                    layer,
                    sql,
                    params,
                } => (project, layer, sql, params),
            };
            writes.push(Write {
                project,
                layer,
                sql,
                params,
            });
        }
        let resp = self.query.write_batch(writes).await;
        if resp.success {
            let total = resp.data.map(|b| b.total_rows_affected).unwrap_or(0);
            Response::ok(
                BatchResult {
                    total_rows_affected: total,
                },
                resp.meta,
            )
        } else {
            Response {
                success: false,
                data: None,
                error: resp.error,
                meta: resp.meta,
            }
        }
    }
}
