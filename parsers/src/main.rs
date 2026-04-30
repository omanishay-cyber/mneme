//! `parse-worker` binary — the long-lived process the mneme supervisor
//! spawns (one instance, internally hosting N parser workers).
//!
//! Behaviour:
//! 1. Build a [`ParserPool`] sized to `cpu_count * 4` (§21.3).
//! 2. Pre-compile every cached query (warm the cache).
//! 3. Spawn N worker tasks, each owning an MPSC receiver.
//! 4. Read JSON-encoded [`ParseJob`]s from stdin (one per line).
//! 5. Round-robin them to workers; emit JSON [`ParseResult`]s on stdout.
//!
//! In production the supervisor talks to this process over IPC framed as
//! length-prefixed bytes; the JSON-over-stdio path here is identical in
//! contract and is what the integration tests in `mneme/tests/` drive.

use common::jobs::{JobId, JobOutcome};
use common::worker_ipc;
use mneme_parsers::{
    dispatch::{try_send_fanout, DispatchOutcome},
    incremental::IncrementalParser,
    parser_pool::ParserPool,
    query_cache,
    worker::Worker,
    ParseJob, ParserError,
};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

// AI-DNA pace tunables (feedback_mneme_ai_dna_pace.md Principle B).
//
// Both can be overridden at runtime via the `MNEME_PARSE_RESULT_CHANNEL_CAP`
// and `MNEME_PARSE_WORKER_JOB_CHANNEL_CAP` env vars so future tuning doesn't
// require a recompile. Resource policy
// (`docs/design/2026-04-23-resource-policy-addendum.md`): "no artificial caps
// on RAM, CPU, or disk", so a default 4×-bumped value is fine — the channel
// memory is `cap * sizeof(message)` which is bytes-per-thousand for these
// small enums.
const RESULT_CHANNEL_DEFAULT: usize = 4096;
const WORKER_JOB_CHANNEL_DEFAULT: usize = 256;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

fn result_channel_cap() -> usize {
    env_usize("MNEME_PARSE_RESULT_CHANNEL_CAP", RESULT_CHANNEL_DEFAULT)
}

fn worker_job_channel_cap() -> usize {
    env_usize(
        "MNEME_PARSE_WORKER_JOB_CHANNEL_CAP",
        WORKER_JOB_CHANNEL_DEFAULT,
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let pool = Arc::new(ParserPool::with_default_size()?);
    info!(
        languages = pool.enabled_languages().len(),
        workers_per_language = pool.workers_per_language(),
        "parser pool ready"
    );

    // Warm the query cache so first-parse latency hits the <50ms target.
    if let Err(e) = query_cache::warm_up() {
        // Non-fatal: bad pattern for one grammar shouldn't kill the worker.
        error!(error = %e, "query warm-up reported issues");
    }

    let inc = Arc::new(IncrementalParser::new(pool.clone()));

    // N workers. We keep parity with the pool's per-language count: the
    // supervisor decides job dispatch by language, so worker count is the
    // upper bound on language-parallelism inside this process.
    let worker_count = (num_cpus::get() * 4).max(4);
    info!(workers = worker_count, "spawning parser workers");

    // AI-DNA pace: result fan-in channel sized for `worker_count * burst-rate`.
    // When AI edits 10 files in 30s the supervisor fan-outs ~10 jobs in flight
    // per worker; the result emitter must absorb the return wave without
    // back-pressuring workers. 1024 → 4096 (4× headroom). See
    // feedback_mneme_ai_dna_pace.md Principle B: "every queue depth tuned for
    // AI-rate, not human-rate".
    let result_cap = result_channel_cap();
    let job_cap = worker_job_channel_cap();
    info!(result_cap, job_cap, "ai-dna pace channels sized");
    let (tx_results, mut rx_results) =
        mpsc::channel::<Result<mneme_parsers::ParseResult, ParserError>>(result_cap);
    let mut job_senders = Vec::with_capacity(worker_count);

    for id in 0..worker_count {
        // AI-DNA pace: per-worker job channel from 64 → 256 (4×). Pairs with the
        // M16 fan-out dispatcher: bigger per-worker buffers absorb burst writes
        // before the dispatcher has to fall back to send_timeout. See
        // feedback_mneme_ai_dna_pace.md Principle B.
        let (tx_jobs, rx_jobs) = mpsc::channel::<ParseJob>(job_cap);
        job_senders.push(tx_jobs);
        let worker = Worker::new(id, inc.clone(), rx_jobs, tx_results.clone());
        tokio::spawn(worker.run());
    }
    drop(tx_results); // workers each hold a clone; loop exits when all do

    // Result-emitter task: writes one JSON line per result to stdout AND
    // fires a typed `WorkerCompleteJob` IPC message to the supervisor.
    // The stdout line is kept intact so integration tests that drove the
    // worker via stdio (see `parsers/src/tests.rs`) continue to work;
    // the IPC push is purely additive.
    //
    // Routing note: per-job telemetry updates on the supervisor side
    // depend on the supervisor-assigned JobId, which arrives on the
    // stdin wire as `job_id`. Our `ParseResult` carries the same field,
    // so we reuse it here. When `job_id == 0` (jobs never routed via the
    // supervisor — e.g. ad-hoc stdio calls) we skip the IPC send.
    let result_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(res) = rx_results.recv().await {
            // Capture the telemetry we need before we consume `res`.
            let (job_id, outcome) = match &res {
                Ok(r) => (
                    r.job_id,
                    JobOutcome::Ok {
                        payload: Some(serde_json::json!({
                            "file_path": r.file_path,
                            "language": r.language,
                            "nodes_len": r.nodes.len(),
                            "edges_len": r.edges.len(),
                            "syntax_errors_len": r.syntax_errors.len(),
                            "incremental": r.incremental,
                        })),
                        duration_ms: r.parse_duration_ms,
                        stats: serde_json::json!({
                            "nodes": r.nodes.len(),
                            "edges": r.edges.len(),
                            "syntax_errors": r.syntax_errors.len(),
                            "incremental": r.incremental,
                        }),
                    },
                ),
                // Parser errors don't carry a job_id; we extract it only
                // on the Ok path. Still report a best-effort failure so
                // the queue marks the job failed and doesn't wait
                // forever.
                Err(e) => (
                    0,
                    JobOutcome::Err {
                        message: format!("{e}"),
                        duration_ms: 0,
                        stats: serde_json::Value::Null,
                    },
                ),
            };

            // Legacy stdout emit (unchanged).
            let line = match res {
                Ok(r) => serde_json::to_string(&r)
                    .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}")),
                Err(e) => format!("{{\"error\":\"{}\"}}", e),
            };
            if let Err(e) = stdout.write_all(line.as_bytes()).await {
                error!(error = %e, "stdout write failed");
                break;
            }
            let _ = stdout.write_all(b"\n").await;
            let _ = stdout.flush().await;

            // IPC emit (additive). Fire-and-forget — a deaf supervisor
            // must not wedge the worker's result pipeline.
            if job_id != 0 {
                if let Err(e) =
                    worker_ipc::report_complete(JobId(job_id), outcome).await
                {
                    debug!(error = %e, job_id, "worker_complete_job ipc send skipped");
                }
            }
        }
    });

    // Stdin reader: parse JSON jobs and round-robin to workers.
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut cursor = 0usize;

    // Read jobs from stdin until EOF, then keep the process alive on a
    // SIGINT/SIGTERM watch so the supervisor's monitor doesn't reap us the
    // moment our launcher closes stdin. A worker running under the
    // supervisor with no incoming jobs is the expected steady state — not
    // an exit condition.
    //
    // M16 — dispatch via `try_send_fanout` instead of strict round-robin.
    // The legacy `senders[target].send(job).await` pattern blocked the
    // dispatcher whenever the targeted worker was busy on a giant file,
    // even when other workers were idle (head-of-line blocking). The
    // fan-out walk skips past full channels and falls back to a bounded
    // 60s send_timeout only when every worker is at capacity.
    tokio::spawn(async move {
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let job: ParseJob = match serde_json::from_str::<JobWire>(&line) {
                Ok(j) => j.into_job(),
                Err(e) => {
                    error!(error = %e, raw = %line, "invalid job JSON");
                    continue;
                }
            };
            match try_send_fanout(&job_senders, cursor, job).await {
                DispatchOutcome::Delivered { index } => {
                    cursor = index.wrapping_add(1);
                }
                DispatchOutcome::AllFull => {
                    error!(
                        "all worker queues full for 60s; dropping job (every \
                         worker is wedged on slow parses or downstream stall)"
                    );
                }
                DispatchOutcome::AllClosed => {
                    error!("every worker queue closed; aborting dispatch loop");
                    break;
                }
            }
        }
        tracing::info!("stdin closed; parse-worker entering idle mode (waiting for signals)");
    });

    // Block forever on ctrl-c. Supervisor kills us with taskkill / SIGKILL
    // during shutdown.
    let _ = tokio::signal::ctrl_c().await;
    info!("parse-worker shutting down cleanly");
    let _ = result_task.await;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("MNEME_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

// ---------------------------------------------------------------------------
// Wire format — keeps the `content` field as a String for ergonomics on the
// stdin side, then converts to the Arc<Vec<u8>> the worker expects.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct JobWire {
    file_path: std::path::PathBuf,
    language: mneme_parsers::Language,
    /// UTF-8 source. Binary content is rejected at the supervisor layer.
    content: String,
    #[serde(default)]
    prev_tree_id: Option<u64>,
    #[serde(default)]
    job_id: u64,
}

impl JobWire {
    fn into_job(self) -> ParseJob {
        ParseJob {
            file_path: self.file_path,
            language: self.language,
            content: Arc::new(self.content.into_bytes()),
            prev_tree_id: self.prev_tree_id,
            content_hash: None,
            job_id: self.job_id,
        }
    }
}
