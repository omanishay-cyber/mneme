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

    let worker_count = std::env::var("MNEME_SCAN_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| (num_cpus_or_default() * 2).max(2));

    tracing::info!(workers = worker_count, "scan-worker pool starting");

    let registry = Arc::new(ScannerRegistry::new(RegistryConfig::default()));
    let (jobs_tx, jobs_rx) = mpsc::channel::<ScanJob>(CHANNEL_CAP);
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
            // Each worker pops jobs from a shared mutex-protected receiver
            // (single channel, multiple consumers).
            loop {
                let job = {
                    let mut guard = jobs.lock().await;
                    guard.recv().await
                };
                let Some(job) = job else { break };
                let res = worker.run_one(job).await;
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
}

/// Forward one already-buffered stdin line to the worker pool. Skips
/// blanks and logs (but does not crash) on bad JSON. Shared by the
/// pre-fed `first_line` step and the loop body.
async fn handle_stdin_line(line: &str, jobs_tx: &mpsc::Sender<ScanJob>) {
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
            let _ = jobs_tx.send(job).await;
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

    let mut scanned = 0usize;
    let mut findings_total = 0usize;
    let mut errors = 0usize;
    let mut timeouts = 0usize;
    let stdout = tokio::io::stdout();
    let stdout = Arc::new(tokio::sync::Mutex::new(stdout));

    // Worker for the orchestrator path — single instance because we walk
    // the project sequentially. Scanners themselves are CPU-cheap; the
    // dominant cost is file IO. Parallelising the walk would complicate
    // ordered stdout emission; stick with the simple sequential path.
    let worker = ScanWorker::new(registry.clone(), 0);

    // B-019 (D:\Mneme Dome cycle, 2026-04-30): per-file timeout +
    // stdout heartbeat. Two related fixes for the same symptom — CLI
    // observes scanner subprocess produced no stdout for >30s on real
    // Electron projects (37k+ partial findings before kill). Causes:
    //   a) A single file can wedge `worker.run_one()` (e.g. pathological
    //      regex, runaway tree-sitter walk). Wrap the call in a 60s
    //      timeout so guaranteed forward progress to the next file.
    //   b) Long stretches of files that produce no findings leave stdout
    //      silent for minutes; the CLI's line_budget then false-positives
    //      a hang. Emit a `_progress` JSON line every 25 files (or every
    //      5s, whichever first) so the read loop sees fresh bytes.
    const PER_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    const PROGRESS_FILE_INTERVAL: usize = 25;
    const PROGRESS_TIME_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    let mut last_progress_emit = std::time::Instant::now();
    let mut files_since_progress: usize = 0;

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
        // `diff` scope: today we approximate "diff" as "files changed in
        // the last 24h" — a cheap mtime filter. The CLI honours the
        // semantic upgrade to a real `git diff HEAD` set in the
        // supervised path; the fallback ships the simple version.
        if scope_filter == "diff" && !was_modified_recently(path) {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(file = %path.display(), error = %e, "read failed; skipping");
                errors += 1;
                continue;
            }
        };
        // Skip files whose first 512 bytes contain a NUL — almost
        // certainly binary.
        if content.as_bytes().iter().take(512).any(|&b| b == 0) {
            continue;
        }

        let job = ScanJob {
            file_path: path.to_path_buf(),
            content: Arc::new(content),
            ast_id: None,
            scanner_filter: scanner_filter.clone(),
            job_id: 0,
        };
        // B-008: per-file checkpoint to stderr. The last `[scan-file]`
        // line drained by the CLI on a non-zero exit pinpoints the file
        // that triggered a scanner panic (panic = "abort" in release
        // makes the catch_unwind in worker.rs a no-op, so process-level
        // diagnostics are the only signal).
        eprintln!("[scan-file] {}", path.display());
        // B-019: per-file timeout. Any single file taking >60s indicates
        // a runaway scanner; we record it and move on. The stderr
        // checkpoint above pinpoints the offender for postmortem.
        let res = match tokio::time::timeout(PER_FILE_TIMEOUT, worker.run_one(job)).await {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "[scan-timeout] {} (exceeded {}s)",
                    path.display(),
                    PER_FILE_TIMEOUT.as_secs()
                );
                timeouts += 1;
                errors += 1;
                continue;
            }
        };
        scanned += 1;
        files_since_progress += 1;
        if !res.failed_scanners.is_empty() {
            errors += res.failed_scanners.len();
        }
        let mut buf_out = stdout.lock().await;
        for f in &res.findings {
            findings_total += 1;
            // One JSON line per Finding. Trailing newline is mandatory
            // — the CLI parser splits on `\n` and skips blank lines.
            let mut bytes = match serde_json::to_vec(f) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "finding serialize failed; dropping");
                    continue;
                }
            };
            bytes.push(b'\n');
            if buf_out.write_all(&bytes).await.is_err() {
                // Reader hung up — bail.
                break;
            }
        }
        // B-019: emit a `_progress` heartbeat so the CLI's line_budget
        // never fires on long stretches of zero-finding files.
        let need_progress = files_since_progress >= PROGRESS_FILE_INTERVAL
            || last_progress_emit.elapsed() >= PROGRESS_TIME_INTERVAL;
        if need_progress {
            let progress = serde_json::json!({
                "_progress": true,
                "scanned": scanned,
                "findings": findings_total,
                "errors": errors,
                "timeouts": timeouts,
                "current_file": path.display().to_string(),
            });
            if let Ok(mut bytes) = serde_json::to_vec(&progress) {
                bytes.push(b'\n');
                let _ = buf_out.write_all(&bytes).await;
            }
            files_since_progress = 0;
            last_progress_emit = std::time::Instant::now();
        }
        let _ = buf_out.flush().await;
        drop(buf_out);
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    let summary = serde_json::json!({
        "_done": true,
        "scanned": scanned,
        "findings": findings_total,
        "errors": errors,
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
