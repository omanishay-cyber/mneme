//! Mneme scan-worker binary entry point.
//!
//! Spawns a pool of [`ScanWorker`]s (default = `num_cpus * 2`), wires
//! them to a shared MPSC scan-job channel and a shared MPSC results
//! channel, attaches a [`StoreIpcBatcher`] that forwards findings to the
//! store-worker, and waits for SIGINT / Ctrl-C to drain.
//!
//! The actual cross-process plumbing (named pipe / Unix socket framing
//! between this binary and the store-worker) is owned by the supervisor
//! and store crates; this binary exposes the channels via stdin JSON
//! lines for now so the supervisor can pipe jobs in and receive batches
//! on stdout. That keeps this crate self-contained and IPC-transport
//! agnostic.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use common::jobs::{JobId, JobOutcome};
use common::worker_ipc;
use mneme_scanners::{
    job::{ScanJob, ScanResult},
    registry::{RegistryConfig, ScannerRegistry},
    store_ipc::{BatcherConfig, FindingsBatch, StoreIpcBatcher},
    worker::ScanWorker,
};

/// Channel capacity for both jobs and results.
const CHANNEL_CAP: usize = 1024;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    init_tracing();

    // Peek the first stdin line. If it parses as an `OrchestratorCommand`
    // (carries an `action: "scan_all"` field), dispatch to
    // `run_orchestrator_mode` and exit. Otherwise treat the line as the
    // first `StdinJob` of the supervisor's per-file pipeline (it gets
    // forwarded into the worker channel below).
    let stdin = tokio::io::stdin();
    let mut peeker = BufReader::new(stdin);
    let mut first_line = String::new();
    let n_read = match peeker.read_line(&mut first_line).await {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(error = %e, "failed to read first stdin line");
            return Err(e);
        }
    };
    if n_read > 0 {
        let trimmed = first_line.trim();
        if !trimmed.is_empty() {
            if let Ok(cmd) = serde_json::from_str::<OrchestratorCommand>(trimmed) {
                if cmd.action == "scan_all" {
                    return run_orchestrator_mode(cmd).await;
                }
            }
        }
    }

    // A3-015 (2026-05-04): clamp MNEME_SCAN_WORKERS env override to a
    // sane range. A user (or buggy supervisor) setting MNEME_SCAN_WORKERS
    // = 10000 would spawn 10K tasks each cloning the registry Arc and
    // contending on the jobs-channel mutex; lock contention alone makes
    // the system unusable. Cap at num_cpus * 8 (a 32-core box gets at
    // most 256 workers), floor at 1 (env="0" silently falls through to
    // the default rather than spawning zero workers).
    let cpus = num_cpus_or_default();
    let max_workers = scan_max_workers(cpus);
    let worker_count = std::env::var("MNEME_SCAN_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.clamp(1, max_workers))
        .unwrap_or_else(|| scan_default_workers(cpus));

    tracing::info!(workers = worker_count, "scan-worker pool starting");

    let registry = Arc::new(ScannerRegistry::new(RegistryConfig::default()));
    // B11.7 (v0.3.2): channel carries `ScanJobWithShard` so workers can
    // persist findings directly to per-project findings.db when running
    // under the supervisor-dispatched audit fan-out path. Legacy stdin
    // dispatch with no shard_root drops through to the batched stdout
    // pipe (which the supervisor's `monitor_child` only logs).
    let (jobs_tx, jobs_rx) = mpsc::channel::<ScanJobWithShard>(CHANNEL_CAP);
    let (results_tx, results_rx) = mpsc::channel::<ScanResult>(CHANNEL_CAP);
    let (batches_tx, mut batches_rx) = mpsc::channel::<FindingsBatch>(CHANNEL_CAP);

    // Fan-out tap — every ScanResult flowing to the batcher ALSO goes
    // here so we can fire `WorkerCompleteJob` IPC notifications without
    // forking the worker loop logic.
    let (ipc_tap_tx, mut ipc_tap_rx) = mpsc::channel::<(u64, u64, bool, usize, usize)>(CHANNEL_CAP);

    // Fan out workers, all sharing the same jobs receiver.
    let jobs_rx = Arc::new(tokio::sync::Mutex::new(jobs_rx));
    let mut worker_handles = Vec::with_capacity(worker_count);
    for id in 0..worker_count {
        let registry = registry.clone();
        let results = results_tx.clone();
        let jobs = jobs_rx.clone();
        let ipc_tap = ipc_tap_tx.clone();
        worker_handles.push(tokio::spawn(async move {
            let worker = ScanWorker::new(registry, id as u32);
            // B-029 (D:\Mneme Dome cycle, 2026-05-01): same per-file
            // timeout the orchestrator path uses (B-019/B-027). Without
            // it, a single pathological file can wedge a pool worker
            // forever — and the watchdog won't restart it (no worker
            // emits heartbeats per audit finding 1.1).
            const PER_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
            // B12 (v0.3.2): per-worker findings writer cache, keyed by
            // shard_root. Opened lazily on first job for each shard;
            // dropped on worker exit (Drop closes the connection).
            // SQLite WAL mode amortises the per-batch fsync; one writer
            // per shard preserves the per-shard single-writer invariant
            // even when multiple worker processes are active (each
            // worker = separate process, separate fd).
            //
            // A3-014 (2026-05-04): cap the cache at 16 entries with FIFO
            // eviction. Original implementation kept every shard's writer
            // open for the lifetime of the worker process. With workers
            // long-lived (loop until channel close) and multi-project
            // audits dispatching across many shards, the HashMap could
            // accumulate hundreds of open SQLite handles -- each with a
            // WAL+SHM file lock. 16 is enough headroom for any realistic
            // multi-project session; eviction closes the connection via
            // FindingsWriter Drop.
            let mut shard_writers: std::collections::HashMap<
                std::path::PathBuf,
                mneme_scanners::FindingsWriter,
            > = std::collections::HashMap::new();
            let mut shard_writer_order: std::collections::VecDeque<std::path::PathBuf> =
                std::collections::VecDeque::with_capacity(16);
            const SHARD_WRITER_CAP: usize = 16;
            // Each worker pops jobs from a shared mutex-protected receiver
            // (single channel, multiple consumers).
            loop {
                let with_shard = {
                    let mut guard = jobs.lock().await;
                    guard.recv().await
                };
                let Some(with_shard) = with_shard else { break };
                let ScanJobWithShard { job, shard_root } = with_shard;
                let job_path_for_log = job.file_path.clone();
                let job_id = job.job_id;
                let worker_clone = worker.clone();
                let res = match tokio::time::timeout(
                    PER_FILE_TIMEOUT,
                    tokio::task::spawn_blocking(move || worker_clone.run_one_blocking(job)),
                )
                .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(join_err)) => {
                        eprintln!(
                            "[pool-scan-spawn-error] worker={} file={} ({})",
                            id,
                            job_path_for_log.display(),
                            join_err
                        );
                        continue;
                    }
                    Err(_) => {
                        eprintln!(
                            "[pool-scan-timeout] worker={} file={} (exceeded {}s)",
                            id,
                            job_path_for_log.display(),
                            PER_FILE_TIMEOUT.as_secs()
                        );
                        // Still emit a synthetic ScanResult so downstream
                        // counters stay consistent; failed_scanners signals
                        // the timeout.
                        ScanResult {
                            job_id,
                            findings: Vec::new(),
                            scan_duration_ms: PER_FILE_TIMEOUT.as_millis() as u64,
                            failed_scanners: vec!["timeout".to_string()],
                        }
                    }
                };
                // B12 + B11.7 (v0.3.2): if the supervisor passed a
                // shard_root, persist findings DIRECTLY to that shard's
                // findings.db. This is the data-loss fix for the
                // supervisor-dispatch path — the batched stdout pipe
                // path (`results.send(...)` below) is not consumed by
                // the supervisor today, so without this direct write
                // the findings would be lost. We persist BEFORE sending
                // to the results channel so a panic / kill never
                // discards work.
                if !res.findings.is_empty() {
                    if let Some(shard) = shard_root.as_ref() {
                        let db_path = shard.join("findings.db");
                        let writer = if shard_writers.contains_key(shard) {
                            // A3-014: cache hit -- bump the entry to the
                            // back of the FIFO order so future evictions
                            // pick a less-recently-used shard first.
                            // Linear scan over <=16 entries is trivial.
                            if let Some(pos) = shard_writer_order
                                .iter()
                                .position(|p| p == shard)
                            {
                                let path = shard_writer_order.remove(pos).unwrap();
                                shard_writer_order.push_back(path);
                            }
                            shard_writers.get_mut(shard)
                        } else {
                            // A3-014: evict the oldest writer if we're at cap.
                            // FindingsWriter::Drop closes the SQLite handle.
                            if shard_writers.len() >= SHARD_WRITER_CAP {
                                if let Some(oldest) = shard_writer_order.pop_front() {
                                    shard_writers.remove(&oldest);
                                }
                            }
                            match mneme_scanners::FindingsWriter::open(&db_path) {
                                Ok(w) => {
                                    shard_writers.insert(shard.clone(), w);
                                    shard_writer_order.push_back(shard.clone());
                                    shard_writers.get_mut(shard)
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        worker_id = id,
                                        shard = %shard.display(),
                                        error = %e,
                                        "could not open findings.db for direct persist; \
                                         findings will only flow via the batched stdout pipe"
                                    );
                                    None
                                }
                            }
                        };
                        if let Some(w) = writer {
                            if let Err(e) = w.write_findings(&res.findings) {
                                tracing::warn!(
                                    worker_id = id,
                                    shard = %shard.display(),
                                    error = %e,
                                    findings = res.findings.len(),
                                    "direct findings.db write failed; continuing"
                                );
                            }
                        }
                    }
                }
                // Tap: best-effort IPC telemetry push. A full channel
                // just drops the tap — the primary batcher path still
                // completes.
                let ok = res.failed_scanners.is_empty();
                let _ = ipc_tap.try_send((
                    res.job_id,
                    res.scan_duration_ms,
                    ok,
                    res.findings.len(),
                    res.failed_scanners.len(),
                ));
                if results.send(res).await.is_err() {
                    break;
                }
            }
        }));
    }
    drop(results_tx);
    drop(ipc_tap_tx);

    // IPC tap consumer — emits one `WorkerCompleteJob` per scan
    // completion. Additive alongside the existing batched stdout path.
    let ipc_tap_handle = tokio::spawn(async move {
        while let Some((job_id, duration_ms, ok, findings_n, failed_n)) = ipc_tap_rx.recv().await {
            if job_id == 0 {
                continue;
            }
            let outcome = if ok {
                JobOutcome::Ok {
                    payload: None,
                    duration_ms,
                    stats: serde_json::json!({
                        "findings": findings_n,
                        "failed_scanners": failed_n,
                    }),
                }
            } else {
                JobOutcome::Err {
                    message: format!("{failed_n} scanner(s) failed"),
                    duration_ms,
                    stats: serde_json::json!({
                        "findings": findings_n,
                        "failed_scanners": failed_n,
                    }),
                }
            };
            if let Err(e) = worker_ipc::report_complete(JobId(job_id), outcome).await {
                tracing::debug!(error = %e, job_id, "scanner worker_complete_job ipc send skipped");
            }
        }
    });

    // Spawn the batcher.
    let batcher_handle = tokio::spawn(async move {
        let batcher = StoreIpcBatcher::new(BatcherConfig::default());
        if let Err(e) = batcher.run(results_rx, batches_tx).await {
            tracing::error!(error = %e, "batcher exited with error");
        }
    });

    // Forward batches to stdout as length-prefixed JSON.
    let stdout_handle = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(batch) = batches_rx.recv().await {
            let bytes = match serde_json::to_vec(&batch) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "failed to serialize batch");
                    continue;
                }
            };
            let len = (bytes.len() as u32).to_be_bytes();
            if stdout.write_all(&len).await.is_err() {
                break;
            }
            if stdout.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    // Read jobs from stdin (one JSON object per line) and push into the
    // jobs channel. We re-use the `peeker` buffered reader from the
    // orchestrator-mode sniff above so that the first stdin line —
    // which we already consumed into `first_line` — is processed BEFORE
    // we continue reading. This keeps the supervisor's existing
    // one-line dispatch contract intact even though we pre-read.
    let stdin_handle = {
        let jobs_tx = jobs_tx.clone();
        let prefilled = first_line;
        tokio::spawn(async move {
            // Process the already-consumed first line, if any.
            if !prefilled.trim().is_empty() {
                handle_stdin_line(&prefilled, &jobs_tx).await;
            }
            let mut reader = peeker.lines();
            while let Ok(Some(line)) = reader.next_line().await {
                handle_stdin_line(&line, &jobs_tx).await;
            }
        })
    };
    drop(jobs_tx);

    // Wait for shutdown signal.
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("ctrl-c received, draining workers");

    // Dropping all senders will close the chain.
    let _ = stdin_handle.await;
    for h in worker_handles {
        let _ = h.await;
    }
    let _ = batcher_handle.await;
    let _ = stdout_handle.await;
    let _ = ipc_tap_handle.await;

    tracing::info!("scan-worker pool exited cleanly");
    Ok(())
}

#[derive(serde::Deserialize)]
struct StdinJob {
    job_id: u64,
    file_path: String,
    content: String,
    #[serde(default)]
    ast_id: Option<u64>,
    #[serde(default)]
    scanner_filter: Vec<String>,
    /// B11.7 (v0.3.2, D:\Mneme Dome cycle, 2026-05-02): per-project shard
    /// root the supervisor's `Job::Scan` carries. When present (i.e. the
    /// worker was dispatched from `enqueue_audit`), the worker resolves
    /// the per-shard `findings.db` and streams findings directly to it
    /// via `FindingsWriter` after processing each file. This is what makes
    /// supervisor-dispatched audit fan-out actually persist results
    /// without going through the batched stdout pipe (which the
    /// supervisor's `monitor_child` only captures as log noise).
    ///
    /// Backward-compat: the legacy CLI-spawned subprocess path does NOT
    /// set `shard_root` (it streams findings to its own stdout pipe and
    /// the CLI persists them via B12). `#[serde(default)]` keeps that
    /// shape parsing cleanly.
    #[serde(default)]
    shard_root: Option<String>,
}

/// B11.7 (v0.3.2): wrapper that pairs a `ScanJob` with its optional
/// `shard_root`. The pool worker uses the shard_root to drive direct
/// findings.db persistence (B12 streaming guarantee) when running under
/// the supervisor-dispatched path. Legacy stdin-pipe path leaves
/// shard_root None and falls through to the old batched stdout pipe.
struct ScanJobWithShard {
    job: ScanJob,
    shard_root: Option<std::path::PathBuf>,
}

/// Forward one already-buffered stdin line to the worker pool. Skips
/// blanks and logs (but does not crash) on bad JSON. Shared by the
/// pre-fed `first_line` step and the loop body.
async fn handle_stdin_line(line: &str, jobs_tx: &mpsc::Sender<ScanJobWithShard>) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    match serde_json::from_str::<StdinJob>(trimmed) {
        Ok(stdin_job) => {
            let job = ScanJob {
                file_path: stdin_job.file_path.into(),
                content: Arc::new(stdin_job.content),
                ast_id: stdin_job.ast_id,
                scanner_filter: stdin_job.scanner_filter,
                job_id: stdin_job.job_id,
            };
            let with_shard = ScanJobWithShard {
                job,
                shard_root: stdin_job.shard_root.map(std::path::PathBuf::from),
            };
            let _ = jobs_tx.send(with_shard).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, line = %line, "bad scan job json");
        }
    }
}

/// One-shot orchestrator request. Read on the first stdin line when the
/// CLI's `mneme audit` command falls back from the supervisor IPC path.
///
/// Wire format:
/// ```json
/// {"action": "scan_all",
///  "project_root": "/abs/path",
///  "scope": "diff" | "full",
///  "scanner_filter": ["theme", "security"]}
/// ```
#[derive(serde::Deserialize)]
struct OrchestratorCommand {
    action: String,
    project_root: std::path::PathBuf,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default)]
    scanner_filter: Vec<String>,
}

fn default_scope() -> String {
    "full".to_string()
}

/// Walk `project_root`, run every applicable scanner against every
/// scannable file, and emit one JSON line per [`Finding`] to stdout.
/// Closes with a single summary line:
/// `{"_done": true, "scanned": N, "findings": M, "errors": K, "duration_ms": D}`
///
/// Used by `mneme audit` when the supervisor is unreachable. The CLI is
/// responsible for persisting the streamed findings to its
/// per-project `findings.db` shard.
async fn run_orchestrator_mode(cmd: OrchestratorCommand) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let started = std::time::Instant::now();
    let project_root = cmd.project_root.clone();
    if !project_root.is_absolute() {
        // Reject relative paths — the CLI should always pass an absolute
        // canonicalized path. Returning here would make stdout silently
        // empty, so we surface the failure instead.
        let line = serde_json::json!({
            "_error": format!("project_root is not absolute: {}", project_root.display())
        });
        let mut stdout = tokio::io::stdout();
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        stdout.write_all(&bytes).await?;
        return Ok(());
    }
    if !project_root.exists() {
        let line = serde_json::json!({
            "_error": format!("project_root does not exist: {}", project_root.display())
        });
        let mut stdout = tokio::io::stdout();
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        stdout.write_all(&bytes).await?;
        return Ok(());
    }

    tracing::info!(
        project = %project_root.display(),
        scope = %cmd.scope,
        action = %cmd.action,
        "orchestrator mode starting",
    );

    // Build the registry once. Project root is forwarded so the markdown-drift
    // scanner can resolve relative `[link](...)` claims.
    let registry = Arc::new(ScannerRegistry::new(RegistryConfig {
        project_root: Some(project_root.display().to_string()),
        ..RegistryConfig::default()
    }));

    let scope_filter = cmd.scope.as_str();
    let scanner_filter = cmd.scanner_filter;

    let walker = walkdir::WalkDir::new(&project_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hard_ignored(e.path()));

    // Optional gitignore (`.mnemeignore` first, then `.gitignore`).
    let gi = load_project_ignore(&project_root);

    // A3-013: counters moved into Arc<AtomicUsize> below so spawned tasks
    // can mutate concurrently. Old `let mut scanned = 0usize;` etc.
    // removed in the parallelization refactor.
    let stdout = tokio::io::stdout();
    let stdout = Arc::new(tokio::sync::Mutex::new(stdout));

    // A3-013 (2026-05-04): parallel orchestrator path.
    //
    // Original implementation used a single ScanWorker and processed files
    // sequentially. The pool path (B11.7, supervisor-dispatched) was
    // already parallelised; the orchestrator/fallback path was the holdout.
    // With the regex-bomb fixes (A3-001..A3-006) eliminating the worst
    // CPU spikes, single-threaded scanning is now leaving cores idle.
    //
    // Approach: keep the walker sequential (cheap directory iteration),
    // but spawn each file's scan + emit into a `JoinSet`, bounded by a
    // `Semaphore` of size num_cpus. Counters become atomics; stdout
    // remains arc-mutex'd for serialised output. Findings interleave
    // across files (the CLI parser doesn't depend on order). Per-file
    // timeout via `spawn_blocking + tokio::time::timeout` is preserved
    // inside each spawned task.
    let worker = ScanWorker::new(registry.clone(), 0);

    const PER_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    const PROGRESS_FILE_INTERVAL: usize = 25;
    const PROGRESS_TIME_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    // A3-012 (2026-05-04): per-file content size cap. A 50MB minified
    // bundle or 10MB lockfile would otherwise run all 10+ regexes against
    // the entire content, dominating the per-file 60s timeout budget.
    const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;

    // Counters as atomics so spawned tasks can mutate concurrently.
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    let scanned = Arc::new(AtomicUsize::new(0));
    let findings_total = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let timeouts = Arc::new(AtomicUsize::new(0));
    let files_since_progress = Arc::new(AtomicUsize::new(0));
    let last_progress_emit =
        Arc::new(tokio::sync::Mutex::new(std::time::Instant::now()));

    // Bounded concurrency: num_cpus. Higher values thrash; lower values
    // leave cores idle. Cap at 16 so a 64-core box doesn't spawn 64 task
    // futures + 64 blocking threads (tokio blocking pool default = 512
    // but our scanners are CPU-bound, not I/O-bound).
    let max_concurrency = num_cpus_or_default().max(1).min(16);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrency));
    let mut join_set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "walk error; continuing");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if let Some(gi) = gi.as_ref() {
            if matches!(gi.matched(path, false), ignore::Match::Ignore(_)) {
                continue;
            }
        }
        if !is_scannable_extension(path) {
            continue;
        }
        if scope_filter == "diff" && !was_modified_recently(path) {
            continue;
        }

        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > MAX_SCAN_BYTES {
                eprintln!(
                    "[scan-skip-large] {} bytes={} cap={}",
                    path.display(),
                    meta.len(),
                    MAX_SCAN_BYTES
                );
                continue;
            }
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(file = %path.display(), error = %e, "read failed; skipping");
                errors.fetch_add(1, AtomicOrdering::Relaxed);
                continue;
            }
        };
        if content.as_bytes().iter().take(512).any(|&b| b == 0) {
            continue;
        }

        // A3-013: acquire a permit BEFORE spawning. This back-pressures
        // the walker so we never have more than max_concurrency in-flight
        // scan tasks. acquire_owned().await yields when the cap is hit;
        // when a spawned task drops its permit (on completion) the next
        // walker iteration unblocks.
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // semaphore closed -- shutdown path
        };

        let worker_clone = worker.clone();
        let path_buf = path.to_path_buf();
        let scanner_filter_clone = scanner_filter.clone();
        let stdout_c = stdout.clone();
        let scanned_c = scanned.clone();
        let findings_total_c = findings_total.clone();
        let errors_c = errors.clone();
        let timeouts_c = timeouts.clone();
        let files_since_progress_c = files_since_progress.clone();
        let last_progress_emit_c = last_progress_emit.clone();

        join_set.spawn(async move {
            let _permit = permit; // dropped at task end -> next walker iter

            let job = ScanJob {
                file_path: path_buf.clone(),
                content: Arc::new(content),
                ast_id: None,
                scanner_filter: scanner_filter_clone,
                job_id: 0,
            };
            eprintln!("[scan-file] {}", path_buf.display());

            let worker_inner = worker_clone.clone();
            let res = match tokio::time::timeout(
                PER_FILE_TIMEOUT,
                tokio::task::spawn_blocking(move || worker_inner.run_one_blocking(job)),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(join_err)) => {
                    eprintln!(
                        "[scan-spawn-error] {} ({})",
                        path_buf.display(),
                        join_err
                    );
                    errors_c.fetch_add(1, AtomicOrdering::Relaxed);
                    return;
                }
                Err(_) => {
                    eprintln!(
                        "[scan-timeout] {} (exceeded {}s)",
                        path_buf.display(),
                        PER_FILE_TIMEOUT.as_secs()
                    );
                    timeouts_c.fetch_add(1, AtomicOrdering::Relaxed);
                    errors_c.fetch_add(1, AtomicOrdering::Relaxed);
                    return;
                }
            };
            scanned_c.fetch_add(1, AtomicOrdering::Relaxed);
            files_since_progress_c.fetch_add(1, AtomicOrdering::Relaxed);
            eprintln!(
                "[scan-done] {} findings={} failed={}",
                path_buf.display(),
                res.findings.len(),
                res.failed_scanners.len()
            );
            if !res.failed_scanners.is_empty() {
                errors_c
                    .fetch_add(res.failed_scanners.len(), AtomicOrdering::Relaxed);
            }
            let mut buf_out = stdout_c.lock().await;
            for f in &res.findings {
                findings_total_c.fetch_add(1, AtomicOrdering::Relaxed);
                let mut bytes = match serde_json::to_vec(f) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "finding serialize failed; dropping");
                        continue;
                    }
                };
                bytes.push(b'\n');
                if buf_out.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            // Progress heartbeat -- atomic snapshot of counters under the
            // last-emit mutex so two tasks don't double-emit on the same tick.
            let mut last_emit = last_progress_emit_c.lock().await;
            let cur_progress =
                files_since_progress_c.load(AtomicOrdering::Relaxed);
            if cur_progress >= PROGRESS_FILE_INTERVAL
                || last_emit.elapsed() >= PROGRESS_TIME_INTERVAL
            {
                let progress = serde_json::json!({
                    "_progress": true,
                    "scanned": scanned_c.load(AtomicOrdering::Relaxed),
                    "findings": findings_total_c.load(AtomicOrdering::Relaxed),
                    "errors": errors_c.load(AtomicOrdering::Relaxed),
                    "timeouts": timeouts_c.load(AtomicOrdering::Relaxed),
                    "current_file": path_buf.display().to_string(),
                });
                if let Ok(mut bytes) = serde_json::to_vec(&progress) {
                    bytes.push(b'\n');
                    let _ = buf_out.write_all(&bytes).await;
                }
                files_since_progress_c.store(0, AtomicOrdering::Relaxed);
                *last_emit = std::time::Instant::now();
            }
            drop(last_emit);
            let _ = buf_out.flush().await;
        });
    }

    // Drain all in-flight scan tasks before emitting the summary.
    while join_set.join_next().await.is_some() {}

    let duration_ms = started.elapsed().as_millis() as u64;
    let summary = serde_json::json!({
        "_done": true,
        "scanned": scanned.load(AtomicOrdering::Relaxed),
        "findings": findings_total.load(AtomicOrdering::Relaxed),
        "errors": errors.load(AtomicOrdering::Relaxed),
        "timeouts": timeouts.load(AtomicOrdering::Relaxed),
        "duration_ms": duration_ms,
    });
    let mut summary_bytes = serde_json::to_vec(&summary)?;
    summary_bytes.push(b'\n');
    let mut buf_out = stdout.lock().await;
    buf_out.write_all(&summary_bytes).await?;
    let _ = buf_out.flush().await;
    Ok(())
}

/// Hard-coded ignore list — directories the orchestrator must NEVER
/// descend into. Mirrors `cli::commands::build::is_ignored` but kept
/// local so the scanners crate stays decoupled from the CLI.
fn is_hard_ignored(path: &std::path::Path) -> bool {
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
            | "AppData"
            | "OneDrive"
            | "$Recycle.Bin"
            | "NTUSER.DAT"
            | "Library"
            | ".Trash"
            | "Applications"
            | ".mneme"
            | ".claude"
            // B-016 (v0.3.2-v2-home): cache/output dirs that any scanner
            // pass would only waste cycles on. `graphify-out` belongs to
            // the user's `mneme graphify` cache (thousands of JSON
            // entries that drained the audit budget on Orion). The
            // others are common build/output artefacts that already
            // appear in the wider build hard-ignore list but were
            // missing here.
            | "graphify-out"
            | "coverage"
            | ".nyc_output"
            | ".gradle"
            | ".rush"
            | ".mneme-graphify"
            | ".datatree"
    )
}

/// Build an `ignore::gitignore::Gitignore` matcher from the project's
/// `.mnemeignore` (preferred) or `.gitignore`. Returns `None` if
/// neither exists.
fn load_project_ignore(root: &std::path::Path) -> Option<ignore::gitignore::Gitignore> {
    use ignore::gitignore::GitignoreBuilder;
    for name in [".mnemeignore", ".gitignore"] {
        let p = root.join(name);
        if p.exists() {
            let mut builder = GitignoreBuilder::new(root);
            if builder.add(&p).is_some() {
                continue;
            }
            if let Ok(gi) = builder.build() {
                return Some(gi);
            }
        }
    }
    None
}

/// Cheap extension allowlist. Mirrors the languages tree-sitter actually
/// supports inside the CLI build pipeline. We keep this hand-maintained
/// instead of depending on the parsers crate so the scanners crate
/// stays small (no tree-sitter on the orchestrator path).
fn is_scannable_extension(path: &std::path::Path) -> bool {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(s) => s.to_ascii_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        // TS/JS family
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs"
        // Web
        | "html" | "htm" | "css" | "scss" | "sass" | "less"
        // Systems
        | "rs" | "go" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh"
        // JVM
        | "java" | "kt" | "kts" | "scala"
        // Scripting
        | "py" | "rb" | "lua" | "sh" | "bash" | "zsh" | "fish" | "ps1"
        // Config-ish
        | "json" | "yaml" | "yml" | "toml" | "ini"
        // Markup
        | "md" | "markdown" | "mdx" | "rst" | "txt"
        // Misc
        | "swift" | "dart" | "ex" | "exs" | "elm" | "vue" | "svelte" | "astro"
        | "sol" | "zig" | "cs" | "fs" | "fsx" | "vb" | "php"
    )
}

/// Approximate `--scope=diff` filter: was this file modified in the
/// last 24 hours? Cheap mtime check; deliberately coarse — the
/// supervised path uses a real `git diff HEAD` set, but the fallback
/// ships the simpler version.
fn was_modified_recently(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return true;
    };
    // 24h cutoff.
    elapsed.as_secs() < 86_400
}

fn num_cpus_or_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// A10-010 (2026-05-04): default scan-worker count, extracted for unit
/// tests. Mirrors the original `(cpus * 2).max(2)` expression.
pub fn scan_default_workers(cpus: usize) -> usize {
    (cpus * 2).max(2)
}

/// A10-010: env-override clamp ceiling for `MNEME_SCAN_WORKERS`.
/// `cpus * 8` floored at 8, `saturating_mul` so no overflow on a
/// pathological cpus value.
pub fn scan_max_workers(cpus: usize) -> usize {
    cpus.saturating_mul(8).max(8)
}

#[cfg(test)]
mod scan_pool_size_tests {
    use super::{scan_default_workers, scan_max_workers};

    #[test]
    fn scan_default_workers_floors_at_2_for_zero_or_one_cpu() {
        assert_eq!(scan_default_workers(0), 2);
        assert_eq!(scan_default_workers(1), 2);
    }

    #[test]
    fn scan_default_workers_scales_2x_per_cpu() {
        assert_eq!(scan_default_workers(2), 4);
        assert_eq!(scan_default_workers(4), 8);
        assert_eq!(scan_default_workers(8), 16);
        assert_eq!(scan_default_workers(16), 32);
        assert_eq!(scan_default_workers(32), 64);
        assert_eq!(scan_default_workers(64), 128);
    }

    #[test]
    fn scan_max_workers_floors_at_8_for_zero_or_one_cpu() {
        assert_eq!(scan_max_workers(0), 8);
        assert_eq!(scan_max_workers(1), 8);
    }

    #[test]
    fn scan_max_workers_scales_8x_per_cpu() {
        assert_eq!(scan_max_workers(2), 16);
        assert_eq!(scan_max_workers(4), 32);
        assert_eq!(scan_max_workers(8), 64);
        assert_eq!(scan_max_workers(16), 128);
        assert_eq!(scan_max_workers(32), 256);
        assert_eq!(scan_max_workers(64), 512);
    }

    #[test]
    fn scan_max_workers_does_not_overflow_on_pathological_cpus() {
        // saturating_mul catches a usize::MAX cpus value.
        assert_eq!(scan_max_workers(usize::MAX), usize::MAX);
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();

    // B-008: panic hook so crashes in release (panic = "abort") surface a
    // grep-able panic site to stderr BEFORE abort fires. The CLI audit
    // path (audit.rs::run_direct_subprocess_with_registry) drains and
    // logs this on non-zero exit so the user sees `[SCANNER PANIC] ...`
    // in the build summary instead of an opaque "subprocess crashed".
    //
    // Combined with the per-file `[scan-file] <path>` checkpoint emitted
    // by `run_orchestrator_mode`, the LAST `[scan-file]` line in the
    // captured stderr identifies the file that triggered the panic.
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| String::from("<unknown>"));
        let payload = info
            .payload()
            .downcast_ref::<&'static str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| String::from("<non-string panic payload>"));
        eprintln!("[SCANNER PANIC] location={location} message={payload}");
        if std::env::var("RUST_BACKTRACE").is_ok() {
            eprintln!(
                "[SCANNER PANIC] backtrace:\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
    }));
}
