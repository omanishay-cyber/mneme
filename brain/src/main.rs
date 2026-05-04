//! `brain` binary entry point.
//!
//! Spawns the [`worker`] loop and bridges its `mpsc` job channel to whatever
//! IPC transport the supervisor is configured to use. The default build
//! reads NDJSON-encoded `BrainJob` records from stdin and writes
//! `BrainResult` records to stdout — this keeps the binary trivially
//! testable from a shell and makes it easy to swap in a Unix-domain or
//! Windows named-pipe transport later (per design §3) without touching the
//! worker code.
//!
//! Exit codes:
//!   0  normal shutdown
//!   1  fatal init failure (model paths bad, etc.)
//!   2  IO error on stdin/stdout

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use common::jobs::{JobId as SupervisorJobId, JobOutcome};
use common::worker_ipc;
use mneme_brain::worker::{spawn_worker, WorkerConfig};
use mneme_brain::{BrainJob, BrainResult};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    init_tracing();

    let cfg = match WorkerConfig::with_defaults() {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "brain init failed");
            return ExitCode::from(1);
        }
    };

    let mut handle = spawn_worker(cfg);
    info!("brain ready (NDJSON over stdio)");

    // Shared per-job timestamp ledger — stdin records when a job
    // arrives, stdout looks it up to compute `duration_ms`. Locked via
    // a tokio Mutex so both threads can poke it.
    let ledger = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::<
        u64,
        Instant,
    >::new()));

    // Forward stdin → jobs channel on a blocking thread.
    let jobs_tx = handle.jobs_tx.clone();
    let ledger_stdin = ledger.clone();
    let stdin_task = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut line = String::new();
        loop {
            line.clear();
            let n = handle.read_line(&mut line)?;
            if n == 0 {
                // EOF — ask the worker to shut down cleanly.
                let _ = jobs_tx.blocking_send(BrainJob::Shutdown);
                return Ok(());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<BrainJob>(trimmed) {
                Ok(job) => {
                    let job_id = job.id();
                    if job_id != 0 {
                        // blocking_lock is OK here: this task is already
                        // running on a blocking thread. Map insert is
                        // microseconds so lock contention is negligible.
                        ledger_stdin.blocking_lock().insert(job_id, Instant::now());
                    }
                    if jobs_tx.blocking_send(job).is_err() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    warn!(error = %e, "bad job json — skipping");
                }
            }
        }
    });

    // Forward results → stdout on the runtime.
    let ledger_stdout = ledger.clone();
    let stdout_task = tokio::spawn(async move {
        forward_results(&mut handle.results_rx, ledger_stdout).await;
        // When results channel closes, also wait for worker to wind down.
        let _ = handle.join.await;
    });

    // Wait on whichever side completes first.
    let exit_code = tokio::select! {
        r = stdin_task => {
            if let Ok(Err(e)) = r {
                error!(error = %e, "stdin reader exited with error");
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
        _ = stdout_task => ExitCode::SUCCESS,
    };

    // BUG-A2-018 fix: drain any leftover ledger entries on exit. Without
    // this, jobs that arrived on stdin but never produced a result before
    // the result channel closed would leak Instant entries forever in a
    // long-running daemon. The drain is a no-op in the common (clean
    // shutdown) case but bounds memory in the partial-failure case.
    {
        let mut g = ledger.lock().await;
        let stranded = g.len();
        if stranded > 0 {
            warn!(stranded, "brain shutdown: dropping stranded ledger entries");
            g.clear();
        }
    }

    exit_code
}

async fn forward_results(
    rx: &mut mpsc::Receiver<BrainResult>,
    ledger: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<u64, Instant>>>,
) {
    while let Some(result) = rx.recv().await {
        // Look up per-job start time BEFORE serialising (we need the
        // id; failure path also uses it).
        let job_id = result.id();
        let duration_ms = {
            let mut g = ledger.lock().await;
            g.remove(&job_id)
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0)
        };
        let (is_ok, message, stats) = result_telemetry(&result);

        let line = match serde_json::to_string(&result) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to serialise BrainResult");
                continue;
            }
        };
        // Scope the stdout lock so it drops before any .await below.
        // `StdoutLock` is !Send, which would otherwise poison the async
        // future.
        {
            let stdout = std::io::stdout();
            let mut h = stdout.lock();
            if writeln!(h, "{line}").is_err() {
                return;
            }
            if h.flush().is_err() {
                return;
            }
        }

        // IPC emit alongside stdout — fire-and-forget telemetry push.
        if job_id != 0 {
            let outcome = if is_ok {
                JobOutcome::Ok {
                    payload: None,
                    duration_ms,
                    stats,
                }
            } else {
                JobOutcome::Err {
                    message: message.unwrap_or_else(|| "brain worker error".to_string()),
                    duration_ms,
                    stats,
                }
            };
            if let Err(e) = worker_ipc::report_complete(SupervisorJobId(job_id), outcome).await {
                debug!(error = %e, job_id, "brain worker_complete_job ipc send skipped");
            }
        }
    }
}

/// Pull the ok/err + stats view out of a BrainResult without consuming it.
fn result_telemetry(result: &BrainResult) -> (bool, Option<String>, serde_json::Value) {
    match result {
        BrainResult::Embedding { vector, .. } => (
            true,
            None,
            serde_json::json!({"kind": "embedding", "dim": vector.len()}),
        ),
        BrainResult::EmbeddingBatch { vectors, .. } => (
            true,
            None,
            serde_json::json!({"kind": "embedding_batch", "count": vectors.len()}),
        ),
        BrainResult::Clusters { communities, .. } => (
            true,
            None,
            serde_json::json!({"kind": "clusters", "communities": communities.len()}),
        ),
        BrainResult::Concepts { concepts, .. } => (
            true,
            None,
            serde_json::json!({"kind": "concepts", "count": concepts.len()}),
        ),
        BrainResult::Summary { summary, .. } => (
            true,
            None,
            serde_json::json!({"kind": "summary", "chars": summary.len()}),
        ),
        BrainResult::Error { message, .. } => (
            false,
            Some(message.clone()),
            serde_json::json!({"kind": "error"}),
        ),
    }
}

fn init_tracing() {
    // Best-effort tracing init — don't crash if a global subscriber is
    // already set elsewhere. WIDE-013 / REG-016: previously gated on a
    // non-existent `tracing-subscriber` cargo feature, which produced
    // an `unexpected_cfg` warning and meant the no-op fallback was
    // ALWAYS taken. Now we always wire the real subscriber, with a
    // runtime JSON-vs-pretty switch via the `MNEME_LOG_FORMAT` env var
    // (`json` for structured logs, anything else / unset → pretty).
    let _ = tracing_subscriber_init();
}

fn tracing_subscriber_init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let want_json = std::env::var("MNEME_LOG_FORMAT")
        .ok()
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if want_json {
        fmt().with_env_filter(filter).json().try_init()?;
    } else {
        fmt().with_env_filter(filter).try_init()?;
    }
    Ok(())
}
