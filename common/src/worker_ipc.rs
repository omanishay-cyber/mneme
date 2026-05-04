//! Minimal supervisor-client helper used by worker binaries.
//!
//! Workers (parsers, scanners, brain, md-ingest, livebus, multimodal-bridge)
//! that were launched by the supervisor need a fire-and-forget channel to
//! push `WorkerCompleteJob` notifications back. The CLI already has its
//! own supervisor client in `cli/src/ipc.rs`; this module is the tiny
//! worker-side equivalent.
//!
//! Design:
//! * Discovery mirrors the CLI: read `<PathManager::default_root()>/supervisor.pipe`
//!   when present, fall back to the legacy
//!   `<PathManager::default_root()>/run/mneme-supervisor.sock`. Routing
//!   through `PathManager` honors the `MNEME_HOME` env override so workers
//!   spawned with the supervisor's env see the same install root.
//! * Wire format: 4-byte BE length prefix + JSON body, same as the
//!   supervisor's `IpcServer`.
//! * Failure mode is graceful — a supervisor that's gone deaf must NOT
//!   take the worker down. The helper returns an error and the caller
//!   logs it; the worker keeps processing jobs.
//!
//! The helper is additive: workers still write their legacy stdout
//! result lines. `WorkerCompleteJob` is a typed complement, not a
//! replacement — closes task P15 in ARCHITECTURE.md's worker dispatch
//! roadmap.
//!
//! Only two shapes are public:
//! * [`WorkerCompleteMessage`] — mirror of the supervisor's
//!   `ControlCommand::WorkerCompleteJob` variant so worker crates don't
//!   need a dep on `mneme-supervisor`.
//! * [`report_complete`] — async one-shot sender.

use crate::jobs::{JobId, JobOutcome};
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Stream as IpcStreamExt;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Default timeout for the one-shot report. Kept small because the
/// worker has more jobs to process; a deaf supervisor must not stall it.
pub const DEFAULT_REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// BUG-A2-037 fix: maximum bytes we will read from a supervisor reply.
/// Magic constant from the prior code; promoted to a named const so the
/// next caller-side bump is greppable. Replies above this cap are dropped
/// with a `tracing::warn!` event so silent truncation can't reoccur.
pub const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// Wire shape identical to supervisor::ipc::ControlCommand::WorkerCompleteJob.
///
/// Kept here (rather than re-exported from `mneme-supervisor`) so worker
/// crates avoid a reverse dependency on the supervisor binary's library.
/// `serde(tag = "command", rename_all = "snake_case")` matches the
/// supervisor's expected framing exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum WorkerCompleteMessage {
    /// A worker reporting it has finished processing a specific job.
    WorkerCompleteJob {
        /// Supervisor-assigned job id (taken from the router-emitted line).
        job_id: JobId,
        /// Completion outcome (ok or error) + duration + stats.
        outcome: JobOutcome,
    },
}

/// Push one [`WorkerCompleteMessage::WorkerCompleteJob`] to the supervisor.
///
/// Behaviour:
/// * If the supervisor socket/pipe cannot be found, returns `Err(NotFound)`.
/// * If the connect or write fails, returns `Err(Io)`.
/// * Never panics. Callers are expected to log-and-continue.
/// * The supervisor's reply (a `ControlResponse::Ok` / `::Error`) is
///   consumed and discarded — this is a one-shot push, not a request.
///
/// Called from each worker's per-job completion path in addition to the
/// existing stdout emission, so the stdout flow stays intact.
pub async fn report_complete(job_id: JobId, outcome: JobOutcome) -> Result<(), ReportError> {
    report_complete_to(
        job_id,
        outcome,
        &discover_socket_path().ok_or(ReportError::NotFound)?,
    )
    .await
}

/// Same as [`report_complete`] but with an explicit socket path. Exposed
/// primarily for tests that spawn a supervisor on a scratch socket.
///
/// Bug K (postmortem 2026-04-29 §12.2): on connect-failure (the daemon
/// "looks dead" — typically because it just respawned with a new PID
/// and a new pipe name), this also re-reads the discovery file once
/// and retries with the freshly-resolved path. The retry only fires if
/// the freshly-resolved path differs from `socket_path`. Without this,
/// a worker-to-supervisor send issued during a daemon respawn would
/// fail forever even though the new daemon is up at a freshly-written
/// pipe name.
pub async fn report_complete_to(
    job_id: JobId,
    outcome: JobOutcome,
    socket_path: &Path,
) -> Result<(), ReportError> {
    let msg = WorkerCompleteMessage::WorkerCompleteJob { job_id, outcome };
    let body = serde_json::to_vec(&msg).map_err(ReportError::Encode)?;
    // BUG-A2-036 fix: explicit overflow check on the u32 length prefix.
    // Pre-fix `body.len() as u32` truncated bodies > 4 GB and the
    // supervisor mis-parsed subsequent bytes as a new message,
    // desynchronising the protocol forever. We never write a body we
    // can't honestly describe in the prefix.
    let body_len: u32 = body.len().try_into().map_err(|_| {
        ReportError::Io(format!(
            "report body too large: {} bytes (max {})",
            body.len(),
            u32::MAX
        ))
    })?;
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&body_len.to_be_bytes());
    framed.extend_from_slice(&body);

    // First attempt — use the path the caller passed in.
    let first = run_round_trip(socket_path, &framed).await;
    if first.is_ok() {
        return first;
    }

    // Bug K: on failure, re-read the discovery file. If it now
    // points at a different path (daemon respawned mid-call), retry
    // once on the fresh path.
    if let Some(refreshed) = discover_socket_path() {
        if refreshed != socket_path {
            tracing::debug!(
                stale = %socket_path.display(),
                fresh = %refreshed.display(),
                "supervisor.pipe changed mid-call; retrying with fresh path (Bug K)"
            );
            return run_round_trip(&refreshed, &framed).await;
        }
    }
    first
}

/// Internal round-trip: connect, write, flush, read+discard reply.
/// Wrapped in [`DEFAULT_REPORT_TIMEOUT`] so a wedged supervisor never
/// stalls the worker.
async fn run_round_trip(socket_path: &Path, framed: &[u8]) -> Result<(), ReportError> {
    let work = async {
        let mut stream = connect_stream(socket_path).await?;
        stream
            .write_all(framed)
            .await
            .map_err(|e| ReportError::Io(format!("write: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| ReportError::Io(format!("flush: {e}")))?;

        // Consume the supervisor's reply so the pipe isn't left with
        // data trailing behind us. Any parse error is ignored — we
        // already delivered the notification.
        // BUG-A2-037 fix: the cap is now `MAX_REPLY_BYTES` (named const)
        // and oversize replies log a warn so silent truncation is
        // discoverable.
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_ok() {
            let len = u32::from_be_bytes(len_buf) as usize;
            if len <= MAX_REPLY_BYTES {
                let mut buf = vec![0u8; len];
                let _ = stream.read_exact(&mut buf).await;
            } else {
                tracing::warn!(
                    reply_bytes = len,
                    cap = MAX_REPLY_BYTES,
                    "supervisor reply exceeds MAX_REPLY_BYTES; discarding tail"
                );
            }
        }
        Ok::<(), ReportError>(())
    };

    tokio::time::timeout(DEFAULT_REPORT_TIMEOUT, work)
        .await
        .map_err(|_| ReportError::Timeout)?
}

/// Resolve the supervisor's socket/pipe path using the same discovery
/// rules the CLI uses.
///
/// Order:
/// 1. `$MNEME_SUPERVISOR_SOCKET` override (tests / unusual installs).
/// 2. `<PathManager::default_root()>/supervisor.pipe` (written by the
///    supervisor at boot — contains the actual pipe name; Windows
///    PID-scoped). Routing through [`PathManager`] honors the
///    `MNEME_HOME` env override (HOME-bypass-worker-ipc / M21 fix).
/// 3. Legacy fallback: `<PathManager::default_root()>/run/mneme-supervisor.sock`.
pub fn discover_socket_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MNEME_SUPERVISOR_SOCKET") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let root = crate::paths::PathManager::default_root()
        .root()
        .to_path_buf();
    let disco = root.join("supervisor.pipe");
    if let Ok(content) = std::fs::read_to_string(&disco) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            // BUG-A2-035 fix: the supervisor.pipe file is a writable
            // discovery hint; if a hostile process can write into
            // `<MNEME_HOME>/`, it could redirect worker IPC to an
            // attacker-controlled path and exfiltrate job results. We
            // validate the resolved path against the platform-specific
            // pipe-name shape and reject anything else, surfacing a
            // tracing warn so the operator can investigate.
            if is_acceptable_supervisor_path(trimmed) {
                return Some(PathBuf::from(trimmed));
            } else {
                tracing::warn!(
                    contents = %trimmed,
                    path = %disco.display(),
                    "supervisor.pipe contents do not match expected pipe shape; ignoring"
                );
            }
        }
    }
    Some(root.join("run").join("mneme-supervisor.sock"))
}

/// BUG-A2-035 helper: gate paths read from the on-disk discovery file
/// against the platform-specific naming contract.
///
/// Windows: must look like a named-pipe path containing `mneme-` (the
/// supervisor uses `\\.\pipe\mneme-supervisor-<pid>` etc).
/// Unix: must point inside the canonical install root and end with
/// `mneme-supervisor.sock` (or any `mneme-*.sock`).
fn is_acceptable_supervisor_path(raw: &str) -> bool {
    #[cfg(windows)]
    {
        // Accept Windows named pipes that mention "mneme-" anywhere in
        // the trailing path component. The leading `\\.\pipe\` is the
        // canonical form; some tests pass bare names.
        let lower = raw.to_ascii_lowercase();
        let starts_pipe = lower.starts_with(r"\\.\pipe\") || lower.starts_with(r"\\?\pipe\");
        let bare_name = !raw.contains('\\') && !raw.contains('/');
        let mentions_mneme = lower.contains("mneme-");
        return mentions_mneme && (starts_pipe || bare_name);
    }
    #[cfg(unix)]
    {
        // The supervisor only writes paths under PathManager::default_root(),
        // and the file name MUST start with `mneme-` and end in `.sock`.
        let path = std::path::Path::new(raw);
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            return false;
        };
        let name_ok = name.starts_with("mneme-") && name.ends_with(".sock");
        if !name_ok {
            return false;
        }
        // Best-effort containment check: must live under the install
        // root we resolved above. Tests may pass `/tmp/...` so we also
        // accept tmpdir for the test-friendly case.
        let root = crate::paths::PathManager::default_root()
            .root()
            .to_path_buf();
        let tmpdir = std::env::temp_dir();
        path.starts_with(&root) || path.starts_with(&tmpdir)
    }
}

async fn connect_stream(socket_path: &Path) -> Result<Stream, ReportError> {
    #[cfg(unix)]
    {
        let name = socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| ReportError::Io(format!("invalid path {}: {e}", socket_path.display())))?;
        <Stream as IpcStreamExt>::connect(name)
            .await
            .map_err(|e| ReportError::Io(format!("connect {}: {e}", socket_path.display())))
    }
    #[cfg(windows)]
    {
        let pipe_name = socket_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("mneme-supervisor")
            .to_string();
        let name = pipe_name
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| ReportError::Io(format!("invalid pipe name {pipe_name}: {e}")))?;
        <Stream as IpcStreamExt>::connect(name)
            .await
            .map_err(|e| ReportError::Io(format!("connect pipe '{pipe_name}': {e}")))
    }
}

/// Errors surfaced by [`report_complete`].
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// The supervisor socket/pipe couldn't be discovered.
    #[error("supervisor socket not discoverable (no MNEME_SUPERVISOR_SOCKET, no ~/.mneme/supervisor.pipe)")]
    NotFound,
    /// I/O error while connecting or writing.
    #[error("ipc io: {0}")]
    Io(String),
    /// The supervisor did not reply within the timeout.
    #[error("supervisor did not acknowledge within {:?}", DEFAULT_REPORT_TIMEOUT)]
    Timeout,
    /// JSON encode failure. Effectively impossible with our types, but we
    /// keep it explicit rather than `unwrap()`.
    #[error("encode: {0}")]
    Encode(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Both `discover_socket_*` tests mutate process-global env vars
    /// (`MNEME_SUPERVISOR_SOCKET`, `MNEME_HOME`). Cargo runs unit tests
    /// in parallel by default, so without serialization the two tests
    /// race and one ends up reading the other's mutation. This mutex
    /// is held for the duration of any env-touching test.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn worker_complete_message_round_trips_as_supervisor_command() {
        let msg = WorkerCompleteMessage::WorkerCompleteJob {
            job_id: JobId(99),
            outcome: JobOutcome::Ok {
                payload: None,
                duration_ms: 17,
                stats: json!({"nodes": 5}),
            },
        };
        let s = serde_json::to_string(&msg).unwrap();
        // Must match the supervisor ControlCommand JSON shape.
        assert!(s.contains(r#""command":"worker_complete_job""#), "got: {s}");
        assert!(s.contains(r#""job_id":99"#), "got: {s}");
        assert!(s.contains(r#""duration_ms":17"#), "got: {s}");
    }

    #[test]
    fn discover_socket_honours_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("MNEME_SUPERVISOR_SOCKET").ok();
        // BUG-A2-038 fix: wrap env mutations in `unsafe { ... }` for
        // forward-compat with Rust 1.81+ edition 2024 where
        // `set_var`/`remove_var` are gated. SAFETY: the ENV_LOCK mutex
        // serialises every env-touching test in this module.
        // Use set_var only for this test; unset afterwards.
        // Use a path that satisfies BUG-A2-035 validator on this platform.
        #[cfg(unix)]
        let probe = "/tmp/mneme-supervisor-test.sock";
        #[cfg(windows)]
        let probe = r"\\.\pipe\mneme-supervisor-test";
        unsafe {
            std::env::set_var("MNEME_SUPERVISOR_SOCKET", probe);
        }
        let p = discover_socket_path().expect("env override wins");
        assert_eq!(p, PathBuf::from(probe));
        unsafe {
            match saved {
                Some(v) => std::env::set_var("MNEME_SUPERVISOR_SOCKET", v),
                None => std::env::remove_var("MNEME_SUPERVISOR_SOCKET"),
            }
        }
    }

    /// HOME-bypass-worker-ipc / M21 regression:
    /// when `MNEME_HOME` is set, the legacy fallback path must be rooted
    /// under that override (`<MNEME_HOME>/run/mneme-supervisor.sock`)
    /// rather than `~/.mneme/run/mneme-supervisor.sock`.
    ///
    /// Pre-fix this test FAILED — `discover_socket_path` read
    /// `dirs::home_dir()` directly and ignored `MNEME_HOME`. After the
    /// M21 fix it routes through `PathManager::default_root()` which
    /// honors the override.
    #[test]
    fn discover_socket_honours_mneme_home_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Both env vars need to be exclusive for this test to be
        // deterministic. We save/restore both.
        let saved_sock = std::env::var("MNEME_SUPERVISOR_SOCKET").ok();
        let saved_home = std::env::var("MNEME_HOME").ok();

        // BUG-A2-038 fix: wrap env mutations in `unsafe { ... }`.
        // SAFETY: ENV_LOCK serialises env-touching tests in this module.
        unsafe {
            std::env::remove_var("MNEME_SUPERVISOR_SOCKET");
        }
        // Pick a path that's distinct from `~/.mneme` so the assertion
        // unambiguously proves the override was honored.
        let custom_root = std::env::temp_dir().join("mneme-m21-override-test");
        unsafe {
            std::env::set_var("MNEME_HOME", &custom_root);
        }

        let resolved = discover_socket_path().expect("path resolves");
        let expected_legacy = custom_root.join("run").join("mneme-supervisor.sock");
        assert_eq!(
            resolved, expected_legacy,
            "expected MNEME_HOME override to produce {expected_legacy:?}, got {resolved:?}"
        );

        // Restore prior env state.
        unsafe {
            match saved_sock {
                Some(v) => std::env::set_var("MNEME_SUPERVISOR_SOCKET", v),
                None => std::env::remove_var("MNEME_SUPERVISOR_SOCKET"),
            }
            match saved_home {
                Some(v) => std::env::set_var("MNEME_HOME", v),
                None => std::env::remove_var("MNEME_HOME"),
            }
        }
    }
}
