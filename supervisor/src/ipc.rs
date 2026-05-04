//! Control-plane IPC.
//!
//! The CLI tool (`mneme daemon ...`) connects to the supervisor over a
//! Unix domain socket (Unix) or named pipe (Windows) and exchanges
//! length-prefixed JSON messages. The supervisor listens forever; each
//! incoming connection is handled on its own task.
//!
//! Wire format (per message):
//!     `<u32 length BE>` `<JSON body>`

use crate::child::ChildStatus;
use crate::error::SupervisorError;
use crate::job_queue::JobQueueSnapshot;
use crate::manager::{ChildManager, ChildSnapshot};
use common::jobs::{Job, JobId, JobOutcome};
use common::query::{BlastItem, GodNode, RecallHit};
use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::tokio::Listener as _;
use interprocess::local_socket::traits::tokio::Stream as IpcStreamExt;
use interprocess::local_socket::ListenerOptions;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

/// Maximum size of a single framed IPC message (16 MiB). A connection
/// claiming a larger frame is treated as malformed and closed.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Per-read timeout on an IPC connection (NEW-016). A client that
/// connects but never writes a frame would otherwise hold a tokio
/// task forever; the timeout drops them so the daemon can't be
/// resource-starved by malicious or buggy clients.
const IPC_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum concurrent IPC connections (I-4 / I-5 / NEW-008). Beyond
/// this the listener still accepts but the per-connection task waits
/// on the semaphore before draining its frame.
///
/// AI-DNA pace: bumped from 64 to 256 (4×). Modern AI workflows fan out
/// 8-12 parallel agent calls, plus the CLI / MCP / vision-app baseline of
/// 5-10 in-flight calls and supervisor tooling polling /metrics, /health,
/// `mneme daemon status`. Under burst the legacy 64-cap saturated and the
/// per-connection tasks queued on the semaphore — visible to the AI as
/// IPC latency. 256 leaves the same connection-storm safety margin
/// (`Semaphore` still bounds the working set) while absorbing real
/// burst rates. Tunable at runtime via `MNEME_IPC_MAX_CONNS`.
///
/// See `feedback_mneme_ai_dna_pace.md` Principle B: "every queue depth
/// tuned for AI-rate, not human-rate".
const MAX_CONCURRENT_CONNECTIONS_DEFAULT: usize = 256;

fn max_concurrent_connections() -> usize {
    std::env::var("MNEME_IPC_MAX_CONNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_CONCURRENT_CONNECTIONS_DEFAULT)
}

/// Commands accepted by the IPC server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ControlCommand {
    /// Liveness probe.
    Ping,
    /// Return per-child status snapshots.
    Status,
    /// Return the last `n` log entries (optionally filtered by child).
    Logs {
        /// Optional child filter.
        child: Option<String>,
        /// Number of entries to return.
        n: usize,
    },
    /// Restart a single child.
    Restart {
        /// Child name to restart.
        child: String,
    },
    /// Restart every child (rolling).
    RestartAll,
    /// Stop the supervisor (graceful shutdown).
    Stop,
    /// Update a heartbeat for a specific child (called by workers).
    Heartbeat {
        /// Child name reporting the heartbeat.
        child: String,
    },
    /// Route a job payload to the worker pool whose names share `pool`
    /// as a prefix (e.g. `"parser-worker-"`, `"scanner-worker-"`, or
    /// `"brain-worker"`). The daemon writes `payload` as a JSON line to
    /// the selected worker's stdin. Used by `mneme build` and the
    /// scanner/brain orchestrators so the CLI does not have to run parse
    /// / scan / embed work inline.
    Dispatch {
        /// Child-name prefix identifying the pool.
        pool: String,
        /// JSON payload handed verbatim to the worker.
        payload: String,
    },
    /// (v0.3) Queue a structured `Job`. Supervisor owns routing, retry
    /// on worker crash, and back-pressure. Returns [`ControlResponse::Job
    /// Queued`] with a [`JobId`] the CLI can poll for completion.
    DispatchJob {
        /// The job to queue.
        job: Job,
    },
    /// (v0.3) Worker-side notification that a job finished. Payload is
    /// opaque — the CLI interprets it based on the original `Job` kind.
    WorkerCompleteJob {
        /// Job identifier minted by the supervisor at `DispatchJob` time.
        job_id: JobId,
        /// Outcome reported by the worker.
        outcome: JobOutcome,
    },
    /// (v0.3) Return the current job-queue snapshot (pending/in-flight
    /// counts + cumulative totals). Used by `mneme status --jobs` and
    /// the CLI wait loop.
    JobQueueStatus,
    /// (v0.3.1) Supervisor-mediated recall. Opens the project's
    /// `graph.db` shard read-only and runs the same FTS5/LIKE query the
    /// CLI's direct-DB fallback runs today. Benefit: the supervisor
    /// pools read connections and caches the prepared statement across
    /// requests.
    Recall {
        /// Project root whose shard to query.
        project: std::path::PathBuf,
        /// Free-form query string.
        query: String,
        /// Max number of hits.
        limit: usize,
        /// Optional filter (unused today; kept for wire-compat).
        #[serde(rename = "filter_type")]
        filter_type: Option<String>,
    },
    /// (v0.3.1) Supervisor-mediated blast-radius query.
    Blast {
        /// Project root whose shard to query.
        project: std::path::PathBuf,
        /// File path or fully-qualified function name.
        target: String,
        /// Max traversal depth.
        depth: usize,
    },
    /// (v0.3.1) Supervisor-mediated top-N most-connected concept query.
    GodNodes {
        /// Project root whose shard to query.
        project: std::path::PathBuf,
        /// How many nodes to return.
        n: usize,
    },
    /// (v0.3.1, NEW-019) Trigger a graphify-corpus run for `project_id`.
    /// The supervisor pushes an `Job::Ingest` per discovered .md/.txt
    /// file onto the shared job queue; the md-ingest pool drains it.
    /// Returns immediately with the count of jobs queued — the MCP tool
    /// can poll `JobQueueStatus` to follow progress.
    GraphifyCorpus {
        /// Project root whose corpus to ingest.
        project_id: std::path::PathBuf,
    },
    /// (v0.3.1, NEW-019) Snapshot the supervisor's view of `project_id`
    /// (or the entire daemon when `project_id` is `None`). Today this is
    /// a thin wrapper that returns `(child_snapshots, job_queue_snapshot)`
    /// so MCP clients can drive a UI without two round-trips. The
    /// `scope` parameter is reserved for v0.4 partial-snapshot fan-out
    /// (e.g. only worker telemetry vs only queue stats).
    Snapshot {
        /// Optional project root filter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<std::path::PathBuf>,
        /// Coarse scope hint (`"all"`, `"workers"`, `"queue"`).
        #[serde(default)]
        scope: String,
    },
    /// (v0.3.1, NEW-019) Rebuild every supervised worker for the named
    /// project. `force=true` kills running workers immediately; `force
    /// =false` (the default) waits for the current job to drain.
    Rebuild {
        /// Project root scope.
        project_id: std::path::PathBuf,
        /// Whether to forcefully kill running workers.
        #[serde(default)]
        force: bool,
    },

    // ----- Phase A B2.2: CLI-mirror stubs -----
    //
    // The CLI sends these verbs via `IpcRequest::{Audit, Drift, ...}`
    // (cli/src/ipc.rs). Before B2.2 the supervisor's `serde` rejected
    // each as an "unknown variant", forcing every call to land in the
    // CLI's malformed-command WARN path before falling back to the
    // direct subprocess / direct-DB. The variants below acknowledge
    // the verbs at the wire level so `serde_json::from_slice` succeeds;
    // the matching dispatch arms below return a controlled
    // `ControlResponse::Error` so the CLI's existing fallback logic
    // (which is the source of truth for these ops today) keeps working
    // without the malformed-command spam.
    //
    // Field names below MUST match `cli::ipc::IpcRequest::*` exactly
    // — they share the wire via `tag = "command", rename_all =
    // "snake_case"`. Do NOT add behaviour here; the supervisor-side
    // implementation lands later in the roadmap.
    /// Run all configured scanners. The supervisor enumerates scannable
    /// files under `project` (or CWD when `project` is None) and submits
    /// one `Job::Scan` per file to the scanner-worker pool — see B11.7
    /// in the v0.3.2 D:\Mneme Dome cycle. The CLI's `mneme audit`
    /// direct-subprocess path remains as a fallback when the supervisor
    /// is unreachable (or returns Error).
    Audit {
        /// Scanner scope: `full` (every scannable file) or `diff`. Today
        /// the supervisor only implements `full` — `diff` falls back to
        /// the standalone subprocess path.
        scope: String,
        /// B11.7 (v0.3.2): the project root the supervisor should
        /// enumerate. `#[serde(default)]` keeps wire-compat with older
        /// CLIs that send only `scope`; in that case the supervisor
        /// returns an Error and the CLI takes the fallback path.
        #[serde(default)]
        project: Option<std::path::PathBuf>,
    },
    /// (B2.2) Current drift findings. CLI mirror — supervisor stub.
    Drift {
        /// Optional severity filter.
        #[serde(default)]
        severity: Option<String>,
    },
    /// (B2.2) Step ledger op. CLI mirror — supervisor stub.
    Step {
        /// status | show | verify | complete | resume | plan-from
        op: String,
        /// Optional argument (step id, markdown path, …).
        #[serde(default)]
        arg: Option<String>,
    },
    /// (B2.2) Hook entry point: UserPromptSubmit. CLI mirror —
    /// supervisor stub.
    Inject {
        /// User's prompt text.
        prompt: String,
        /// Session ID.
        session_id: String,
        /// Working directory at the time the hook fired.
        cwd: std::path::PathBuf,
    },
    /// (B2.2) Hook entry point: SessionStart. CLI mirror — supervisor
    /// stub.
    SessionPrime {
        /// Active project path.
        project: std::path::PathBuf,
        /// Session ID assigned by the host.
        session_id: String,
    },
    /// (B2.2) Hook entry point: PreToolUse. CLI mirror — supervisor
    /// stub.
    PreTool {
        /// Tool name about to be invoked.
        tool: String,
        /// JSON-encoded tool params.
        params: String,
        /// Session ID.
        session_id: String,
    },
    /// (B2.2) Hook entry point: PostToolUse. CLI mirror — supervisor
    /// stub.
    PostTool {
        /// Tool name that ran.
        tool: String,
        /// Path to the file containing the tool's serialized result.
        result_file: std::path::PathBuf,
        /// Session ID.
        session_id: String,
    },
    /// (B2.2) Hook entry point: Stop (between turns). CLI mirror —
    /// supervisor stub.
    TurnEnd {
        /// Session ID.
        session_id: String,
    },
    /// (B2.2) Hook entry point: SessionEnd. CLI mirror — supervisor
    /// stub.
    SessionEnd {
        /// Session ID.
        session_id: String,
    },
    /// (v0.3.2, SD-1 fix) Append a turn row to `history.db::turns`
    /// through the supervisor's shared per-shard writer task. Used by
    /// hooks (`mneme inject`, `mneme turn-end`) so the per-shard
    /// single-writer invariant is preserved — every hook write now
    /// serializes through ONE `Store` instance owned by the supervisor
    /// instead of every hook process opening its own writer.
    WriteTurn {
        /// Project root the hook resolved its CWD to.
        project: std::path::PathBuf,
        /// Session id assigned by the host.
        session_id: String,
        /// Role label (`user` | `assistant` | `session_end` | …).
        role: String,
        /// Raw turn content.
        content: String,
    },
    /// (v0.3.2, SD-1 fix) Append a ledger row to
    /// `tasks.db::ledger_entries` through the supervisor's shared
    /// per-shard writer task.
    WriteLedgerEntry {
        /// Project root the hook resolved its CWD to.
        project: std::path::PathBuf,
        /// Session id assigned by the host.
        session_id: String,
        /// Ledger entry kind (`decision` | `note` | …).
        kind: String,
        /// Short summary text.
        summary: String,
        /// Optional rationale string (NULL when omitted).
        #[serde(default)]
        rationale: Option<String>,
    },
    /// (v0.3.2, SD-1 fix) Append a tool-call row to
    /// `tool_cache.db::tool_calls` through the supervisor's shared
    /// per-shard writer task.
    WriteToolCall {
        /// Project root the hook resolved its CWD to.
        project: std::path::PathBuf,
        /// Session id assigned by the host.
        session_id: String,
        /// Tool name as Claude reports it.
        tool: String,
        /// Verbatim JSON-encoded params.
        params_json: String,
        /// Verbatim JSON-encoded tool result.
        result_json: String,
    },
    /// (v0.3.2, SD-1 fix) Append a file-event row to
    /// `livestate.db::file_events` through the supervisor's shared
    /// per-shard writer task.
    WriteFileEvent {
        /// Project root the hook resolved its CWD to.
        project: std::path::PathBuf,
        /// File path the event references.
        file_path: String,
        /// Event kind (`pre_write` | `post_write` | …).
        event_type: String,
        /// Actor label (the tool name, typically).
        actor: String,
    },
}

/// Responses sent back over the same connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Generic ack.
    Pong,
    /// Status reply.
    Status {
        /// Per-child snapshots.
        children: Vec<ChildSnapshot>,
    },
    /// Logs reply.
    Logs {
        /// Log entries (oldest-first).
        entries: Vec<crate::log_ring::LogEntry>,
    },
    /// Successful dispatch — carries the worker name the job was routed to.
    Dispatched {
        /// Worker that accepted the job.
        worker: String,
    },
    /// (v0.3) `DispatchJob` accepted — returns the opaque `JobId`.
    JobQueued {
        /// Supervisor-assigned job id.
        job_id: JobId,
    },
    /// (v0.3) Snapshot of the job queue.
    JobQueue {
        /// Queue stats.
        snapshot: JobQueueSnapshot,
    },
    /// Generic OK acknowledgement.
    Ok {
        /// Optional human-readable message.
        message: Option<String>,
    },
    /// (v0.3.1) Supervisor-mediated recall results. Shape matches what
    /// the CLI's direct-DB fallback would render so downstream printing
    /// is source-agnostic.
    RecallResults {
        /// Hits ranked by FTS5 (or LIKE fallback).
        hits: Vec<RecallHit>,
    },
    /// (v0.3.1) Supervisor-mediated blast-radius results.
    BlastResults {
        /// Dependents in BFS order.
        impacted: Vec<BlastItem>,
    },
    /// (v0.3.1) Supervisor-mediated top-N concept results.
    GodNodesResults {
        /// Nodes sorted by (degree desc, qualified_name asc).
        nodes: Vec<GodNode>,
    },
    /// (v0.3.1, NEW-019) Result of `GraphifyCorpus` — count of jobs
    /// queued. Caller polls `JobQueueStatus` for progress.
    GraphifyCorpusQueued {
        /// Number of `Job::Ingest` items queued onto the shared queue.
        queued: usize,
        /// Project root the supervisor enumerated.
        project: std::path::PathBuf,
    },
    /// (v0.3.1, NEW-019) Combined snapshot of workers + queue.
    SnapshotCombined {
        /// Per-child snapshots (filtered by `project_id` when v0.4
        /// per-project supervision lands; today this is the full set).
        children: Vec<ChildSnapshot>,
        /// Job-queue stats.
        jobs: JobQueueSnapshot,
        /// Echoed scope so the caller knows what it asked for.
        scope: String,
    },
    /// (v0.3.1, NEW-019) Result of `Rebuild`. Lists the worker names
    /// that received a kill signal. The supervisor's restart loop will
    /// respawn each one through the normal monitor path.
    RebuildAcked {
        /// Worker names killed.
        workers: Vec<String>,
        /// Whether `force` was honoured.
        force: bool,
    },
    /// Error reply.
    Error {
        /// Error message.
        message: String,
    },
    /// Generic "this RPC isn't supported by this build" reply.
    /// Used to keep older / partially-wired callers from misinterpreting
    /// an `Error` (which signals a runtime failure) as a permanent
    /// capability gap. NEW-019 wires up GraphifyCorpus/Snapshot/Rebuild;
    /// this variant exists so future verbs can return Unsupported until
    /// the plumbing arrives.
    BadRequest {
        /// Diagnostic message (e.g. "read timeout" or "unknown verb").
        message: String,
    },
}

/// IPC server. Listens on a Unix socket / Windows named pipe.
pub struct IpcServer {
    manager: Arc<ChildManager>,
    socket_path: PathBuf,
}

impl IpcServer {
    /// Construct a new IPC server.
    pub fn new(manager: Arc<ChildManager>, socket_path: PathBuf) -> Self {
        Self {
            manager,
            socket_path,
        }
    }

    /// Run the listener until `shutdown.notified()`.
    pub async fn serve(self, shutdown: Arc<Notify>) {
        // Best-effort cleanup of a stale socket file from a previous run.
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.socket_path);
        }

        let listener = match build_listener(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                error!(socket = %self.socket_path.display(), error = %e, "ipc listener bind failed");
                return;
            }
        };
        info!(socket = %self.socket_path.display(), "ipc server listening");

        // I-4 / I-5: track every per-connection task in a JoinSet so we
        // can drain on shutdown instead of leaking detached `tokio::spawn`
        // futures. Before this fix every IPC connection became a
        // permanently-detached task whose stack lived until the OS
        // closed its pipe, which on Windows is unreliable.
        let mut conns: JoinSet<()> = JoinSet::new();

        // I-4 / I-5 / NEW-008: semaphore caps concurrent live IPC
        // connections at `max_concurrent_connections()`. The listener still
        // accepts; per-connection tasks acquire a permit before doing
        // any work and release it on drop. This bounds the supervisor's
        // working-set under a runaway client that opens connections in
        // a tight loop without ever closing them.
        //
        // AI-DNA pace: cap defaults to MAX_CONCURRENT_CONNECTIONS_DEFAULT
        // (256, 4× the legacy 64) but is configurable via
        // `MNEME_IPC_MAX_CONNS` so future tuning doesn't require recompile.
        let conn_cap = max_concurrent_connections();
        let conn_limiter = Arc::new(Semaphore::new(conn_cap));

        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    info!("ipc server shutting down; draining connections");
                    break;
                }
                // Reap finished connection tasks eagerly so the JoinSet
                // doesn't grow unbounded under steady-state load.
                Some(res) = conns.join_next(), if !conns.is_empty() => {
                    if let Err(e) = res {
                        if !e.is_cancelled() {
                            warn!(error = %e, "ipc connection task join error");
                        }
                    }
                }
                accept = listener.accept() => {
                    match accept {
                        Ok(stream) => {
                            let manager = self.manager.clone();
                            let sd = shutdown.clone();
                            let limiter = conn_limiter.clone();
                            conns.spawn(async move {
                                // Acquire a permit before doing ANY work
                                // on the new connection. The permit lives
                                // for the lifetime of the spawned task;
                                // dropping it (on task return or panic)
                                // returns it to the pool.
                                //
                                // BUG-A4-015 (2026-05-04): tokio dep
                                // floor for this code path. We rely on
                                // `tokio::sync::Semaphore::acquire_owned`
                                // being **FIFO-fair**, which it is from
                                // tokio 1.30 onward (LIFO before that).
                                // The supervisor's Cargo.toml pins
                                // `tokio = "1.40"`, satisfying the
                                // floor; if anyone backports to a
                                // tokio < 1.30 the fairness guarantee
                                // disappears and IPC clients can starve
                                // under burst. DO NOT relax the version
                                // pin without auditing this acquire.
                                let _permit = match limiter.acquire_owned().await {
                                    Ok(p) => p,
                                    Err(_) => {
                                        // Semaphore closed: shutdown
                                        // raced us — drop the connection.
                                        return;
                                    }
                                };
                                if let Err(e) = handle_conn(stream, manager, sd).await {
                                    warn!(error = %e, "ipc connection closed with error");
                                }
                            });
                        }
                        Err(e) => {
                            error!(error = %e, "ipc accept failed");
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        }

        // Graceful drain: abort outstanding connections so we don't
        // leak readers waiting on dead pipes, then await them so
        // tokio's JoinSet doesn't drop them mid-poll.
        conns.shutdown().await;
    }
}

#[cfg(unix)]
fn build_listener(path: &PathBuf) -> Result<Listener, SupervisorError> {
    let name = path
        .as_path()
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| SupervisorError::Ipc(format!("name conversion failed: {e}")))?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .map_err(|e| SupervisorError::Ipc(format!("listener create failed: {e}")))?;
    Ok(listener)
}

#[cfg(windows)]
fn build_listener(path: &Path) -> Result<Listener, SupervisorError> {
    let pipe_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("mneme-supervisor")
        .to_string();
    let name = pipe_name
        .as_str()
        .to_ns_name::<GenericNamespaced>()
        .map_err(|e| SupervisorError::Ipc(format!("name conversion failed: {e}")))?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .map_err(|e| SupervisorError::Ipc(format!("listener create failed: {e}")))?;
    Ok(listener)
}

async fn handle_conn(
    mut stream: Stream,
    manager: Arc<ChildManager>,
    shutdown: Arc<Notify>,
) -> Result<(), SupervisorError> {
    // I-4 / I-5: reuse a single body buffer per connection. A long-lived
    // IPC client (`mneme daemon status` running in a tight loop) would
    // otherwise allocate a fresh Vec<u8> per request — measurable in the
    // working-set delta over a 100-poll burst.
    //
    // AI-DNA pace: preallocate 64 KiB (covers the typical command body —
    // recall queries, blast result lists, status payloads). Larger frames
    // (up to MAX_FRAME_BYTES = 16 MiB) still grow the buffer once on
    // first use; the preallocation just removes the small-message hot-
    // path realloc storm visible when AI fires hundreds of MCP calls
    // per second. See `feedback_mneme_ai_dna_pace.md` Principle B.
    let mut body: Vec<u8> = Vec::with_capacity(64 * 1024);
    loop {
        // Read a length prefix (u32 BE), bounded by IPC_READ_TIMEOUT
        // (NEW-016) so a client that opens the connection and stops
        // writing can't park a tokio task forever.
        let mut len_buf = [0u8; 4];
        let read_result =
            tokio::time::timeout(IPC_READ_TIMEOUT, stream.read_exact(&mut len_buf)).await;
        match read_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Ok(Err(e)) => return Err(SupervisorError::Io(e)),
            Err(_) => {
                debug!("ipc connection idle past timeout; closing");
                return Ok(());
            }
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(SupervisorError::Ipc(format!("frame too large: {len}")));
        }

        body.clear();
        body.resize(len, 0);
        match tokio::time::timeout(IPC_READ_TIMEOUT, stream.read_exact(&mut body)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(SupervisorError::Io(e)),
            Err(_) => {
                debug!("ipc connection stalled mid-frame; closing");
                return Ok(());
            }
        };

        let cmd: ControlCommand = match serde_json::from_slice(&body) {
            Ok(c) => c,
            Err(e) => {
                let resp = ControlResponse::Error {
                    message: format!("malformed command: {e}"),
                };
                write_response(&mut stream, &resp).await?;
                continue;
            }
        };
        debug!(?cmd, "ipc command received");

        let resp = dispatch(cmd, manager.clone(), shutdown.clone()).await;
        write_response(&mut stream, &resp).await?;
    }
}

async fn dispatch(
    cmd: ControlCommand,
    manager: Arc<ChildManager>,
    shutdown: Arc<Notify>,
) -> ControlResponse {
    match cmd {
        ControlCommand::Ping => ControlResponse::Pong,
        ControlCommand::Status => ControlResponse::Status {
            children: manager.snapshot().await,
        },
        ControlCommand::Logs { child, n } => {
            let entries = manager.log_ring().tail(child.as_deref(), n);
            // B-005 fix: if the in-memory ring is empty (long-lived
            // supervisor whose 10k-entry buffer rolled over, or a fresh
            // CLI process that hasn't picked up any worker output yet)
            // fall back to the rotated file appender at
            // `~/.mneme/logs/supervisor.log`. That gives `mneme daemon
            // logs` a stable answer instead of an empty array. The file
            // tail is best-effort — missing dir / permission denied
            // returns an empty Vec, never errors.
            let entries = if entries.is_empty() && child.is_none() {
                let lines = crate::tail_supervisor_log(n);
                lines
                    .into_iter()
                    .map(|line| crate::log_ring::LogEntry {
                        timestamp: chrono::Utc::now(),
                        child: "supervisor".to_string(),
                        level: crate::log_ring::LogLevel::Info,
                        message: line,
                        fields: None,
                    })
                    .collect()
            } else {
                entries
            };
            ControlResponse::Logs { entries }
        }
        ControlCommand::Restart { child } => {
            let names = manager.child_names().await;
            if !names.iter().any(|n| n == &child) {
                return ControlResponse::Error {
                    message: format!("unknown child: {child}"),
                };
            }
            if let Err(e) = manager.kill_child(&child).await {
                return ControlResponse::Error {
                    message: format!("kill failed: {e}"),
                };
            }
            ControlResponse::Ok {
                message: Some(format!("child '{child}' kill signalled; restart pending")),
            }
        }
        ControlCommand::RestartAll => {
            let names = manager.child_names().await;
            for n in names {
                let _ = manager.kill_child(&n).await;
            }
            ControlResponse::Ok {
                message: Some("all children kill signalled".into()),
            }
        }
        ControlCommand::Stop => {
            shutdown.notify_waiters();
            ControlResponse::Ok {
                message: Some("shutdown signalled".into()),
            }
        }
        ControlCommand::Heartbeat { child } => {
            let names = manager.child_names().await;
            if !names.iter().any(|n| n == &child) {
                return ControlResponse::Error {
                    message: format!("unknown child: {child}"),
                };
            }
            let _ = ChildStatus::Running; // keep the import alive
            manager.record_heartbeat(&child).await;
            ControlResponse::Ok { message: None }
        }
        ControlCommand::Dispatch { pool, payload } => {
            match manager.dispatch_to_pool(&pool, &payload).await {
                Ok(worker) => ControlResponse::Dispatched { worker },
                Err(e) => ControlResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        ControlCommand::DispatchJob { job } => {
            let Some(queue) = manager.job_queue().await else {
                return ControlResponse::Error {
                    message: "supervisor job queue is not attached".into(),
                };
            };
            match queue.submit(job, None) {
                Ok(job_id) => ControlResponse::JobQueued { job_id },
                Err(e) => ControlResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        ControlCommand::WorkerCompleteJob { job_id, outcome } => {
            let Some(queue) = manager.job_queue().await else {
                return ControlResponse::Error {
                    message: "supervisor job queue is not attached".into(),
                };
            };
            // Pull the telemetry we need BEFORE `complete` moves the
            // outcome into the waker channel — the worker + manager
            // update must happen even if the CLI is no longer listening.
            let duration_ms = outcome.duration_ms();
            let status = outcome.status_str();
            let worker = queue.complete(job_id, outcome);
            if let Some(name) = worker {
                manager
                    .record_job_completion(&name, job_id.0, status, duration_ms)
                    .await;
                debug!(
                    %job_id,
                    worker = %name,
                    status,
                    duration_ms,
                    "worker_complete_job recorded"
                );
            }
            ControlResponse::Ok { message: None }
        }
        ControlCommand::JobQueueStatus => {
            let Some(queue) = manager.job_queue().await else {
                return ControlResponse::Error {
                    message: "supervisor job queue is not attached".into(),
                };
            };
            ControlResponse::JobQueue {
                snapshot: queue.snapshot(),
            }
        }
        ControlCommand::Recall {
            project,
            query,
            limit,
            filter_type: _,
        } => match query_runner::run_recall(&project, &query, limit) {
            Ok(hits) => ControlResponse::RecallResults { hits },
            Err(e) => ControlResponse::Error { message: e },
        },
        ControlCommand::Blast {
            project,
            target,
            depth,
        } => match query_runner::run_blast(&project, &target, depth) {
            Ok(impacted) => ControlResponse::BlastResults { impacted },
            Err(e) => ControlResponse::Error { message: e },
        },
        ControlCommand::GodNodes { project, n } => match query_runner::run_godnodes(&project, n) {
            Ok(nodes) => ControlResponse::GodNodesResults { nodes },
            Err(e) => ControlResponse::Error { message: e },
        },
        // NEW-019: enumerate corpus markdown/text files under `project_id`
        // and queue an `Job::Ingest` per file. Returns the count without
        // waiting for completion — Bucket D (MCP) polls `JobQueueStatus`
        // to render progress.
        ControlCommand::GraphifyCorpus { project_id } => {
            let queue = match manager.job_queue().await {
                Some(q) => q,
                None => {
                    return ControlResponse::Error {
                        message: "supervisor job queue is not attached".into(),
                    };
                }
            };
            match enqueue_corpus(&project_id, &queue).await {
                Ok(queued) => ControlResponse::GraphifyCorpusQueued {
                    queued,
                    project: project_id,
                },
                Err(e) => ControlResponse::Error { message: e },
            }
        }
        // NEW-019: combined supervisor + queue snapshot. The `scope`
        // parameter is echoed back today; the actual filter is uniform
        // (`"all"` shape) so a misuse never silently drops data.
        ControlCommand::Snapshot {
            project_id: _,
            scope,
        } => {
            let children = manager.snapshot().await;
            let jobs = match manager.job_queue().await {
                Some(q) => q.snapshot(),
                None => crate::job_queue::JobQueueSnapshot {
                    pending: 0,
                    in_flight: 0,
                    completed: 0,
                    failed: 0,
                    requeued: 0,
                },
            };
            ControlResponse::SnapshotCombined {
                children,
                jobs,
                scope: if scope.is_empty() {
                    "all".into()
                } else {
                    scope
                },
            }
        }
        // NEW-019: kill every running worker the supervisor knows about.
        // The restart loop respawns each one — this is effectively a
        // bounce-all. `force` today is a hint (we always kill); the
        // parameter is preserved on the wire so v0.4 can implement a
        // "drain then bounce" path without breaking older callers.
        ControlCommand::Rebuild {
            project_id: _,
            force,
        } => {
            let names = manager.child_names().await;
            let mut bounced = Vec::with_capacity(names.len());
            for n in &names {
                match manager.kill_child(n).await {
                    Ok(()) => bounced.push(n.clone()),
                    Err(e) => {
                        warn!(child = %n, error = %e, "rebuild: kill_child failed");
                    }
                }
            }
            ControlResponse::RebuildAcked {
                workers: bounced,
                force,
            }
        }

        // ----- Phase A B2.2: CLI-mirror stub arms -----
        //
        // Each of these returns a controlled `ControlResponse::Error`
        // so the wire is no longer rejected at the serde layer (which
        // was emitting the "malformed command: unknown variant ..."
        // noise on every `mneme audit` / `mneme drift` / hook fire).
        // The CLI's existing fallback paths (direct subprocess for
        // audit, direct-DB for drift/step, empty bundle for the
        // hooks) remain authoritative — supervisor-side
        // implementation is not part of B2.2.
        //
        // Bindings prefixed with `_` to silence unused warnings while
        // preserving wire-shape documentation.
        // B11.7 (v0.3.2): supervisor-mediated audit fan-out. Enumerate
        // scannable files under `project` and submit one `Job::Scan`
        // per file. The router task drains the queue and dispatches
        // round-robin to the scanner-worker pool. Each worker
        // persists findings DIRECTLY to the per-project findings.db
        // via the shard_root threaded through `encode_for_worker`.
        // Returns the count of jobs queued so the CLI can poll
        // `JobQueueStatus` for completion.
        //
        // Falls back to `ControlResponse::Error` when:
        //   - `project` is None (older CLI that didn't set the field —
        //     the CLI's standalone subprocess path is authoritative)
        //   - scope is not "full" (today only "full" is implemented;
        //     "diff" falls back to the standalone subprocess for the
        //     mtime-based file filter)
        //   - the job queue isn't attached
        //   - file enumeration fails (e.g. project path doesn't exist)
        ControlCommand::Audit { scope, project } => {
            let Some(project_path) = project else {
                return ControlResponse::Error {
                    message: "audit dispatch requires `project` field; \
                              older CLIs without this field fall back to standalone subprocess"
                        .into(),
                };
            };
            if scope != "full" {
                return ControlResponse::Error {
                    message: format!(
                        "supervisor audit dispatch only supports scope=full today; \
                         got {scope:?} — falling back to standalone subprocess"
                    ),
                };
            }
            let queue = match manager.job_queue().await {
                Some(q) => q,
                None => {
                    return ControlResponse::Error {
                        message: "supervisor job queue is not attached".into(),
                    };
                }
            };
            match enqueue_audit(&project_path, &queue).await {
                Ok(outcome) => {
                    let AuditEnumOutcome {
                        queued,
                        scanned,
                        truncated,
                        dropped,
                    } = outcome;
                    info!(
                        project = %project_path.display(),
                        queued,
                        scanned,
                        truncated,
                        dropped,
                        "audit dispatched: {queued} Job::Scan items queued for scanner-worker pool"
                    );
                    // BUG-A4-004 fix (2026-05-04): surface truncation
                    // and queue-full drops to the CLI via the response
                    // message so the operator can re-run with a
                    // narrower scope or wait for the queue to drain.
                    let mut msg = format!(
                        "audit dispatched: {queued} files queued ({scanned} scanned)"
                    );
                    if truncated {
                        msg.push_str(&format!(
                            "; WARNING: enumeration truncated at scan cap -- partial audit, re-run with a narrower path"
                        ));
                    }
                    if dropped > 0 {
                        msg.push_str(&format!(
                            "; WARNING: {dropped} files dropped because the job queue is full"
                        ));
                    }
                    ControlResponse::Ok { message: Some(msg) }
                }
                Err(e) => ControlResponse::Error { message: e },
            }
        }
        ControlCommand::Drift {
            severity: _severity,
        } => ControlResponse::Error {
            message: "drift not yet implemented in supervisor; CLI fallback is authoritative"
                .into(),
        },
        ControlCommand::Step { op: _op, arg: _arg } => ControlResponse::Error {
            message: "step not yet implemented in supervisor; CLI fallback is authoritative".into(),
        },
        ControlCommand::Inject {
            prompt: _prompt,
            session_id: _session_id,
            cwd: _cwd,
        } => ControlResponse::Error {
            message: "inject not yet implemented in supervisor; CLI emits empty bundle".into(),
        },
        ControlCommand::SessionPrime {
            project: _project,
            session_id: _session_id,
        } => ControlResponse::Error {
            message: "session_prime not yet implemented in supervisor; CLI emits empty bundle"
                .into(),
        },
        ControlCommand::PreTool {
            tool: _tool,
            params: _params,
            session_id: _session_id,
        } => ControlResponse::Error {
            message: "pre_tool not yet implemented in supervisor; CLI emits skip:false".into(),
        },
        ControlCommand::PostTool {
            tool: _tool,
            result_file: _result_file,
            session_id: _session_id,
        } => ControlResponse::Error {
            message: "post_tool not yet implemented in supervisor; fire-and-forget".into(),
        },
        ControlCommand::TurnEnd {
            session_id: _session_id,
        } => ControlResponse::Error {
            message: "turn_end not yet implemented in supervisor; fire-and-forget".into(),
        },
        ControlCommand::SessionEnd {
            session_id: _session_id,
        } => ControlResponse::Error {
            message: "session_end not yet implemented in supervisor; fire-and-forget".into(),
        },
        // SD-1 fix (v0.3.2): hook writes routed through the supervisor's
        // shared per-shard writer task. The single-writer-per-shard
        // invariant in store/ is honoured because every hook now talks
        // to ONE `Store` instance (the one held by `hook_store()`),
        // instead of every short-lived hook process opening its own
        // writer alongside the supervisor's. Burst-load `SQLITE_BUSY`
        // retries and journal contention cannot happen because every
        // write serializes through the same in-process writer task.
        ControlCommand::WriteTurn {
            project,
            session_id,
            role,
            content,
        } => match hook_store_write_turn(&project, &session_id, &role, &content).await {
            Ok(()) => ControlResponse::Ok { message: None },
            Err(e) => ControlResponse::Error { message: e },
        },
        ControlCommand::WriteLedgerEntry {
            project,
            session_id,
            kind,
            summary,
            rationale,
        } => match hook_store_write_ledger(
            &project,
            &session_id,
            &kind,
            &summary,
            rationale.as_deref(),
        )
        .await
        {
            Ok(()) => ControlResponse::Ok { message: None },
            Err(e) => ControlResponse::Error { message: e },
        },
        ControlCommand::WriteToolCall {
            project,
            session_id,
            tool,
            params_json,
            result_json,
        } => match hook_store_write_tool_call(
            &project,
            &session_id,
            &tool,
            &params_json,
            &result_json,
        )
        .await
        {
            Ok(()) => ControlResponse::Ok { message: None },
            Err(e) => ControlResponse::Error { message: e },
        },
        ControlCommand::WriteFileEvent {
            project,
            file_path,
            event_type,
            actor,
        } => match hook_store_write_file_event(&project, &file_path, &event_type, &actor).await {
            Ok(()) => ControlResponse::Ok { message: None },
            Err(e) => ControlResponse::Error { message: e },
        },
    }
}

// ---------------------------------------------------------------------
// SD-1 fix (v0.3.2): shared per-shard writer for hook IPC handlers.
//
// The supervisor lazily constructs ONE `store::Store` instance and
// reuses it for every hook write across every connection. `Store`
// internally spawns one writer task per shard, so this gives us the
// invariant we want: at most one writer per shard, supervisor-owned,
// regardless of how many hook processes fire concurrently.
// ---------------------------------------------------------------------

/// Lazy-initialized shared `Store` used by the hook write handlers.
/// First access constructs it via the default `PathManager` root, every
/// subsequent access returns the same instance. Lives until process exit.
fn hook_store() -> &'static store::Store {
    static STORE: OnceLock<store::Store> = OnceLock::new();
    STORE.get_or_init(|| {
        let paths = common::paths::PathManager::default_root();
        store::Store::new(paths)
    })
}

/// Resolve a project path to its `ProjectId` and ensure the shard
/// exists (idempotent `build_or_migrate`). Mirrors what the CLI's
/// `HookCtx::resolve` does — kept in sync deliberately so the
/// supervisor path and the direct-DB fallback write to the same shard.
async fn hook_resolve_project(project: &Path) -> Result<common::ids::ProjectId, String> {
    let project_id = common::ids::ProjectId::from_path(project)
        .map_err(|e| format!("hash project path {}: {e}", project.display()))?;
    let project_name = project
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string();
    hook_store()
        .builder
        .build_or_migrate(&project_id, project, &project_name)
        .await
        .map_err(|e| format!("build_or_migrate: {e}"))?;
    Ok(project_id)
}

fn hook_inject_opts() -> store::inject::InjectOptions {
    store::inject::InjectOptions {
        idempotency_key: None,
        emit_event: false,
        audit: false,
        timeout_ms: Some(2_000),
    }
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

async fn hook_store_write_turn(
    project: &Path,
    session_id: &str,
    role: &str,
    content: &str,
) -> Result<(), String> {
    let project_id = hook_resolve_project(project).await?;
    let sql = "INSERT INTO turns(session_id, role, content, timestamp) \
               VALUES(?1, ?2, ?3, ?4)";
    let params = vec![
        serde_json::Value::String(session_id.to_string()),
        serde_json::Value::String(role.to_string()),
        serde_json::Value::String(content.to_string()),
        serde_json::Value::String(now_iso()),
    ];
    let resp = hook_store()
        .inject
        .insert(
            &project_id,
            common::layer::DbLayer::History,
            sql,
            params,
            hook_inject_opts(),
        )
        .await;
    if !resp.success {
        return Err(format!(
            "history.turns insert: {}",
            resp.error
                .map(|e| format!("{e:?}"))
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

async fn hook_store_write_ledger(
    project: &Path,
    session_id: &str,
    kind: &str,
    summary: &str,
    rationale: Option<&str>,
) -> Result<(), String> {
    let project_id = hook_resolve_project(project).await?;
    let entry_id = uuid::Uuid::new_v7(uuid::Timestamp::now(uuid::NoContext))
        .as_simple()
        .to_string();
    let timestamp_ms = chrono::Utc::now().timestamp_millis();
    let kind_payload = serde_json::json!({
        "kind": kind,
        "summary": summary,
        "rationale": rationale,
    })
    .to_string();
    let sql = "INSERT INTO ledger_entries\
               (id, session_id, timestamp, kind, summary, rationale, kind_payload) \
               VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)";
    let params = vec![
        serde_json::Value::String(entry_id),
        serde_json::Value::String(session_id.to_string()),
        serde_json::Value::Number(timestamp_ms.into()),
        serde_json::Value::String(kind.to_string()),
        serde_json::Value::String(summary.to_string()),
        rationale
            .map(|r| serde_json::Value::String(r.to_string()))
            .unwrap_or(serde_json::Value::Null),
        serde_json::Value::String(kind_payload),
    ];
    let resp = hook_store()
        .inject
        .insert(
            &project_id,
            common::layer::DbLayer::Tasks,
            sql,
            params,
            hook_inject_opts(),
        )
        .await;
    if !resp.success {
        return Err(format!(
            "tasks.ledger_entries insert: {}",
            resp.error
                .map(|e| format!("{e:?}"))
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

async fn hook_store_write_tool_call(
    project: &Path,
    session_id: &str,
    tool: &str,
    params_json: &str,
    result_json: &str,
) -> Result<(), String> {
    let project_id = hook_resolve_project(project).await?;
    let params_hash = blake3::hash(params_json.as_bytes()).to_hex().to_string();
    let sql = "INSERT OR REPLACE INTO tool_calls\
               (tool, params_hash, params, result, session_id, cached_at) \
               VALUES(?1, ?2, ?3, ?4, ?5, ?6)";
    let params = vec![
        serde_json::Value::String(tool.to_string()),
        serde_json::Value::String(params_hash),
        serde_json::Value::String(params_json.to_string()),
        serde_json::Value::String(result_json.to_string()),
        serde_json::Value::String(session_id.to_string()),
        serde_json::Value::String(now_iso()),
    ];
    let resp = hook_store()
        .inject
        .insert(
            &project_id,
            common::layer::DbLayer::ToolCache,
            sql,
            params,
            hook_inject_opts(),
        )
        .await;
    if !resp.success {
        return Err(format!(
            "tool_cache.tool_calls insert: {}",
            resp.error
                .map(|e| format!("{e:?}"))
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

async fn hook_store_write_file_event(
    project: &Path,
    file_path: &str,
    event_type: &str,
    actor: &str,
) -> Result<(), String> {
    let project_id = hook_resolve_project(project).await?;
    let sql = "INSERT INTO file_events(file_path, event_type, actor, happened_at) \
               VALUES(?1, ?2, ?3, ?4)";
    let params = vec![
        serde_json::Value::String(file_path.to_string()),
        serde_json::Value::String(event_type.to_string()),
        serde_json::Value::String(actor.to_string()),
        serde_json::Value::String(now_iso()),
    ];
    let resp = hook_store()
        .inject
        .insert(
            &project_id,
            common::layer::DbLayer::LiveState,
            sql,
            params,
            hook_inject_opts(),
        )
        .await;
    if !resp.success {
        return Err(format!(
            "livestate.file_events insert: {}",
            resp.error
                .map(|e| format!("{e:?}"))
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    Ok(())
}

/// Enumerate every `.md`, `.markdown`, or `.txt` file under `project_id`
/// (skipping `.git`, `node_modules`, `target`, and `.mneme`) and submit
/// an `Ingest` job per file. Returns the count of jobs queued.
///
/// Best-effort — files we can't read are skipped rather than failing
/// the whole RPC. We bound enumeration at 100k entries so a runaway
/// directory tree never wedges the dispatch task; beyond that we log
/// a warn and stop.
async fn enqueue_corpus(
    project_id: &Path,
    queue: &Arc<crate::job_queue::JobQueue>,
) -> Result<usize, String> {
    use common::jobs::Job;
    use walkdir::WalkDir;

    let root = match dunce::canonicalize(project_id) {
        Ok(p) => p,
        Err(_) => project_id.to_path_buf(),
    };

    // Resolve the project's shard root via the same hashing the rest of
    // the supervisor uses for read-side queries. Mirrors the path
    // resolution in `query_runner::resolve_graph_db` above.
    let shard_root = {
        use common::ids::ProjectId;
        use common::paths::PathManager;
        let id = match ProjectId::from_path(&root) {
            Ok(id) => id,
            Err(e) => {
                return Err(format!("cannot hash project path {}: {e}", root.display()));
            }
        };
        PathManager::default_root().project_root(&id)
    };

    let mut queued = 0usize;
    let mut scanned = 0usize;
    const SCAN_CAP: usize = 100_000;
    for entry in WalkDir::new(&root).into_iter().filter_entry(|e| {
        let n = e.file_name().to_string_lossy();
        !matches!(
            n.as_ref(),
            ".git" | "node_modules" | "target" | ".mneme" | "dist"
        )
    }) {
        scanned += 1;
        if scanned > SCAN_CAP {
            warn!(
                scanned,
                "graphify_corpus enumeration cap hit; stopping early"
            );
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "md" | "markdown" | "txt") {
            continue;
        }
        let job = Job::Ingest {
            md_file: entry.path().to_path_buf(),
            shard_root: shard_root.clone(),
        };
        match queue.submit(job, None) {
            Ok(_) => queued += 1,
            Err(e) => {
                debug!(error = %e, "graphify_corpus: queue submit dropped (queue full?)");
            }
        }
    }
    info!(
        project = %root.display(),
        queued,
        "graphify_corpus enumeration complete"
    );
    Ok(queued)
}

/// B11.7 (v0.3.2, D:\Mneme Dome cycle, 2026-05-02): enumerate every
/// scannable file under `project_id` and submit one `Job::Scan` per
/// file. Returns the count of jobs queued.
///
/// This is the supervisor-side audit fan-out. The router task (`run_router`
/// in lib.rs) drains the queue and dispatches to the `scanner-worker-*`
/// pool round-robin via `dispatch_to_pool`. Each worker processes its
/// share of files, with findings persisted DIRECTLY to the per-project
/// `findings.db` (via the shard_root field that `encode_for_worker`
/// threads into the StdinJob — see B11.7 patch).
///
/// Expected speedup on a high-end AWS instance (6 scanner-workers): audit phase 30 min →
/// ~5 min. The standalone subprocess path (audit::run_direct_subprocess)
/// remains as a fallback when the supervisor is unreachable.
///
/// File enumeration mirrors `mneme_scanners::main::run_orchestrator_mode`
/// — same hard-ignore list, same .mnemeignore/.gitignore handling, same
/// extension allowlist. The `scope` parameter is reserved (today only
/// `"full"` is supported via this path; "diff" still goes via the
/// standalone subprocess for consistency with the legacy mtime filter).
/// Result of an audit enumeration pass.
///
/// BUG-A4-004 fix (2026-05-04): the audit dispatch used to return only
/// `Ok(queued)` and silently swallowed both the "scan cap hit" case
/// (200 K traversed paths) and the "queue full" submit failure case.
/// Operators with monorepos saw "audit dispatched: 200000 files queued"
/// and trusted that drift findings were complete -- when in fact entire
/// subtrees and any file past the queue's `max_pending` cap had been
/// dropped. This struct lets the IPC handler surface those degraded-
/// mode signals back to the CLI.
pub(crate) struct AuditEnumOutcome {
    /// Files for which a Job::Scan was successfully enqueued.
    pub queued: usize,
    /// Total WalkDir entries traversed (includes ignored / non-file).
    pub scanned: usize,
    /// True if the SCAN_CAP fired and we stopped enumeration early.
    pub truncated: bool,
    /// BUG-A4-005: count of jobs the queue refused (queue full).
    pub dropped: usize,
}

async fn enqueue_audit(
    project_id: &Path,
    queue: &Arc<crate::job_queue::JobQueue>,
) -> Result<AuditEnumOutcome, String> {
    use common::jobs::Job;
    use walkdir::WalkDir;

    let root = match dunce::canonicalize(project_id) {
        Ok(p) => p,
        Err(_) => project_id.to_path_buf(),
    };

    // Resolve the project's shard root via the same hashing the rest of
    // the supervisor uses for read-side queries. Mirrors the path
    // resolution in `query_runner::resolve_graph_db` and
    // `enqueue_corpus`.
    let shard_root = {
        use common::ids::ProjectId;
        use common::paths::PathManager;
        let id = match ProjectId::from_path(&root) {
            Ok(id) => id,
            Err(e) => {
                return Err(format!("cannot hash project path {}: {e}", root.display()));
            }
        };
        PathManager::default_root().project_root(&id)
    };

    // Make sure the shard directory exists so the worker's
    // `FindingsWriter::open(shard_root.join("findings.db"))` succeeds
    // on first write. Cheap; idempotent.
    if let Err(e) = std::fs::create_dir_all(&shard_root) {
        return Err(format!(
            "cannot create shard root {}: {e}",
            shard_root.display()
        ));
    }

    let mut queued = 0usize;
    let mut scanned = 0usize;
    let mut dropped = 0usize;
    let mut truncated = false;
    const SCAN_CAP: usize = 200_000;
    let walker = WalkDir::new(&root)
        .into_iter()
        .filter_entry(|e| !is_hard_ignored_for_audit(e.path()));
    for entry in walker {
        scanned += 1;
        if scanned > SCAN_CAP {
            truncated = true;
            warn!(scanned, "audit enumeration cap hit; stopping early");
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if !is_scannable_extension_for_audit(entry.path()) {
            continue;
        }
        let job = Job::Scan {
            file_path: entry.path().to_path_buf(),
            ast_id: None,
            shard_root: shard_root.clone(),
        };
        match queue.submit(job, None) {
            Ok(_) => queued += 1,
            Err(e) => {
                // BUG-A4-005 fix: bumped from `debug!` to `warn!` and we
                // count the drops so the IPC handler can surface them.
                // A debug-level line is suppressed under the prod
                // MNEME_LOG=warn default, which let dropped scan jobs
                // silently disappear -- the operator only saw the final
                // "queued" tally and assumed the full set was scanned.
                dropped += 1;
                warn!(error = %e, "audit: queue submit dropped (queue full?)");
            }
        }
    }
    info!(
        project = %root.display(),
        queued,
        scanned,
        dropped,
        truncated,
        "audit enumeration complete"
    );
    Ok(AuditEnumOutcome {
        queued,
        scanned,
        truncated,
        dropped,
    })
}

/// B11.7: hard-coded ignore list for the supervisor's audit fan-out.
/// Mirrors `mneme_scanners::main::is_hard_ignored` so the supervisor-
/// dispatched and standalone-subprocess paths visit the same set of
/// files. Kept inline rather than depending on the scanners crate so
/// the supervisor stays decoupled.
fn is_hard_ignored_for_audit(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | ".hg"
            | ".svn"
            | "dist"
            | "dist-electron"
            | "release"
            | "out"
            | "build"
            | "bin"
            | "obj"
            | ".next"
            | ".nuxt"
            | ".vite"
            | ".parcel-cache"
            | ".turbo"
            | ".svelte-kit"
            | ".astro"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".tox"
            | "site-packages"
            | ".idea"
            | ".vscode"
            | ".vs"
            | ".DS_Store"
            | ".cache"
            | ".yarn"
            | ".pnpm-store"
            | ".mneme"
            | ".claude"
            | "graphify-out"
            | "coverage"
            | ".nyc_output"
            | ".gradle"
            | ".rush"
            | ".mneme-graphify"
            | ".datatree"
    )
}

/// B11.7: scannable-extension allowlist for the supervisor audit
/// fan-out. Mirrors `mneme_scanners::main::is_scannable_extension`.
fn is_scannable_extension_for_audit(path: &Path) -> bool {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(s) => s.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        "ts" | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "html"
            | "htm"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "rs"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "cxx"
            | "hpp"
            | "hh"
            | "java"
            | "kt"
            | "kts"
            | "scala"
            | "py"
            | "rb"
            | "lua"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "md"
            | "markdown"
            | "mdx"
            | "rst"
            | "txt"
            | "swift"
            | "dart"
            | "ex"
            | "exs"
            | "elm"
            | "vue"
            | "svelte"
            | "astro"
            | "sol"
            | "zig"
            | "cs"
            | "fs"
            | "fsx"
            | "vb"
            | "php"
    )
}

/// Read-side helpers for the three new supervisor-mediated queries.
///
/// Each one resolves the project's `graph.db` shard, fetches a cached
/// read-only connection via [`with_cached_conn`], and runs the same SQL
/// the CLI's direct-DB fallback runs today. The supervisor never writes
/// here — the per-shard writer-task invariant is preserved.
///
/// ## SD-4 / PERF-1: connection cache
///
/// Before SD-4 every IPC `Recall` / `Blast` / `GodNodes` call opened a
/// fresh `Connection` (≈300-500µs of fsync + page-cache cold-start
/// per request) and dropped it on return. The doc comment on
/// `ControlCommand::Recall` claimed the supervisor "pools read
/// connections and caches the prepared statement" — the cache part was
/// a lie until this fix.
///
/// We now keep one `Connection` per `graph.db` path in a process-wide
/// `OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>>`:
///
/// * Outer `Mutex<HashMap>` protects the path → connection map; held
///   only briefly during lookup-or-insert. Cheap.
/// * Inner `Arc<Mutex<Connection>>` lets callers acquire serial access
///   to a single connection without holding the outer map lock.
/// * Connections are opened with `SQLITE_OPEN_READ_ONLY |
///   SQLITE_OPEN_NO_MUTEX`. Because we have explicit mutexes around
///   each one, sqlite's internal mutex would just be redundant cost.
///
/// All failures are surfaced as `Err(String)` (not `SupervisorError`) so
/// the caller can forward the message verbatim in `ControlResponse::Error`
/// without losing detail.
mod query_runner {
    use super::{BlastItem, GodNode, Path, RecallHit};
    use common::ids::ProjectId;
    use common::paths::PathManager;
    use rusqlite::{Connection, OpenFlags};
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};

    /// Process-wide cache of read-only connections keyed by absolute
    /// `graph.db` path. Lazily initialised; lives for the life of the
    /// supervisor process. See module-level doc comment for the locking
    /// strategy.
    fn conn_cache() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>> {
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Test-only hook: reset the cache between tests so a freshly
    /// created tempfile DB isn't shadowed by a stale connection from a
    /// previous test in the same binary. Not exposed in production
    /// code paths.
    #[cfg(test)]
    pub(super) fn _reset_conn_cache_for_tests() {
        if let Ok(mut g) = conn_cache().lock() {
            g.clear();
        }
    }

    /// Test-only hook: returns true if the given path currently has a
    /// cached connection. Used by `cache_hit_reuses_connection` to
    /// distinguish a cache HIT (entry survives across calls) from a
    /// cache MISS (entry was newly inserted).
    #[cfg(test)]
    pub(super) fn _has_cached_conn(db: &Path) -> bool {
        let g = match conn_cache().lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        g.contains_key(db)
    }

    /// Test-only hook: returns the strong-count of the cached
    /// `Arc<Mutex<Connection>>` for `db`, if any. A reused cache entry
    /// keeps the SAME `Arc`, so identity (rather than just contents)
    /// is provable by checking `Arc::strong_count` doesn't reset
    /// between calls. Returns `None` if no entry is cached.
    #[cfg(test)]
    pub(super) fn _cached_conn_arc_ptr(db: &Path) -> Option<usize> {
        let g = conn_cache().lock().ok()?;
        g.get(db).map(|h| Arc::as_ptr(h) as usize)
    }

    /// Test-only hook: drive `with_cached_conn` directly. Lets tests
    /// hit the cache path without resolving a project ID through
    /// `PathManager::default_root`, which is host-dependent.
    #[cfg(test)]
    pub(super) fn _with_cached_conn_for_tests<R>(
        db: &Path,
        f: impl FnOnce(&Connection) -> Result<R, String>,
    ) -> Result<R, String> {
        with_cached_conn(db, f)
    }

    /// Acquire serial access to the cached connection for `db`,
    /// inserting a fresh one if none exists yet. Panics-on-poison are
    /// converted to `Err(String)` so the IPC handler can surface a
    /// clean error message rather than tearing down the supervisor.
    fn with_cached_conn<R>(
        db: &Path,
        f: impl FnOnce(&Connection) -> Result<R, String>,
    ) -> Result<R, String> {
        // Step 1: lookup-or-insert under the outer map lock.
        let conn_handle: Arc<Mutex<Connection>> = {
            let mut map = conn_cache()
                .lock()
                .map_err(|e| format!("conn cache poisoned: {e}"))?;
            // `entry().or_insert_with` would force eager construction
            // and the constructor here is fallible (open_with_flags),
            // so we open-and-cache manually.
            if let Some(h) = map.get(db) {
                h.clone()
            } else {
                let conn = Connection::open_with_flags(
                    db,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|e| format!("open {}: {e}", db.display()))?;
                let h = Arc::new(Mutex::new(conn));
                map.insert(db.to_path_buf(), h.clone());
                h
            }
        };

        // Step 2: acquire the per-connection lock outside the map lock.
        let guard = conn_handle
            .lock()
            .map_err(|e| format!("connection lock poisoned: {e}"))?;
        f(&guard)
    }

    /// Resolve a project root to its `graph.db` path via `PathManager`.
    fn resolve_graph_db(project: &Path) -> Result<PathBuf, String> {
        let root = dunce::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
        let id = ProjectId::from_path(&root)
            .map_err(|e| format!("cannot hash project path {}: {e}", root.display()))?;
        let paths = PathManager::default_root();
        let db = paths.project_root(&id).join("graph.db");
        if !db.exists() {
            return Err(format!(
                "graph.db not found at {}. Run `mneme build .` first.",
                db.display()
            ));
        }
        Ok(db)
    }

    /// Mirrors `cli/src/commands/recall.rs::has_nodes_fts`.
    fn has_nodes_fts(conn: &Connection) -> Result<bool, String> {
        let mut stmt = conn
            .prepare(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'nodes_fts' LIMIT 1",
            )
            .map_err(|e| format!("prep fts check: {e}"))?;
        let exists: Option<i64> = stmt.query_row([], |row| row.get(0)).ok();
        Ok(exists.is_some())
    }

    /// Identical sanitizer to `cli/src/commands/recall.rs::fts5_sanitize`.
    fn fts5_sanitize(q: &str) -> String {
        let mut out = String::with_capacity(q.len());
        let mut last_was_space = true;
        for c in q.chars() {
            if c.is_alphanumeric() || c == '_' {
                out.push(c);
                last_was_space = false;
            } else if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        }
        out.trim().to_string()
    }

    fn recall_like(conn: &Connection, query: &str, limit: usize) -> Result<Vec<RecallHit>, String> {
        let pattern = format!("%{}%", query.replace('%', r"\%").replace('_', r"\_"));
        let sql = "
            SELECT kind, name, qualified_name, file_path, line_start
            FROM nodes
            WHERE name LIKE ?1 ESCAPE '\\' OR qualified_name LIKE ?1 ESCAPE '\\'
            ORDER BY LENGTH(qualified_name) ASC
            LIMIT ?2
        ";
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("prep like recall: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params![pattern, limit as i64], |row| {
                Ok(RecallHit {
                    kind: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    qualified_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    file_path: row.get::<_, Option<String>>(3)?,
                    line_start: row.get::<_, Option<i64>>(4)?,
                })
            })
            .map_err(|e| format!("exec like recall: {e}"))?;

        let mut hits = Vec::new();
        for h in rows.flatten() {
            hits.push(h);
        }
        Ok(hits)
    }

    fn recall_fts(conn: &Connection, raw: &str, limit: usize) -> Result<Vec<RecallHit>, String> {
        let sanitized = fts5_sanitize(raw);
        if sanitized.is_empty() {
            return recall_like(conn, raw, limit);
        }

        let sql = "
            SELECT n.kind, n.name, n.qualified_name, n.file_path, n.line_start
            FROM nodes_fts
            JOIN nodes n ON n.rowid = nodes_fts.rowid
            WHERE nodes_fts MATCH ?1
            ORDER BY rank
            LIMIT ?2
        ";
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("prep fts recall: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params![sanitized, limit as i64], |row| {
                Ok(RecallHit {
                    kind: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    qualified_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    file_path: row.get::<_, Option<String>>(3)?,
                    line_start: row.get::<_, Option<i64>>(4)?,
                })
            })
            .map_err(|e| format!("exec fts recall: {e}"))?;

        let mut hits = Vec::new();
        for r in rows {
            match r {
                Ok(h) => hits.push(h),
                Err(e) => return Err(format!("row map: {e}")),
            }
        }
        if hits.is_empty() {
            return recall_like(conn, raw, limit);
        }
        Ok(hits)
    }

    pub(super) fn run_recall(
        project: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, String> {
        let db = resolve_graph_db(project)?;
        // SD-4: cached read-only connection per `graph.db` path.
        with_cached_conn(&db, |conn| {
            if has_nodes_fts(conn)? {
                recall_fts(conn, query, limit)
            } else {
                recall_like(conn, query, limit)
            }
        })
    }

    pub(super) fn run_blast(
        project: &Path,
        target: &str,
        depth: usize,
    ) -> Result<Vec<BlastItem>, String> {
        let db = resolve_graph_db(project)?;
        // SD-4: cached read-only connection per `graph.db` path.
        with_cached_conn(&db, |conn| run_blast_inner(conn, target, depth))
    }

    fn run_blast_inner(
        conn: &Connection,
        target: &str,
        depth: usize,
    ) -> Result<Vec<BlastItem>, String> {
        // Resolve target to one or more starting node qualified_names.
        //
        // Match `file_path` BOTH with and without the Windows `\\?\`
        // long-path prefix. Parser stores canonical `\\?\C:\...` paths
        // but CLI users pass `C:\...`. Without the prefixed OR-clause
        // every blast on a Windows full-path target gets `starts=[]` and
        // returns 0 dependents.
        let starts: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    r#"SELECT qualified_name FROM nodes
                     WHERE qualified_name = ?1
                        OR name = ?1
                        OR file_path = ?1
                        OR file_path = '\\?\' || ?1
                     ORDER BY CASE
                       WHEN qualified_name = ?1 THEN 0
                       WHEN name = ?1 THEN 1
                       WHEN file_path = ?1 THEN 2
                       ELSE 3
                     END
                     LIMIT 10"#,
                )
                .map_err(|e| format!("prep target resolve: {e}"))?;
            let rows = stmt
                .query_map(rusqlite::params![target], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .map_err(|e| format!("exec target resolve: {e}"))?;
            let mut out = Vec::new();
            for r in rows {
                if let Ok(Some(q)) = r {
                    out.push(q);
                }
            }
            out
        };

        if starts.is_empty() {
            return Ok(Vec::new());
        }

        let mut visited: HashSet<String> = starts.iter().cloned().collect();
        let mut frontier: VecDeque<(String, usize)> =
            starts.iter().map(|s| (s.clone(), 0)).collect();
        let mut impacted: Vec<BlastItem> = Vec::new();

        let mut stmt = conn
            .prepare(
                "SELECT source_qualified FROM edges WHERE target_qualified = ?1
                 UNION
                 SELECT source_qualified FROM edges WHERE target_qualified IN (
                   SELECT qualified_name FROM nodes WHERE name = ?1
                 )",
            )
            .map_err(|e| format!("prep blast query: {e}"))?;

        while let Some((node, d)) = frontier.pop_front() {
            if d >= depth {
                continue;
            }
            let rows = stmt
                .query_map(rusqlite::params![node], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .map_err(|e| format!("exec blast query: {e}"))?;
            for r in rows {
                if let Ok(Some(src)) = r {
                    if visited.insert(src.clone()) {
                        let next_depth = d + 1;
                        impacted.push(BlastItem {
                            qualified_name: src.clone(),
                            depth: next_depth,
                        });
                        frontier.push_back((src, next_depth));
                    }
                }
            }
        }

        Ok(impacted)
    }

    pub(super) fn run_godnodes(project: &Path, n: usize) -> Result<Vec<GodNode>, String> {
        let db = resolve_graph_db(project)?;
        // SD-4: cached read-only connection per `graph.db` path.
        with_cached_conn(&db, |conn| run_godnodes_inner(conn, n))
    }

    fn run_godnodes_inner(conn: &Connection, n: usize) -> Result<Vec<GodNode>, String> {
        let sql = "
            WITH degrees AS (
                SELECT qn, SUM(fan_in) AS fan_in, SUM(fan_out) AS fan_out
                FROM (
                    SELECT target_qualified AS qn, 1 AS fan_in, 0 AS fan_out FROM edges
                    UNION ALL
                    SELECT source_qualified AS qn, 0 AS fan_in, 1 AS fan_out FROM edges
                )
                GROUP BY qn
            )
            SELECT n.qualified_name, n.kind, n.name, n.file_path,
                   (d.fan_in + d.fan_out) AS degree, d.fan_in, d.fan_out
            FROM degrees d
            JOIN nodes n ON n.qualified_name = d.qn
            ORDER BY degree DESC, n.qualified_name ASC
            LIMIT ?1
        ";
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("prep godnodes: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params![n as i64], |row| {
                Ok(GodNode {
                    qualified_name: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    kind: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    file_path: row.get::<_, Option<String>>(3)?,
                    degree: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    fan_in: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    fan_out: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                })
            })
            .map_err(|e| format!("exec godnodes: {e}"))?;

        let mut gods = Vec::new();
        for g in rows.flatten() {
            gods.push(g);
        }
        Ok(gods)
    }
}

async fn write_response(
    stream: &mut Stream,
    resp: &ControlResponse,
) -> Result<(), SupervisorError> {
    let body = serde_json::to_vec(resp)?;
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

/// Connect to the supervisor's IPC endpoint as a client.
///
/// Used by the binary's CLI subcommands and exposed publicly so other
/// workers / tests can speak the same protocol.
pub async fn connect_client(path: &Path) -> Result<Stream, SupervisorError> {
    #[cfg(unix)]
    {
        let name = path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| SupervisorError::Ipc(format!("name conversion failed: {e}")))?;
        <Stream as IpcStreamExt>::connect(name)
            .await
            .map_err(|e| SupervisorError::Ipc(format!("ipc connect failed: {e}")))
    }
    #[cfg(windows)]
    {
        let pipe_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("mneme-supervisor")
            .to_string();
        let name = pipe_name
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| SupervisorError::Ipc(format!("name conversion failed: {e}")))?;
        <Stream as IpcStreamExt>::connect(name)
            .await
            .map_err(|e| SupervisorError::Ipc(format!("ipc connect failed: {e}")))
    }
}

// ---------------------------------------------------------------------
// SD-4 / PERF-1: connection cache regression tests.
//
// These tests do NOT spin up a supervisor or hit the IPC wire — they
// drive `query_runner::with_cached_conn` (via the `_with_cached_conn_
// for_tests` hook) against a real on-disk SQLite file the test creates
// in a tempfile dir. The point is to prove that:
//
//   1. The first call against a fresh `graph.db` path inserts a
//      connection into the cache (cache MISS path).
//   2. A subsequent call against the SAME path re-uses that exact
//      `Arc<Mutex<Connection>>` (cache HIT path).
//   3. The connection actually works for SQL after being cached
//      (i.e. we're not handing back a half-initialised handle).
// ---------------------------------------------------------------------
#[cfg(test)]
mod conn_cache_tests {
    use super::query_runner;
    use rusqlite::{params, Connection};
    use std::path::PathBuf;

    /// Build a minimal graph.db with one row in `nodes` so the cached
    /// connection has something real to query.
    fn seed_graph_db(dir: &std::path::Path) -> PathBuf {
        let db = dir.join("graph.db");
        let conn = Connection::open(&db).expect("create test db");
        conn.execute_batch(
            "CREATE TABLE nodes (
                kind TEXT,
                name TEXT,
                qualified_name TEXT,
                file_path TEXT,
                line_start INTEGER
            );
            INSERT INTO nodes(kind, name, qualified_name, file_path, line_start)
            VALUES ('fn', 'main', 'crate::main', 'src/main.rs', 1);",
        )
        .expect("seed nodes");
        drop(conn);
        db
    }

    /// SD-4: a second call against the SAME `graph.db` path must reuse
    /// the cached `Arc<Mutex<Connection>>` rather than open a fresh
    /// SQLite handle. We assert this by comparing the `Arc::as_ptr`
    /// value across the two calls — a fresh Arc would have a different
    /// pointer value.
    #[test]
    fn cache_hit_reuses_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = seed_graph_db(dir.path());

        // Reset the cache so a previous test in the same binary
        // doesn't pollute state (the cache is process-wide).
        query_runner::_reset_conn_cache_for_tests();

        // Call 1: cache MISS — a connection should be inserted.
        assert!(
            !query_runner::_has_cached_conn(&db),
            "cache should be empty before first call"
        );
        let r1: i64 = query_runner::_with_cached_conn_for_tests(&db, |conn| {
            conn.query_row("SELECT COUNT(*) FROM nodes", params![], |row| row.get(0))
                .map_err(|e| format!("query: {e}"))
        })
        .expect("first call should succeed");
        assert_eq!(r1, 1, "seed row should be visible");
        assert!(
            query_runner::_has_cached_conn(&db),
            "cache should hold an entry after first call"
        );
        let ptr1 = query_runner::_cached_conn_arc_ptr(&db).expect("entry should be present");

        // Call 2: cache HIT — the same Arc should still be there.
        let r2: String = query_runner::_with_cached_conn_for_tests(&db, |conn| {
            conn.query_row(
                "SELECT qualified_name FROM nodes LIMIT 1",
                params![],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| format!("query: {e}"))
        })
        .expect("second call should succeed");
        assert_eq!(r2, "crate::main");
        let ptr2 = query_runner::_cached_conn_arc_ptr(&db).expect("entry should still be present");

        assert_eq!(
            ptr1, ptr2,
            "cache HIT must reuse the same Arc<Mutex<Connection>> \
             (ptr1 = {ptr1:#x}, ptr2 = {ptr2:#x}); a fresh open would \
             allocate a new Arc and the pointers would differ"
        );

        // Cleanup so we don't pollute neighbouring tests.
        query_runner::_reset_conn_cache_for_tests();
    }

    /// SD-4: distinct `graph.db` paths must each get their own cache
    /// entry — we never collide on the map key.
    #[test]
    fn cache_distinct_paths_get_distinct_entries() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let db_a = seed_graph_db(dir_a.path());
        let db_b = seed_graph_db(dir_b.path());

        query_runner::_reset_conn_cache_for_tests();

        // Touch both to populate the cache.
        let _: i64 = query_runner::_with_cached_conn_for_tests(&db_a, |c| {
            c.query_row("SELECT 1", params![], |r| r.get(0))
                .map_err(|e| format!("a: {e}"))
        })
        .expect("a call");
        let _: i64 = query_runner::_with_cached_conn_for_tests(&db_b, |c| {
            c.query_row("SELECT 1", params![], |r| r.get(0))
                .map_err(|e| format!("b: {e}"))
        })
        .expect("b call");

        let ptr_a = query_runner::_cached_conn_arc_ptr(&db_a).expect("a in cache");
        let ptr_b = query_runner::_cached_conn_arc_ptr(&db_b).expect("b in cache");
        assert_ne!(
            ptr_a, ptr_b,
            "distinct paths must map to distinct cache entries"
        );

        query_runner::_reset_conn_cache_for_tests();
    }
}
