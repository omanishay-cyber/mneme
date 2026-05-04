//! Filesystem watcher — incremental re-index on save.
//!
//! Wraps the `notify` crate v6 for cross-platform watching (inotify on Linux,
//! FSEvents on macOS, ReadDirectoryChangesW on Windows). Events are debounced
//! over a short window (default 250ms) so a burst of saves from a single
//! editor write collapses into one re-index, and the debounced set is
//! dispatched to the parser + store pipeline on a background task.
//!
//! The pipeline goals are:
//!
//! - p95 save-to-graph latency under 500ms.
//! - rename/move handled via a `Remove` followed by a `Create` — both are
//!   routed through the same code path.
//! - delete removes all nodes + edges for the file atomically.
//! - 100% local — no network I/O, no MCP round-trip.
//!
//! A `file_reindexed` event is emitted on the livebus so subscribers
//! (Claude Code session, vision app) can refresh immediately.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind, RemoveKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use common::{ids::ProjectId, layer::DbLayer, paths::PathManager};
use parsers::{
    extractor::Extractor, incremental::IncrementalParser, looks_like_test_path,
    parser_pool::ParserPool, Language, NodeKind,
};
use store::{inject::InjectOptions, Store};

/// Debounce window for collapsing burst saves.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(250);

/// Hard cap on tracked pending events to avoid unbounded memory if the user
/// does something wild like untarring 100k files.
///
/// AI-DNA pace: bumped from 10_000 to 65_536 (≈6.5×). We index 1000+
/// file projects; AI mass-rename / mass-format operations easily emit
/// 10k+ filesystem events in a single burst. Under the legacy cap the
/// debouncer silently dropped events past 10k — visible to the user as
/// "some files weren't re-indexed". 64k absorbs full project-wide bursts
/// while still bounding memory: each `PendingBatch` is ~32B + a `PathBuf`
/// (~50B avg on Windows) so 64k pending entries ≈ 5 MB. Tunable at
/// runtime via `MNEME_WATCHER_MAX_PENDING`.
///
/// See `feedback_mneme_ai_dna_pace.md` Principle B: "Same-speed indexing.
/// When AI is editing 10 files in 30 seconds, the watcher → re-parse →
/// re-embed → graph-update path completes before the next AI tool call.
/// No back-pressure visible to the AI."
pub const MAX_PENDING: usize = 65_536;

fn max_pending() -> usize {
    std::env::var("MNEME_WATCHER_MAX_PENDING")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_PENDING)
}

/// Classification of a single debounced file event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// File created or modified (rename targets land here).
    Upsert,
    /// File deleted (rename sources land here).
    Delete,
}

/// Snapshot of watcher-wide performance counters. Surfaced by the SLA
/// dashboard so `mneme daemon status` can show the p95 latency.
#[derive(Debug, Clone, Default)]
pub struct WatcherStats {
    /// Total debounced re-index jobs that ran to completion.
    pub total_reindexed: u64,
    /// Total files the watcher dropped because they were ignored.
    pub total_ignored: u64,
    /// Total delete events processed.
    pub total_deletes: u64,
    /// p50 latency in milliseconds.
    pub p50_ms: u64,
    /// p95 latency in milliseconds.
    pub p95_ms: u64,
    /// p99 latency in milliseconds.
    pub p99_ms: u64,
    /// The most recent 256 latency samples. Used to recompute percentiles.
    samples_ms: Vec<u64>,
}

impl WatcherStats {
    const SAMPLE_CAP: usize = 256;

    fn record(&mut self, latency_ms: u64, kind: ChangeKind) {
        match kind {
            ChangeKind::Upsert => self.total_reindexed = self.total_reindexed.saturating_add(1),
            ChangeKind::Delete => {
                self.total_deletes = self.total_deletes.saturating_add(1);
                self.total_reindexed = self.total_reindexed.saturating_add(1);
            }
        }
        if self.samples_ms.len() >= Self::SAMPLE_CAP {
            self.samples_ms.remove(0);
        }
        self.samples_ms.push(latency_ms);
        self.recompute();
    }

    fn recompute(&mut self) {
        if self.samples_ms.is_empty() {
            self.p50_ms = 0;
            self.p95_ms = 0;
            self.p99_ms = 0;
            return;
        }
        let mut sorted = self.samples_ms.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        let idx = |q: f64| -> usize {
            let i = (q * n as f64).floor() as usize;
            i.min(n.saturating_sub(1))
        };
        self.p50_ms = sorted[idx(0.50)];
        self.p95_ms = sorted[idx(0.95)];
        self.p99_ms = sorted[idx(0.99)];
    }
}

/// Shared stats handle so the health server can read them without owning the
/// watcher.
#[derive(Debug, Clone, Default)]
pub struct WatcherStatsHandle(Arc<Mutex<WatcherStats>>);

impl WatcherStatsHandle {
    /// Create a fresh stats handle backed by a zero-initialised `WatcherStats`.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(WatcherStats::default())))
    }

    /// Take a snapshot of the current stats.
    pub fn snapshot(&self) -> WatcherStats {
        self.0.lock().clone()
    }

    fn record(&self, latency_ms: u64, kind: ChangeKind) {
        self.0.lock().record(latency_ms, kind);
    }

    fn bump_ignored(&self) {
        self.0.lock().total_ignored = self.0.lock().total_ignored.saturating_add(1);
    }
}

/// One debounced batch ready for re-indexing.
struct PendingBatch {
    /// Last time we saw an event for this file.
    seen_at: Instant,
    /// Final classification after coalescing.
    kind: ChangeKind,
}

/// Public entry point — start watching `project_root` and block forever.
///
/// Spawns:
///   1. a blocking thread for the `notify` watcher (required by the crate).
///   2. an async debouncer loop that coalesces same-file events.
///   3. a worker that re-indexes each debounced file via store+parsers.
///
/// The returned future completes only on unrecoverable error. Callers that
/// want graceful shutdown should `tokio::select!` on it alongside their
/// shutdown signal.
pub async fn run_watcher(
    project_root: PathBuf,
    livebus_socket: Option<PathBuf>,
    stats: WatcherStatsHandle,
    debounce: Duration,
) -> Result<(), WatcherError> {
    run_watcher_with_bus(project_root, livebus_socket, None, stats, debounce).await
}

/// BUG-A4-014 fix (2026-05-04): variant of [`run_watcher`] that also
/// accepts an in-process `livebus::EventBus`. When `Some`, the watcher
/// publishes a typed `FileChanged` event onto the bus alongside the
/// legacy out-of-process socket emit, so connected `/ws` clients on the
/// supervisor's HTTP port receive immediate notification instead of the
/// stale silence the audit flagged. When `None`, behaviour is identical
/// to the legacy `run_watcher` -- the wrapper above just calls into
/// here with `bus=None` so existing CLI call sites (`mneme-daemon
/// watch`) keep their wire shape.
pub async fn run_watcher_with_bus(
    project_root: PathBuf,
    livebus_socket: Option<PathBuf>,
    bus: Option<livebus::EventBus>,
    stats: WatcherStatsHandle,
    debounce: Duration,
) -> Result<(), WatcherError> {
    let canonical = dunce::canonicalize(&project_root)
        .map_err(|e| WatcherError::Io(format!("canonicalize {}: {e}", project_root.display())))?;

    info!(root = %canonical.display(), "file watcher starting");

    // ----- 1. Per-project store + parser state ----------------------------
    let paths = PathManager::default_root();
    let store = Arc::new(Store::new(paths.clone()));
    let project_id = ProjectId::from_path(&canonical)
        .map_err(|e| WatcherError::Io(format!("hash project id: {e}")))?;
    let project_name = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string();
    // Ensure the shard exists — if the user calls `daemon watch` before
    // `mneme build`, we still come up cleanly instead of panicking.
    store
        .builder
        .build_or_migrate(&project_id, &canonical, &project_name)
        .await
        .map_err(|e| WatcherError::Io(format!("build_or_migrate: {e}")))?;

    let pool = Arc::new(
        ParserPool::new(4).map_err(|e| WatcherError::Io(format!("parser pool init: {e}")))?,
    );
    let inc = Arc::new(IncrementalParser::new(pool.clone()));

    // ----- 2. notify watcher on a blocking thread -------------------------
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let tx_for_notify = raw_tx.clone();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        // `notify` delivers on its own thread; forward into tokio.
        let _ = tx_for_notify.send(res);
    })
    .map_err(|e| WatcherError::Io(format!("notify init: {e}")))?;

    watcher
        .watch(&canonical, RecursiveMode::Recursive)
        .map_err(|e| WatcherError::Io(format!("notify watch: {e}")))?;

    // Keep the watcher alive for the life of the task.
    let _watcher_guard = watcher;

    // ----- 3. Debounce loop -----------------------------------------------
    let (index_tx, mut index_rx) = mpsc::unbounded_channel::<(PathBuf, ChangeKind)>();
    let stats_for_debounce = stats.clone();
    let root_for_debounce = canonical.clone();

    let pending_cap = max_pending();
    tokio::spawn(async move {
        // AI-DNA pace: pre-allocate the debounce table at 1/16th of the
        // configured pending cap. We don't reserve the full cap up front
        // (4 MB on Windows for 64k entries — wasteful for the steady-state
        // case where pending is 0-50 entries), but we do start at a size
        // that absorbs typical AI-burst windows without rehashing. The map
        // grows as needed beyond this. See `feedback_mneme_ai_dna_pace.md`.
        let initial_buckets = (pending_cap / 16).max(64);
        let mut pending: HashMap<PathBuf, PendingBatch> = HashMap::with_capacity(initial_buckets);
        let mut ticker = tokio::time::interval(debounce / 2);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                maybe_ev = raw_rx.recv() => {
                    match maybe_ev {
                        Some(Ok(ev)) => {
                            for path in classify_event(&ev, &root_for_debounce, &stats_for_debounce) {
                                if pending.len() >= pending_cap {
                                    warn!(pending = pending.len(), cap = pending_cap, "debounce queue full; dropping event");
                                    continue;
                                }
                                let kind = event_kind(&ev);
                                pending
                                    .entry(path.clone())
                                    .and_modify(|b| {
                                        b.seen_at = Instant::now();
                                        // Delete wins over upsert when both happen
                                        // in the window (file removed outright).
                                        if matches!(kind, ChangeKind::Delete) {
                                            b.kind = ChangeKind::Delete;
                                        }
                                    })
                                    .or_insert(PendingBatch {
                                        seen_at: Instant::now(),
                                        kind,
                                    });
                            }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "notify error");
                        }
                        None => {
                            debug!("notify channel closed, debouncer exiting");
                            return;
                        }
                    }
                }
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let ready: Vec<(PathBuf, ChangeKind)> = pending
                        .iter()
                        .filter(|(_, b)| now.duration_since(b.seen_at) >= debounce)
                        .map(|(p, b)| (p.clone(), b.kind))
                        .collect();
                    for (p, _) in &ready {
                        pending.remove(p);
                    }
                    for item in ready {
                        if index_tx.send(item).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });

    // ----- 4. Re-index worker ---------------------------------------------
    // BUG-A4-010 fix (2026-05-04): cap concurrent `reindex_one` tasks
    // with a Semaphore. The legacy code spawned one tokio task per
    // file change with no upper bound -- a 10K-file mass-save (e.g.
    // `cargo fmt` on a workspace) would spawn 10K concurrent tasks all
    // contending for the parser pool (size 4) and the per-project
    // store writer. Worst-case the debouncer's MAX_PENDING (~65K) was
    // the only ceiling. Cap at parser_pool_size * 2 so we keep the
    // pool saturated without piling up scheduler / memory pressure.
    let reindex_cap: usize = (4_usize).saturating_mul(2);
    let reindex_limiter = Arc::new(tokio::sync::Semaphore::new(reindex_cap));
    // BUG-A4-014: project token used for in-process bus events. The
    // hashed ProjectId is the canonical token; if it cannot be rendered
    // we fall back to "local" so the topic still validates.
    let project_token: String = project_id.to_string();
    loop {
        let (path, kind) = match index_rx.recv().await {
            Some(x) => x,
            None => return Ok(()),
        };
        let store = store.clone();
        let inc = inc.clone();
        let project_id = project_id.clone();
        let stats = stats.clone();
        let lb = livebus_socket.clone();
        let limiter = reindex_limiter.clone();
        let bus_for_task = bus.clone();
        let project_token_for_task = project_token.clone();
        // Acquire the permit OUTSIDE the spawn so backpressure flows
        // back into the receive loop -- if we are already at the cap,
        // pulling the next event waits for a permit. This is the whole
        // point of the cap: to throttle the dispatch side, not just the
        // worker count.
        let permit = match limiter.acquire_owned().await {
            Ok(p) => p,
            Err(_) => return Ok(()), // semaphore closed -- treat as shutdown
        };
        // Each job runs concurrently within the cap so the worker
        // doesn't serialize on slow parses.
        tokio::spawn(async move {
            // The permit is moved into the task; dropped automatically
            // when the future completes / panics, releasing the slot.
            let _permit = permit;
            if let Err(e) = reindex_one(
                &store,
                &inc,
                &project_id,
                &path,
                kind,
                &stats,
                lb.as_deref(),
                bus_for_task.as_ref(),
                &project_token_for_task,
            )
            .await
            {
                warn!(path = %path.display(), error = %e, "reindex failed");
            }
        });
    }
}

/// Run one re-index pass for a single file.
#[allow(clippy::too_many_arguments)]
async fn reindex_one(
    store: &Store,
    inc: &IncrementalParser,
    project_id: &ProjectId,
    path: &Path,
    kind: ChangeKind,
    stats: &WatcherStatsHandle,
    livebus_socket: Option<&Path>,
    bus: Option<&livebus::EventBus>,
    project_token: &str,
) -> Result<(), WatcherError> {
    let started = Instant::now();
    let path_str = path.display().to_string();

    // --- Delete branch: purge every node + edge referencing this file ----
    if matches!(kind, ChangeKind::Delete) {
        // Collect qualified names first so we can wipe incoming edges too.
        let q = store
            .query
            .query_rows(store::query::Query {
                project: project_id.clone(),
                layer: DbLayer::Graph,
                sql: "SELECT qualified_name FROM nodes WHERE file_path = ?1".into(),
                params: vec![serde_json::Value::String(path_str.clone())],
            })
            .await;
        let qnames: Vec<String> = q
            .data
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                row.get("qualified_name")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect();

        // Edges by source or target.
        // Bug G-1 (2026-05-01): graph DB writes were silently dropped
        // via `let _ =`. When the DB is locked or full, the watcher
        // would miss edge deletions and the code graph would silently
        // rot — every recall query returns stale data with no warning.
        // Surface failures via tracing::warn so they show up in logs.
        for qn in &qnames {
            let _resp_e = store
                .inject
                .delete(
                    project_id,
                    DbLayer::Graph,
                    "DELETE FROM edges WHERE source_qualified = ?1 OR target_qualified = ?1",
                    vec![serde_json::Value::String(qn.clone())],
                    InjectOptions {
                        emit_event: false,
                        audit: false,
                        ..InjectOptions::default()
                    },
                )
                .await;
            if !_resp_e.success {
                let _err_msg = _resp_e
                    .error
                    .as_ref()
                    .map(|e| e.message.as_str())
                    .unwrap_or("unknown");
                tracing::warn!(
                    error = %_err_msg,
                    qualified_name = %qn,
                    "watcher: failed to delete edges for symbol; graph may drift"
                );
            }
        }
        // Finally the nodes themselves.
        let _resp_n = store
            .inject
            .delete(
                project_id,
                DbLayer::Graph,
                "DELETE FROM nodes WHERE file_path = ?1",
                vec![serde_json::Value::String(path_str.clone())],
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if !_resp_n.success {
            let _err_msg = _resp_n
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown");
            tracing::warn!(
                error = %_err_msg,
                path = %path_str,
                "watcher: failed to delete nodes for file; graph may drift"
            );
        }

        let latency_ms = started.elapsed().as_millis() as u64;
        stats.record(latency_ms, ChangeKind::Delete);
        emit_file_reindexed(
            livebus_socket,
            &path_str,
            latency_ms,
            -(qnames.len() as i64),
            0,
        );
        // BUG-A4-014 fix: also emit on the in-process bus when one is
        // wired in, so connected `/ws` clients see the delete event.
        emit_filechanged_to_bus(
            bus,
            project_token,
            &path_str,
            livebus::event::FileChangeKind::Deleted,
            None,
            None,
        );
        return Ok(());
    }

    // --- Upsert branch: parse + extract + idempotent re-insert -----------
    let Some(lang) = Language::from_filename(path) else {
        stats.bump_ignored();
        return Ok(());
    };
    if !lang.is_enabled() {
        stats.bump_ignored();
        return Ok(());
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            // Could be a transient read (editor swap). Don't escalate.
            debug!(path = %path.display(), error = %e, "read failed; skipping");
            return Ok(());
        }
    };
    if looks_binary(&bytes) {
        stats.bump_ignored();
        return Ok(());
    }
    let bytes_arc = Arc::new(bytes);
    let parse = inc
        .parse_file(path, lang, bytes_arc.clone())
        .await
        .map_err(|e| WatcherError::Parse(format!("parse {}: {e}", path.display())))?;

    let extractor = Extractor::new(lang);
    let graph = extractor
        .extract(&parse.tree, &bytes_arc, path)
        .map_err(|e| WatcherError::Parse(format!("extract {}: {e}", path.display())))?;

    // I2/K5/I4: file-level facts shared by `files` row + every node we'll
    // emit. `is_test` derives from the path heuristic in
    // `parsers::looks_like_test_path` (which mirrors the patterns
    // `vision/server/shard.ts::fetchTestCoverage` already looks at).
    let is_test_file = looks_like_test_path(path);
    let file_sha = sha256_hex(&bytes_arc);
    let line_count = 1 + bytes_arc.iter().filter(|&&b| b == b'\n').count();
    let byte_count = bytes_arc.len();

    // I2: keep the `files` table in lock-step with `nodes` on every
    // re-index. INSERT OR REPLACE on the path PRIMARY KEY guarantees a
    // single row per file regardless of how many times the watcher
    // bounces.
    let files_sql = "INSERT OR REPLACE INTO files(path, sha256, language, last_parsed_at, line_count, byte_count) \
                     VALUES(?1, ?2, ?3, datetime('now'), ?4, ?5)";
    let files_params = vec![
        serde_json::Value::String(path_str.clone()),
        serde_json::Value::String(file_sha.clone()),
        serde_json::Value::String(format!("{:?}", lang).to_lowercase()),
        serde_json::Value::Number((line_count as i64).into()),
        serde_json::Value::Number((byte_count as i64).into()),
    ];
    // Bug G-1 (2026-05-01): surface failed file-row inserts.
    let _resp_f = store
        .inject
        .insert(
            project_id,
            DbLayer::Graph,
            files_sql,
            files_params,
            InjectOptions {
                emit_event: false,
                audit: false,
                ..InjectOptions::default()
            },
        )
        .await;
    if !_resp_f.success {
        let _err_msg = _resp_f
            .error
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("unknown");
        tracing::warn!(error = %_err_msg, path = %path_str, "watcher: failed to upsert files row; graph may drift");
    }

    // I4: filter Comment nodes from the writer-visible graph. See
    // phase-a-issues.md §I4 — comments inflate node counts ~50% and
    // distort god-node / community / coupling stats.
    let writeable: Vec<&parsers::Node> = graph
        .nodes
        .iter()
        .filter(|n| n.kind != NodeKind::Comment)
        .collect();
    let n_nodes = writeable.len();
    let n_edges = graph.edges.len();

    // Wipe old edges for this file's nodes before re-inserting — nodes use
    // INSERT OR REPLACE on qualified_name so they self-heal, but edges have
    // no natural UNIQUE and would duplicate on every save without this step.
    let old_nodes = store
        .query
        .query_rows(store::query::Query {
            project: project_id.clone(),
            layer: DbLayer::Graph,
            sql: "SELECT qualified_name FROM nodes WHERE file_path = ?1".into(),
            params: vec![serde_json::Value::String(path_str.clone())],
        })
        .await;
    let old_qnames: Vec<String> = old_nodes
        .data
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            row.get("qualified_name")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();
    // Bug G-1 (2026-05-01): surface failed edge wipes.
    for qn in &old_qnames {
        let _resp = store
            .inject
            .delete(
                project_id,
                DbLayer::Graph,
                "DELETE FROM edges WHERE source_qualified = ?1",
                vec![serde_json::Value::String(qn.clone())],
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if !_resp.success {
            let _err_msg = _resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown");
            tracing::warn!(error = %_err_msg, qualified_name = %qn, "watcher: failed to wipe old edges before re-insert; duplicates may accumulate");
        }
    }

    // Upsert nodes + edges.
    for node in &writeable {
        let sql = "INSERT OR REPLACE INTO nodes(kind,name,qualified_name,file_path,line_start,line_end,language,is_test,file_hash,extra,updated_at) \
                   VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,datetime('now'))";
        let params = vec![
            serde_json::Value::String(format!("{:?}", node.kind).to_lowercase()),
            serde_json::Value::String(node.name.clone()),
            serde_json::Value::String(node.id.clone()),
            serde_json::Value::String(node.file.display().to_string()),
            serde_json::Value::Number((node.line_range.0 as i64).into()),
            serde_json::Value::Number((node.line_range.1 as i64).into()),
            serde_json::Value::String(format!("{:?}", node.language).to_lowercase()),
            serde_json::Value::Number((if is_test_file { 1i64 } else { 0i64 }).into()),
            serde_json::Value::String(file_sha.clone()),
            serde_json::Value::String(
                serde_json::json!({
                    "confidence": format!("{:?}", node.confidence).to_lowercase(),
                    "byte_range": [node.byte_range.0, node.byte_range.1],
                })
                .to_string(),
            ),
        ];
        // Bug G-1 (2026-05-01): surface failed node upserts.
        let _resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Graph,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if !_resp.success {
            let _err_msg = _resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown");
            tracing::warn!(error = %_err_msg, qualified_name = %node.id, "watcher: failed to upsert node; graph may drift");
        }
    }
    for edge in &graph.edges {
        let sql = "INSERT INTO edges(kind,source_qualified,target_qualified,confidence,confidence_score,source_extractor,extra,updated_at) \
                   VALUES(?1,?2,?3,?4,?5,?6,?7,datetime('now'))";
        let conf = format!("{:?}", edge.confidence).to_lowercase();
        let score = edge.confidence.weight();
        let params = vec![
            serde_json::Value::String(format!("{:?}", edge.kind).to_lowercase()),
            serde_json::Value::String(edge.from.clone()),
            serde_json::Value::String(edge.to.clone()),
            serde_json::Value::String(conf),
            serde_json::Value::Number(
                serde_json::Number::from_f64(score as f64)
                    .unwrap_or_else(|| serde_json::Number::from(1)),
            ),
            serde_json::Value::String("watcher".into()),
            serde_json::Value::String(
                serde_json::json!({"unresolved": edge.unresolved_target}).to_string(),
            ),
        ];
        // Bug G-1 (2026-05-01): surface failed edge inserts.
        let _resp = store
            .inject
            .insert(
                project_id,
                DbLayer::Graph,
                sql,
                params,
                InjectOptions {
                    emit_event: false,
                    audit: false,
                    ..InjectOptions::default()
                },
            )
            .await;
        if !_resp.success {
            let _err_msg = _resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown");
            tracing::warn!(error = %_err_msg, edge_kind = ?edge.kind, "watcher: failed to insert edge; graph may drift");
        }
    }

    let latency_ms = started.elapsed().as_millis() as u64;
    stats.record(latency_ms, ChangeKind::Upsert);
    emit_file_reindexed(
        livebus_socket,
        &path_str,
        latency_ms,
        n_nodes as i64,
        n_edges as i64,
    );
    // BUG-A4-014 fix: also emit on the in-process bus when one is
    // wired in, so connected `/ws` clients see the upsert event.
    emit_filechanged_to_bus(
        bus,
        project_token,
        &path_str,
        livebus::event::FileChangeKind::Modified,
        None,
        None,
    );
    Ok(())
}

/// Route a raw `notify::Event` to the set of paths we care about.
fn classify_event(ev: &notify::Event, root: &Path, stats: &WatcherStatsHandle) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in &ev.paths {
        if is_ignored(path, root) {
            stats.bump_ignored();
            continue;
        }
        out.push(path.clone());
    }
    out
}

fn event_kind(ev: &notify::Event) -> ChangeKind {
    match ev.kind {
        EventKind::Remove(RemoveKind::File) | EventKind::Remove(RemoveKind::Any) => {
            ChangeKind::Delete
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => ChangeKind::Delete,
        _ => ChangeKind::Upsert,
    }
}

/// Mirrors the ignore list from `cli/src/commands/build.rs` plus the extras
/// from the task spec (`.mneme/`, `.git/`, explicit `node_modules`).
pub fn is_ignored(path: &Path, root: &Path) -> bool {
    // Quick check against the immediate filename first.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if matches!(
            name,
            "target"
                | "node_modules"
                | ".git"
                | ".mneme"
                | "dist"
                | "build"
                | ".next"
                | ".nuxt"
                | ".svelte-kit"
                | ".venv"
                | "venv"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".idea"
                | ".vscode"
        ) {
            return true;
        }
    }
    // And any ancestor component — covers `project/target/debug/foo.rs`.
    let rel = path.strip_prefix(root).unwrap_or(path);
    for comp in rel.components() {
        let s = comp.as_os_str().to_string_lossy();
        match s.as_ref() {
            "target" | "node_modules" | ".git" | ".mneme" | "dist" | "build" | ".next"
            | ".nuxt" | ".svelte-kit" | ".venv" | "venv" | "__pycache__" | ".pytest_cache"
            | ".mypy_cache" | ".ruff_cache" | ".idea" | ".vscode" => return true,
            _ => {}
        }
    }
    // Editor swap files: vim's `.swp`, emacs' `~`, VSCode's `.tmp`.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.ends_with('~') || name.ends_with(".swp") || name.ends_with(".tmp") {
            return true;
        }
        if name.starts_with(".#") {
            return true;
        }
    }
    false
}

/// I2: hex-encoded SHA-256 of a file's bytes. Matches the format the
/// CLI build path writes to `files.sha256` (and the column name in
/// `store/src/schema.rs`). Kept private to the watcher because it's the
/// only consumer in this crate; the CLI has its own copy.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(d.len() * 2);
    for b in d.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn looks_binary(buf: &[u8]) -> bool {
    buf.iter().take(512).any(|&b| b == 0)
}

/// Emit a `file_reindexed` event on the livebus IPC socket if we have one.
///
/// The livebus crate uses newline-delimited JSON framing — we use a plain
/// `std::os`/`interprocess` connection here. The emit is best-effort: if
/// livebus is down, we silently skip so watcher performance never blocks on
/// event delivery.
fn emit_file_reindexed(
    socket: Option<&Path>,
    path: &str,
    ms: u64,
    nodes_delta: i64,
    edges_delta: i64,
) {
    let Some(socket) = socket else { return };
    let topic = "project.local.file_reindexed";
    let payload = serde_json::json!({
        "kind": "file_reindexed",
        "path": path,
        "ms": ms,
        "nodes_delta": nodes_delta,
        "edges_delta": edges_delta,
    });
    let envelope = serde_json::json!({
        "topic": topic,
        "payload": payload,
    });
    let Ok(line) = serde_json::to_string(&envelope) else {
        return;
    };
    let line = format!("{line}\n");
    let socket = socket.to_path_buf();
    // Fire-and-forget on a blocking task so a slow livebus never blocks us.
    tokio::task::spawn_blocking(move || {
        if let Err(e) = send_livebus_line(&socket, &line) {
            debug!(error = %e, "livebus emit failed (non-fatal)");
        }
    });
}

/// 4.2 fix: publish a typed `FileChanged` event onto the in-process
/// [`livebus::EventBus`] so connected `/ws` clients receive immediate
/// notification.
///
/// This is **additive** to [`emit_file_reindexed`] (which targets the
/// legacy out-of-process livebus IPC socket): the bus-side emit reaches
/// in-process subscribers like the supervisor's own `/ws` route, while
/// the socket-side emit stays the path used by the standalone
/// `mneme-livebus` daemon. Either may be `None`; `None` is a no-op.
///
/// Topic shape mirrors the design doc §11 convention
/// `project.<project>.file_changed`. We use `local` as the project token
/// when the watcher does not know the hashed project id — the production
/// `run_watcher` path will pass the real id; tests + ad-hoc invocations
/// fall back to `local` so the topic still validates.
///
/// BUG-A4-014 fix (2026-05-04): now actually called from `reindex_one`
/// when the supervisor's `lib::run` threads in its in-process
/// `EventBus`. The legacy `mneme-daemon watch` CLI path still passes
/// `bus=None` (it has no bus to share) so this helper degrades to a
/// no-op there -- preserving the existing wire shape for that command.
pub(crate) fn emit_filechanged_to_bus(
    bus: Option<&livebus::EventBus>,
    project_token: &str,
    path: &str,
    change_kind: livebus::event::FileChangeKind,
    bytes: Option<u64>,
    content_hash: Option<String>,
) {
    let Some(bus) = bus else { return };
    let topic = format!("project.{project_token}.file_changed");
    let payload = livebus::EventPayload::FileChanged(livebus::FileChanged {
        path: path.to_string(),
        change_kind,
        bytes,
        content_hash,
    });
    let ev = livebus::Event::from_typed(topic, None, Some(project_token.to_string()), payload);
    if let Err(e) = bus.publish(ev) {
        debug!(error = %e, "livebus bus publish failed (non-fatal)");
    }
}

#[cfg(windows)]
fn send_livebus_line(socket: &Path, line: &str) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    let mut file = OpenOptions::new().read(true).write(true).open(socket)?;
    file.write_all(line.as_bytes())?;
    file.flush()
}

#[cfg(unix)]
fn send_livebus_line(socket: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Watcher-specific error type. Kept small on purpose — most failures inside
/// the hot path are logged and swallowed so the watcher never dies.
#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    /// I/O error reading change events or graph DB.
    #[error("io: {0}")]
    Io(String),
    /// Parsing or graph-conversion error on a changed file.
    #[error("parse: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_common_build_dirs() {
        let root = PathBuf::from("/proj");
        assert!(is_ignored(Path::new("/proj/target/debug/foo.rs"), &root));
        assert!(is_ignored(
            Path::new("/proj/node_modules/pkg/index.js"),
            &root
        ));
        assert!(is_ignored(Path::new("/proj/.git/HEAD"), &root));
        assert!(is_ignored(Path::new("/proj/.mneme/graph.db"), &root));
        assert!(!is_ignored(Path::new("/proj/src/main.rs"), &root));
    }

    /// 4.2: when the watcher emits a `FileChanged` event to the
    /// in-process [`livebus::EventBus`], a `subscribe_raw` consumer
    /// must receive a properly-shaped `Event` with the right topic.
    /// This is the unit-level analog of "edit a file on disk → /ws
    /// client sees an event" — exercising the bridge so a regression
    /// in `emit_filechanged_to_bus` shows up immediately.
    #[tokio::test]
    async fn livebus_emits_filechanged_event_when_file_modifies() {
        let bus = livebus::EventBus::new();
        let mut rx = bus.subscribe_raw();

        emit_filechanged_to_bus(
            Some(&bus),
            "abc123",
            "src/lib.rs",
            livebus::event::FileChangeKind::Modified,
            Some(4096),
            None,
        );

        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("did not receive event in time")
            .expect("bus closed unexpectedly");
        assert_eq!(ev.topic, "project.abc123.file_changed");
        assert_eq!(ev.project_hash.as_deref(), Some("abc123"));
        // Round-trip the JSON payload to confirm the typed shape.
        assert_eq!(
            ev.payload["kind"],
            serde_json::Value::String("file_changed".into())
        );
        assert_eq!(
            ev.payload["path"],
            serde_json::Value::String("src/lib.rs".into())
        );
        assert_eq!(
            ev.payload["change_kind"],
            serde_json::Value::String("modified".into())
        );
    }

    #[test]
    fn ignores_editor_swap_files() {
        let root = PathBuf::from("/proj");
        assert!(is_ignored(Path::new("/proj/src/.main.rs.swp"), &root));
        assert!(is_ignored(Path::new("/proj/src/main.rs~"), &root));
        assert!(is_ignored(Path::new("/proj/src/.#main.rs"), &root));
    }

    #[test]
    fn stats_record_and_recompute_percentiles() {
        let s = WatcherStatsHandle::new();
        for ms in [50u64, 100, 150, 200, 250, 300, 400, 500, 100, 120] {
            s.record(ms, ChangeKind::Upsert);
        }
        let snap = s.snapshot();
        assert_eq!(snap.total_reindexed, 10);
        assert!(snap.p50_ms <= snap.p95_ms);
        assert!(snap.p95_ms <= snap.p99_ms);
    }
}
