//! `mneme audit [--scope=...] [--severity=...]` — run all configured scanners.
//!
//! Two paths:
//!   1. **Supervisor IPC** (preferred): the daemon owns the scanner pool
//!      and writes findings to `~/.mneme/projects/<id>/findings.db`
//!      asynchronously. Returns the JSON findings list.
//!   2. **Direct subprocess fallback** (this file): when the supervisor
//!      is down or rejects the request, spawn `mneme-scanners` as a
//!      child and pipe a one-line `scan_all` orchestration command into
//!      its stdin. The worker walks the project, runs every applicable
//!      scanner, and emits one JSON-line [`Finding`] per discovered
//!      issue on stdout, terminating with a `{"_done": ..., ...}`
//!      summary line. The CLI persists those findings to the per-project
//!      `findings.db` shard via [`mneme_scanners::FindingsWriter`] and
//!      prints a human-friendly summary table.
//!
//! Exit codes:
//!   - 0  : no critical findings (or no findings at all)
//!   - 1  : at least one `critical` finding present (after `--severity` filter)
//!   - 5  : subprocess failed to spawn / crashed / wrote malformed output

use clap::Args;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child as TokioChild, Command as TokioCommand};
use tracing::{debug, info, warn};

use crate::commands::build::make_client;
use crate::error::{CliError, CliResult};
use crate::ipc::{IpcRequest, IpcResponse};

use common::{ids::ProjectId, layer::DbLayer, paths::PathManager};
use scanners::{Finding, FindingsWriter, Severity};

// B11.8 (v0.3.2, D:\Mneme Dome cycle, 2026-05-02): the outer wall-clock
// (`AUDIT_SUBPROCESS_BUDGET` / `MNEME_AUDIT_TIMEOUT_SEC`, 300 s) was
// removed. It killed slow-but-working scans on large projects (a high-end AWS instance:
// 30 min single-threaded scan got SIGKILLed at 5 min, losing 37,423
// partial findings). The per-line read budget below is now the SOLE
// hang guard — see `LINE_READ_BUDGET`. Combined with the streaming
// findings writer (B12) any kill still leaves persisted rows intact.

/// Bug M9 (D-window class): canonical Windows process-creation flags
/// for the `mneme-scanners` subprocess spawned by
/// `run_direct_subprocess`. Sets `CREATE_NO_WINDOW` (`0x08000000`) so
/// no console window flashes when `mneme audit` (or `mneme build`,
/// which calls into audit) runs from a hook context whose parent is
/// itself windowless. The constant is exposed unconditionally so
/// pure-Rust unit tests can pin the contract on every host platform —
/// the `cmd.creation_flags(...)` call site is `#[cfg(windows)]` only.
pub(crate) fn windows_audit_subprocess_flags() -> u32 {
    /// CREATE_NO_WINDOW from `windows-sys`: suppresses console window
    /// allocation for the child process. The canonical Win32 doc:
    /// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    CREATE_NO_WINDOW
}

/// Per-stdout-line read cap. If the scanners worker writes nothing for
/// this long we treat that iteration as a real hang, log a warning, and
/// kill the child. Override via `MNEME_AUDIT_LINE_TIMEOUT_SEC`.
///
/// B11.8 (v0.3.2): this is now the SOLE hang guard for the audit
/// subprocess. The scanner subprocess emits a `_progress` heartbeat
/// every 25 files / 5 s (B-019), so any silence > 30 s is a real wedge.
pub(crate) const LINE_READ_BUDGET: Duration = Duration::from_secs(30);

/// Read `MNEME_AUDIT_LINE_TIMEOUT_SEC` (positive integer seconds) or fall
/// back to [`LINE_READ_BUDGET`]. Zero / junk values fall back — we never
/// want a fully-disabled per-line cap (would deadlock on a wedged child).
fn audit_line_budget() -> Duration {
    parse_env_secs_or("MNEME_AUDIT_LINE_TIMEOUT_SEC", LINE_READ_BUDGET)
}

/// B12 (v0.3.2, D:\Mneme Dome cycle, 2026-05-02): incremental flush cap
/// for the streaming finding writer. The CLI accumulates findings in a
/// small buffer and flushes whenever the buffer fills OR a periodic
/// timer fires — whichever comes first. This is the data-loss fix:
/// before B12, the CLI accumulated EVERY finding from the entire scan
/// in memory and bulk-inserted at end-of-stream; if the subprocess died
/// (timeout, panic, kill) all findings were lost. Our AWS test fleet hit this:
/// 37,423 partial findings → 0 persisted on a wall-clock kill.
const FINDINGS_FLUSH_BUFFER: usize = 100;

/// B12: time-based flush cadence. Even if the buffer never fills (e.g.
/// long stretch of zero-finding files), we flush every 5 s so a kill
/// at any time leaves at most 5 s of work unpersisted.
const FINDINGS_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Helper: parse an env var as a positive integer count of seconds, return
/// the fallback on absent / unparseable / zero. We reject zero on purpose
/// — `Duration::from_secs(0)` would make `tokio::time::timeout` fire
/// immediately on every line, instantly failing every audit. If users
/// genuinely want "fast fail" they can pass `1`.
fn parse_env_secs_or(name: &str, fallback: Duration) -> Duration {
    match std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok()) {
        Some(n) if n > 0 => Duration::from_secs(n),
        _ => fallback,
    }
}

/// CLI args for `mneme audit`.
#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Scope filter: `full` (every scannable file) or `diff` (only files
    /// changed in the last 24h — fast pre-commit check).
    #[arg(long, default_value = "full")]
    pub scope: String,

    /// Lower-bound severity filter. Findings less severe than this are
    /// dropped before printing. Order: `critical` > `error` > `warn` >
    /// `info`. Defaults to `info` (no filter).
    #[arg(long, default_value = "info")]
    pub severity: String,

    /// Optional project root. Defaults to CWD.
    pub project: Option<PathBuf>,
}

/// Entry point used by `main.rs`.
pub async fn run(args: AuditArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    let project = resolve_project(args.project.clone())?;
    let severity_floor = parse_severity(&args.severity)?;
    let scope = normalise_scope(&args.scope)?;

    info!(
        project = %project.display(),
        scope,
        severity = severity_floor.label(),
        "starting mneme audit",
    );

    // Try IPC-first. On any failure (Err or `IpcResponse::Error`) we
    // fall back to the direct subprocess path below.
    let client = make_client(socket_override.clone());
    let ipc_attempt = client
        .request(IpcRequest::Audit {
            scope: scope.to_string(),
            // B11.7 (v0.3.2): pass project so the supervisor can fan
            // out per-file `Job::Scan` to the scanner-worker pool.
            project: Some(project.clone()),
        })
        .await;

    match ipc_attempt {
        Ok(IpcResponse::Error { message }) => {
            warn!(error = %message, "supervisor returned error; falling back to direct subprocess");
        }
        Ok(other) => {
            // Supervisor handled it — reuse the standard renderer.
            return crate::commands::build::handle_response(other);
        }
        Err(e) => {
            warn!(error = %e, "supervisor unreachable; falling back to direct subprocess");
        }
    }

    run_direct_subprocess(&project, scope, severity_floor).await
}

/// Subprocess fallback: spawn `mneme-scanners`, pipe an orchestrator
/// `scan_all` command into stdin, stream findings back from stdout, and
/// persist them to the per-project `findings.db` shard.
///
/// Exposed at `pub(crate)` so `commands::build::run_audit_pass` can take
/// the IPC-bypass path when `mneme build --inline` is in effect — see
/// audit-fix 4.3. Without this entry point the audit pass goes via
/// `audit::run`, which hits `IpcClient::request`, which auto-spawns
/// `mneme-daemon` on dead-pipe — leaking daemon processes during what
/// the user explicitly asked to be an in-process build.
pub(crate) async fn run_direct_subprocess(
    project: &Path,
    scope: &'static str,
    severity_floor: Severity,
) -> CliResult<()> {
    run_direct_subprocess_with_registry(project, scope, severity_floor, None).await
}

/// B-003: registry-aware variant of [`run_direct_subprocess`]. Same
/// behaviour, but registers the spawned child PID with the build's
/// `BuildChildRegistry` so a Ctrl-C arriving mid-scan deterministically
/// taskkills the worker instead of leaving an orphan. The default
/// `audit::run` entry point still calls the no-registry variant —
/// only `mneme build`'s `run_audit_pass` threads its registry through.
pub(crate) async fn run_direct_subprocess_with_registry(
    project: &Path,
    scope: &'static str,
    severity_floor: Severity,
    registry: Option<crate::commands::build::BuildChildRegistry>,
) -> CliResult<()> {
    let bin = resolve_scanners_binary()?;
    debug!(bin = %bin.display(), "resolved scanners binary");

    // Build the orchestrator command JSON. The scanners worker recognises
    // this on the very first stdin line.
    let cmd_json = serde_json::json!({
        "action": "scan_all",
        "project_root": project,
        "scope": scope,
        "scanner_filter": Vec::<String>::new(),
    });
    let cmd_line = format!("{}\n", serde_json::to_string(&cmd_json)?);

    let mut cmd = TokioCommand::new(&bin);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Bug M9 (D-window class): avoid flashing a console window when the
    // scanners-worker is spawned from a hook context (windowless parent).
    // `CREATE_NO_WINDOW` (0x08000000) is the canonical safety belt for
    // tool subprocesses on Windows. `tokio::process::Command` exposes
    // `creation_flags` as an inherent method on Windows (no
    // `CommandExt` import needed). See `windows_audit_subprocess_flags`.
    #[cfg(windows)]
    cmd.creation_flags(windows_audit_subprocess_flags());
    let mut child = cmd
        .spawn()
        .map_err(|e| CliError::Other(format!("failed to spawn {}: {e}", bin.display())))?;

    // B-003: register the child PID with the build registry so a
    // Ctrl-C will taskkill it. Tokio's `Child::id()` returns
    // `Option<u32>` — `None` only when the child has already exited
    // before we asked, which is fine (nothing to clean up). The
    // unregister at the end of this function lets a clean exit
    // remove the PID before the registry's Drop runs (a stale PID
    // is benign on cleanup, but unregistering avoids spurious
    // taskkill warnings on Windows).
    let pid = child.id();
    if let (Some(reg), Some(pid_v)) = (&registry, pid) {
        reg.register(pid_v);
    }

    // Send command, then close stdin so the worker stops reading.
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| CliError::Other("child stdin pipe missing".into()))?;
        stdin
            .write_all(cmd_line.as_bytes())
            .await
            .map_err(|e| CliError::Other(format!("write stdin failed: {e}")))?;
        stdin.flush().await.ok();
        // Drop stdin via take() to send EOF to the worker.
        drop(child.stdin.take());
    }

    // B12 (v0.3.2): open the per-project findings.db NOW, before we read
    // a single line of stdout, so the streaming loop can flush findings
    // incrementally. Pre-B12 the CLI accumulated every finding into a
    // Vec<Finding> and bulk-inserted at end-of-stream; if the subprocess
    // died (panic, kill, timeout) all findings were lost. Our AWS test fleet hit
    // this — 37,423 partial findings → 0 persisted on a wall-clock kill.
    let project_id = ProjectId::from_path(project)
        .map_err(|e| CliError::Other(format!("cannot hash project path: {e}")))?;
    let paths = PathManager::default_root();
    let findings_db = paths.shard_db(&project_id, DbLayer::Findings);
    let mut writer = match FindingsWriter::open(&findings_db) {
        Ok(w) => Some(w),
        Err(e) => {
            warn!(error = %e, db = %findings_db.display(),
                "findings.db open failed (continuing with print-only — findings will not persist)");
            None
        }
    };

    // Stream stdout under a per-line read budget. B11.8 (v0.3.2) removed
    // the outer wall-clock — `line_budget` is now the SOLE hang guard.
    // B12: pass the writer in so per-finding flushes happen during the
    // stream, not at the end. Pass severity_floor so flushed batches
    // are pre-filtered (matches the legacy bulk-write semantics).
    let line_budget = audit_line_budget();
    let stream_outcome =
        stream_scanner_output(&mut child, line_budget, writer.as_mut(), severity_floor).await?;

    let StreamOutcome {
        findings,
        summary,
        subprocess_error,
        timed_out,
        persisted,
    } = stream_outcome;

    // If the per-line hang guard fired we've already killed the child and
    // drained stderr. Surface a graceful Ok with a warning — the build can
    // still complete its other passes, and the user gets a clear log line
    // about WHY findings.db only has partial rows.
    if timed_out {
        // B12: persisted is non-zero even on timeout because we streamed
        // findings to the DB throughout the scan, not just at end. This
        // is the data-loss fix.
        warn!(
            line_budget_secs = line_budget.as_secs(),
            findings_persisted = persisted,
            "mneme-scanners subprocess hit per-line read timeout; killed and continuing \
             (set MNEME_AUDIT_LINE_TIMEOUT_SEC to override; partial findings ARE persisted via B12 streaming)"
        );
        // Best-effort unregister: the child has been killed so Drop won't
        // double-taskkill a still-running process, but a missed unregister
        // would just produce a benign "no such process" on cleanup.
        if let (Some(reg), Some(pid_v)) = (&registry, pid) {
            reg.unregister(pid_v);
        }
        return Ok(());
    }

    // Wait for the child to exit so we can collect its status. After a
    // clean stdout EOF the child is typically already reaping; this just
    // collects the exit code.
    let status = child
        .wait()
        .await
        .map_err(|e| CliError::Other(format!("subprocess wait failed: {e}")))?;

    // B-003: child has reaped cleanly — drop it from the kill list so
    // the registry's Drop guard doesn't taskkill an already-exited
    // PID. Best effort; a leftover PID is harmless on cleanup.
    if let (Some(reg), Some(pid_v)) = (&registry, pid) {
        reg.unregister(pid_v);
    }

    if !status.success() {
        // B-008: Drain the child's captured stderr so panic-abort crashes
        // (Windows 0xc0000409 / Unix SIGABRT) surface their actual panic
        // message + location in the build log. Before this, every scanner
        // panic produced an opaque "subprocess crashed" with the real
        // diagnostic discarded into /dev/null.
        let mut stderr_tail = String::new();
        if let Some(stderr) = child.stderr.take() {
            let mut buf = Vec::with_capacity(4096);
            let mut bufreader = BufReader::new(stderr);
            let drain = bufreader.read_to_end(&mut buf);
            let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
            if !buf.is_empty() {
                let snippet = String::from_utf8_lossy(&buf);
                let trimmed = snippet.trim();
                if !trimmed.is_empty() {
                    stderr_tail = tail_of(trimmed, 2048);
                }
            }
        }
        let detail = if stderr_tail.is_empty() {
            String::from(" (subprocess crashed; stderr empty)")
        } else {
            format!(" (subprocess crashed; stderr tail: {})", stderr_tail)
        };
        return Err(CliError::Other(format!(
            "mneme-scanners subprocess exited with status {status}{detail}"
        )));
    }

    if let Some(err) = subprocess_error {
        return Err(CliError::Other(format!("orchestrator error: {err}")));
    }

    // B12: severity-floor was applied during streaming, so `findings`
    // already matches `kept` (the streaming writer pre-filtered). We
    // rebind for the print_summary call.
    let kept = findings;
    let inserted = persisted;

    print_summary(&kept, summary.as_ref(), inserted, &findings_db);

    // Exit code: 1 if any critical findings remain after the filter, else 0.
    let has_critical = kept.iter().any(|f| f.severity == Severity::Critical);
    if has_critical {
        // Use Other(...) here — main.rs maps Other → exit 1, which
        // matches the contract.
        return Err(CliError::Other(format!(
            "audit found {} critical finding(s)",
            kept.iter()
                .filter(|f| f.severity == Severity::Critical)
                .count()
        )));
    }
    Ok(())
}

/// Outcome of streaming the scanner subprocess's stdout under a per-line
/// read budget. Carries whatever findings / summary / error markers we
/// managed to parse before EOF, plus a `timed_out` flag the caller uses
/// to decide whether to graceful-degrade (return Ok with a warn) or
/// proceed to `child.wait()` for the exit status.
///
/// B12 (v0.3.2): `persisted` tracks the running total of findings flushed
/// to findings.db during streaming. Non-zero even when `timed_out=true`
/// — that's the data-loss fix.
#[derive(Debug, Default)]
struct StreamOutcome {
    findings: Vec<Finding>,
    summary: Option<DoneSummary>,
    subprocess_error: Option<String>,
    /// True iff the per-line read budget fired and we killed the child.
    /// Means findings/summary may be partial. Caller should NOT propagate
    /// the child's exit status when this is set — we already taskkilled it.
    /// B11.8 (v0.3.2): only the per-line budget can set this now.
    timed_out: bool,
    /// B12: number of findings flushed to findings.db during streaming.
    persisted: usize,
}

/// Stream stdout from a spawned `mneme-scanners` child under a per-line
/// read budget AND incrementally flush findings to findings.db.
///
/// Per-line read budget (`line_budget`): each `reader.next_line()` awaits
/// for at most this long. A line-timeout IS fatal — we kill the child,
/// drain stderr, set `timed_out = true`, and return Ok so the caller can
/// graceful-degrade. Pre-B11.8 a line-timeout was non-fatal and the outer
/// wall-clock owned the kill decision; that decision lost work on
/// slow-but-working scans (our AWS test instance: 30 min single-threaded, killed at
/// 5 min, 37k findings dropped). The scanner subprocess emits a
/// `_progress` heartbeat every 25 files / 5 s (B-019), so any silence
/// longer than `line_budget` (default 30 s) is a true wedge.
///
/// Streaming writer (`writer`): when `Some`, every batch of findings is
/// flushed to disk after `FINDINGS_FLUSH_BUFFER` rows OR every
/// `FINDINGS_FLUSH_INTERVAL`, whichever comes first. This is the B12
/// data-loss fix — pre-B12 the CLI bulk-inserted at end-of-stream and
/// any kill / panic / EOF-error lost everything. Findings are
/// pre-filtered against `severity_floor` before flushing so the on-disk
/// shard matches the legacy bulk-write semantics.
///
/// Cleanup invariant: when this function returns with `timed_out = true`,
/// the child has been killed and reaped — the caller does NOT need to
/// (and SHOULD NOT) call `child.wait()` again. When `timed_out = false`
/// the child has reached stdout EOF naturally and the caller should
/// `child.wait()` to collect the exit status.
async fn stream_scanner_output(
    child: &mut TokioChild,
    line_budget: Duration,
    mut writer: Option<&mut FindingsWriter>,
    severity_floor: Severity,
) -> CliResult<StreamOutcome> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::Other("child stdout pipe missing".into()))?;
    let mut reader = BufReader::new(stdout).lines();

    let mut outcome = StreamOutcome::default();
    let mut buffer: Vec<Finding> = Vec::with_capacity(FINDINGS_FLUSH_BUFFER);
    let mut last_flush = Instant::now();

    loop {
        match tokio::time::timeout(line_budget, reader.next_line()).await {
            // Normal line read.
            Ok(Ok(Some(line))) => {
                let prev_len = outcome.findings.len();
                process_scanner_line(&line, &mut outcome);
                // B12: any newly-pushed findings are also pushed to the
                // flush buffer (post-severity-filter). The outcome.findings
                // accumulator stays in sync so callers / tests still see
                // every parsed finding; the buffer only carries kept rows.
                if outcome.findings.len() > prev_len {
                    for f in &outcome.findings[prev_len..] {
                        if severity_rank(f.severity) <= severity_rank(severity_floor) {
                            buffer.push(f.clone());
                        }
                    }
                    if buffer.len() >= FINDINGS_FLUSH_BUFFER {
                        flush_findings(writer.as_deref_mut(), &mut buffer, &mut outcome.persisted);
                        last_flush = Instant::now();
                    }
                }
                // B12: time-based flush even when buffer is small. Files
                // with no findings produce no buffer growth, but a kill
                // could still arrive — flush every 5 s.
                if last_flush.elapsed() >= FINDINGS_FLUSH_INTERVAL && !buffer.is_empty() {
                    flush_findings(writer.as_deref_mut(), &mut buffer, &mut outcome.persisted);
                    last_flush = Instant::now();
                }
            }
            // EOF — child closed stdout cleanly.
            Ok(Ok(None)) => break,
            // IO error reading the line (pipe broken, encoding error, …).
            // Treat the same as EOF — the child is gone or unreadable;
            // let the caller's wait() report the real status.
            Ok(Err(e)) => {
                debug!(error = %e, "scanner stdout read error; ending stream");
                break;
            }
            // Per-line timeout. B11.8 (v0.3.2): this is now FATAL — kill
            // the child, drain stderr, return Ok with timed_out=true.
            // Pre-B11.8 this was non-fatal and the outer wall-clock was
            // the kill trigger; that path killed slow-but-working scans.
            Err(_) => {
                warn!(
                    line_budget_secs = line_budget.as_secs(),
                    findings_persisted = outcome.persisted,
                    "mneme-scanners produced no output for the per-line budget; \
                     killing child (set MNEME_AUDIT_LINE_TIMEOUT_SEC to override)"
                );
                outcome.timed_out = true;
                break;
            }
        }
    }

    // B12: final flush — drain whatever is still buffered. On a clean EOF
    // this is the tail of the scan; on a timeout it is the last fragment
    // before we killed the child. Either way, persist before returning.
    if !buffer.is_empty() {
        flush_findings(writer.as_deref_mut(), &mut buffer, &mut outcome.persisted);
    }

    if outcome.timed_out {
        // Kill + reap. We use start_kill + wait rather than the awaited
        // `kill()` shortcut because we want to also drain stderr after
        // the process is dead but before we return — a dead child's
        // stderr pipe returns EOF quickly so this never re-introduces a
        // hang.
        if let Err(e) = child.start_kill() {
            warn!(error = %e, "Child::start_kill failed; child may already be exited");
        }
        // Wait with a short cap so we don't add another unbounded wait
        // — start_kill is essentially instant on Windows (TerminateProcess)
        // and Unix (SIGKILL), so 5s is generous.
        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(_status)) => { /* reaped */ }
            Ok(Err(e)) => warn!(error = %e, "child.wait after kill failed"),
            Err(_) => warn!("child.wait did not complete within 5s after kill"),
        }
        // Drain stderr best-effort for diagnostics. The child is dead so
        // the pipe will EOF quickly; we still cap the read with a short
        // timeout so we never undo the line-budget guarantee.
        if let Some(stderr) = child.stderr.take() {
            let mut buf = Vec::with_capacity(2048);
            let mut bufreader = BufReader::new(stderr);
            let drain = bufreader.read_to_end(&mut buf);
            let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
            if !buf.is_empty() {
                let snippet = String::from_utf8_lossy(&buf);
                let snippet = snippet.trim();
                if !snippet.is_empty() {
                    warn!(
                        stderr_bytes = buf.len(),
                        stderr_tail = %tail_of(snippet, 1024),
                        "mneme-scanners stderr (post-timeout drain)"
                    );
                }
            }
        }
    }

    Ok(outcome)
}

/// Parse a single scanner-stdout line, mutating the outcome accumulator.
/// Pulled out of [`stream_scanner_output`] so the read loop stays small
/// and so the line-parsing logic is independently testable.
fn process_scanner_line(line: &str, outcome: &mut StreamOutcome) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    // B-027 (D:\Mneme Dome cycle, 2026-05-01): scanner subprocess emits
    // periodic `{"_progress": true, ...}` heartbeat lines (B-019) so the
    // CLI's read-loop budget doesn't false-positive a hang on long
    // stretches of zero-finding files. They are NOT findings — skip them
    // before falling through to the Finding deserializer (which would
    // log "skipping malformed finding line" at debug for every beat).
    if trimmed.contains("\"_progress\"") {
        return;
    }
    // Try the summary marker first — it's the LAST line, so success
    // shortcircuits.
    if trimmed.starts_with("{\"_done\"") || trimmed.contains("\"_done\":true") {
        if let Ok(s) = serde_json::from_str::<DoneSummary>(trimmed) {
            outcome.summary = Some(s);
            return;
        }
    }
    if trimmed.contains("\"_error\"") {
        #[derive(serde::Deserialize)]
        struct ErrLine {
            #[serde(rename = "_error")]
            error: String,
        }
        if let Ok(e) = serde_json::from_str::<ErrLine>(trimmed) {
            outcome.subprocess_error = Some(e.error);
            return;
        }
    }
    match serde_json::from_str::<Finding>(trimmed) {
        Ok(f) => outcome.findings.push(f),
        Err(e) => {
            debug!(error = %e, line = %trimmed, "skipping malformed finding line");
        }
    }
}

/// B12 (v0.3.2): drain `buffer` into the persistent findings.db writer,
/// crediting `*persisted` with the number of rows successfully written.
/// `buffer` is always cleared on return — even on writer errors — so the
/// caller's flush schedule (size + time triggers) cannot get into a state
/// where the buffer never drains. A None writer (open() failed earlier)
/// is treated as a no-op drain.
///
/// Errors are logged at warn! and swallowed. Pre-B12 the bulk-write at
/// end-of-stream was best-effort with a warn-and-continue path, so this
/// preserves the same contract while moving the writes into the streaming
/// loop.
fn flush_findings(
    writer: Option<&mut FindingsWriter>,
    buffer: &mut Vec<Finding>,
    persisted: &mut usize,
) {
    if buffer.is_empty() {
        return;
    }
    if let Some(w) = writer {
        match w.write_findings(buffer) {
            Ok(n) => *persisted += n,
            Err(e) => {
                warn!(
                    error = %e,
                    batch_size = buffer.len(),
                    persisted_so_far = *persisted,
                    "streaming flush failed; dropping batch and continuing"
                );
            }
        }
    }
    buffer.clear();
}

/// Return the last `max` characters of `s` (best-effort UTF-8 boundary).
/// Used when emitting a stderr snippet so multi-MB scanners spam doesn't
/// drown the log line.
fn tail_of(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    // Walk forward to a char boundary so we don't slice mid-codepoint.
    let mut idx = start;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    s[idx..].to_string()
}

/// Pretty-print a per-scanner summary table.
///
/// Layout (fixed column widths for readability):
///
/// ```text
/// scanner       critical  error  warn  info  total
/// theme               0      0    37     2     39
/// security            1      4     0     0      5
/// ...
/// ```
fn print_summary(
    findings: &[Finding],
    summary: Option<&DoneSummary>,
    persisted: usize,
    findings_db: &Path,
) {
    let mut by_scanner: BTreeMap<&str, [usize; 4]> = BTreeMap::new();
    for f in findings {
        let scanner = scanner_for_rule(&f.rule_id);
        let cell = by_scanner.entry(scanner).or_insert([0; 4]);
        cell[severity_index(f.severity)] += 1;
    }

    println!();
    println!(
        "{:<14}{:>10}{:>8}{:>7}{:>7}{:>8}",
        "scanner", "critical", "error", "warn", "info", "total"
    );
    println!("{:-<54}", "");
    let mut total_total = 0usize;
    let mut total_per_sev = [0usize; 4];
    for (scanner, cells) in &by_scanner {
        let row_total: usize = cells.iter().sum();
        total_total += row_total;
        for (i, c) in cells.iter().enumerate() {
            total_per_sev[i] += c;
        }
        println!(
            "{:<14}{:>10}{:>8}{:>7}{:>7}{:>8}",
            scanner, cells[0], cells[1], cells[2], cells[3], row_total
        );
    }
    println!("{:-<54}", "");
    println!(
        "{:<14}{:>10}{:>8}{:>7}{:>7}{:>8}",
        "TOTAL",
        total_per_sev[0],
        total_per_sev[1],
        total_per_sev[2],
        total_per_sev[3],
        total_total
    );
    println!();
    if let Some(s) = summary {
        println!(
            "scanned {} files in {}ms ({} scanner errors)",
            s.scanned, s.duration_ms, s.errors
        );
    }
    println!(
        "{} findings persisted to {}",
        persisted,
        findings_db.display()
    );
}

/// Final stdout line emitted by the scanner subprocess in orchestrator mode.
#[derive(Debug, serde::Deserialize)]
struct DoneSummary {
    #[allow(dead_code)]
    #[serde(rename = "_done")]
    _done: bool,
    scanned: usize,
    #[allow(dead_code)]
    findings: usize,
    errors: usize,
    /// B-027 (2026-05-01 audit follow-up to B-019): files killed by the
    /// per-file 60s timeout. `#[serde(default)]` keeps backward-compat
    /// with older scanner subprocesses that don't emit this field.
    #[allow(dead_code)]
    #[serde(default)]
    timeouts: usize,
    duration_ms: u64,
}

/// Resolve `~/.mneme/bin/mneme-scanners[.exe]` first, then a developer
/// fallback to `target/release/mneme-scanners[.exe]`. The two paths are
/// mutually exclusive — the installed binary takes priority because in
/// release builds `target/` may be stale or missing.
fn resolve_scanners_binary() -> CliResult<PathBuf> {
    let exe_name = if cfg!(windows) {
        "mneme-scanners.exe"
    } else {
        "mneme-scanners"
    };
    // HOME-bypass-audit-resolver: route via PathManager so MNEME_HOME wins.
    let installed = common::paths::PathManager::default_root()
        .root()
        .join("bin")
        .join(exe_name);
    if installed.is_file() {
        return Ok(installed);
    }
    // Dev fallback: target/release/mneme-scanners[.exe] relative to the
    // workspace root. We try the same workspace this CLI was built in.
    let dev_candidates = [
        PathBuf::from("target").join("release").join(exe_name),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join(exe_name)))
            .unwrap_or_default(),
    ];
    for candidate in &dev_candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    Err(CliError::Other(format!(
        "could not find {exe_name} in ~/.mneme/bin or alongside the running binary; \
         install mneme via `mneme install` or build the workspace with `cargo build --release`"
    )))
}

/// Resolve `project` to an absolute, canonicalised path. Falls back to
/// CWD if the user passed nothing.
fn resolve_project(arg: Option<PathBuf>) -> CliResult<PathBuf> {
    let raw = arg.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let canonical = std::fs::canonicalize(&raw).unwrap_or(raw);
    Ok(canonical)
}

/// Parse `--severity` into a [`Severity`]. Accepts the canonical labels
/// (`critical|error|warn|warning|info`) plus a few synonyms.
fn parse_severity(s: &str) -> CliResult<Severity> {
    match s.to_ascii_lowercase().as_str() {
        "critical" | "crit" => Ok(Severity::Critical),
        "error" | "err" => Ok(Severity::Error),
        "warn" | "warning" => Ok(Severity::Warning),
        "info" | "i" => Ok(Severity::Info),
        other => Err(CliError::Other(format!(
            "invalid --severity {other:?}; expected critical|error|warn|info"
        ))),
    }
}

/// Validate `--scope`. Accepts only `full` or `diff` — the orchestrator
/// rejects anything else.
fn normalise_scope(s: &str) -> CliResult<&'static str> {
    match s.to_ascii_lowercase().as_str() {
        "full" | "all" => Ok("full"),
        "diff" => Ok("diff"),
        other => Err(CliError::Other(format!(
            "invalid --scope {other:?}; expected full|diff"
        ))),
    }
}

/// Stable rank for sorting + filtering (lower = more severe).
fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Critical => 0,
        Severity::Error => 1,
        Severity::Warning => 2,
        Severity::Info => 3,
    }
}

/// Column index into the per-scanner table (matches the header order
/// `critical, error, warn, info`).
fn severity_index(s: Severity) -> usize {
    match s {
        Severity::Critical => 0,
        Severity::Error => 1,
        Severity::Warning => 2,
        Severity::Info => 3,
    }
}

/// Strip the rule prefix to recover the scanner name. Mirrors
/// [`mneme_scanners::scanner_name_for_rule`] — kept inline so this
/// module's surface doesn't depend on the scanners crate's public
/// helper.
fn scanner_for_rule(rule_id: &str) -> &str {
    match rule_id.split_once('.') {
        Some((prefix, _)) if !prefix.is_empty() => prefix,
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_finding(rule_id: &str, sev: Severity, file: &str) -> Finding {
        Finding::new_line(rule_id, sev, file, 1, 0, 10, "msg".to_string())
    }

    #[test]
    fn parse_severity_accepts_canonical_labels() {
        assert_eq!(parse_severity("critical").unwrap(), Severity::Critical);
        assert_eq!(parse_severity("error").unwrap(), Severity::Error);
        assert_eq!(parse_severity("warn").unwrap(), Severity::Warning);
        assert_eq!(parse_severity("info").unwrap(), Severity::Info);
    }

    #[test]
    fn parse_severity_accepts_synonyms() {
        assert_eq!(parse_severity("warning").unwrap(), Severity::Warning);
        assert_eq!(parse_severity("crit").unwrap(), Severity::Critical);
        assert_eq!(parse_severity("ERR").unwrap(), Severity::Error);
    }

    #[test]
    fn parse_severity_rejects_unknown() {
        assert!(parse_severity("urgent").is_err());
        assert!(parse_severity("").is_err());
    }

    #[test]
    fn normalise_scope_canonical() {
        assert_eq!(normalise_scope("full").unwrap(), "full");
        assert_eq!(normalise_scope("diff").unwrap(), "diff");
        assert_eq!(normalise_scope("ALL").unwrap(), "full");
    }

    #[test]
    fn normalise_scope_rejects_unknown() {
        assert!(normalise_scope("incremental").is_err());
    }

    #[test]
    fn severity_filter_keeps_at_or_above_floor() {
        let findings = vec![
            mk_finding("a.x", Severity::Critical, "a.ts"),
            mk_finding("a.x", Severity::Error, "a.ts"),
            mk_finding("a.x", Severity::Warning, "a.ts"),
            mk_finding("a.x", Severity::Info, "a.ts"),
        ];
        let floor = Severity::Warning;
        let kept: Vec<_> = findings
            .into_iter()
            .filter(|f| severity_rank(f.severity) <= severity_rank(floor))
            .collect();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].severity, Severity::Critical);
        assert_eq!(kept[1].severity, Severity::Error);
        assert_eq!(kept[2].severity, Severity::Warning);
    }

    #[test]
    fn scanner_for_rule_extracts_prefix() {
        assert_eq!(scanner_for_rule("theme.hardcoded-hex"), "theme");
        assert_eq!(scanner_for_rule("security.eval"), "security");
        assert_eq!(scanner_for_rule("a11y.img-no-alt"), "a11y");
        assert_eq!(scanner_for_rule(""), "unknown");
        assert_eq!(scanner_for_rule("no-dot"), "unknown");
    }

    #[test]
    fn done_summary_round_trips() {
        let line = r#"{"_done":true,"scanned":42,"findings":7,"errors":1,"duration_ms":1234}"#;
        let s: DoneSummary = serde_json::from_str(line).unwrap();
        assert_eq!(s.scanned, 42);
        assert_eq!(s.errors, 1);
        assert_eq!(s.duration_ms, 1234);
    }

    #[test]
    fn finding_round_trips_via_jsonl() {
        let f = mk_finding("theme.hardcoded-hex", Severity::Warning, "x.tsx");
        let line = serde_json::to_string(&f).unwrap();
        let back: Finding = serde_json::from_str(&line).unwrap();
        assert_eq!(back.rule_id, "theme.hardcoded-hex");
        assert_eq!(back.severity, Severity::Warning);
    }

    #[test]
    fn resolve_scanners_binary_returns_path_or_err() {
        // We cannot guarantee a binary exists in CI's test sandbox, but
        // we CAN guarantee the function never panics. A successful
        // result must point at an existing file.
        match resolve_scanners_binary() {
            Ok(p) => assert!(p.is_file(), "returned non-file path: {}", p.display()),
            Err(_) => { /* acceptable in environments where neither path exists */ }
        }
    }

    // ------------------------------------------------------------------
    // B11.7/B11.8/B12 (v0.3.2 D:\Mneme Dome cycle, 2026-05-02): the
    // per-line read budget is now the SOLE hang guard for the scanner
    // subprocess streaming loop (B11.8 removed the outer wall-clock
    // because it killed slow-but-working scans on large projects, losing
    // 37k partial findings on our AWS test instance). The two tests below cover the
    // remaining states of the budget interaction:
    //
    //   1. Per-line fires → child killed immediately, graceful Ok outcome
    //      with timed_out=true (this is the new fatal-on-line-timeout
    //      contract; pre-B11.8 it was non-fatal).
    //   2. Child exits fast → no timeout fires, outcome carries clean EOF.
    // ------------------------------------------------------------------

    /// Spawn a hung child that produces no stdout for the lifetime of the
    /// test. Cross-platform: PowerShell on Windows (always present on
    /// Windows 11), `sh -c sleep` on Unix. Returns a tokio `Child` with
    /// piped stdin/stdout/stderr to mirror the production spawn shape.
    fn spawn_hung_child(seconds: u32) -> TokioChild {
        let mut cmd = if cfg!(windows) {
            let mut c = TokioCommand::new("powershell");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Start-Sleep -Seconds {seconds}"),
            ]);
            c
        } else {
            let mut c = TokioCommand::new("sh");
            c.args(["-c", &format!("sleep {seconds}")]);
            c
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hung child")
    }

    /// Spawn a child that exits cleanly with EOF in roughly 1s. Used to
    /// verify the streaming loop completes naturally when no timeout
    /// fires. The exact output content is unimportant — we just need
    /// stdout to close fast.
    fn spawn_fast_exit_child() -> TokioChild {
        let mut cmd = if cfg!(windows) {
            let mut c = TokioCommand::new("cmd");
            c.args(["/c", "exit", "0"]);
            c
        } else {
            let mut c = TokioCommand::new("sh");
            c.args(["-c", "exit 0"]);
            c
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn fast-exit child")
    }

    /// B11.8 (v0.3.2): the per-line read budget is now the SOLE hang
    /// guard, and a line-timeout is FATAL — `stream_scanner_output`
    /// kills the hung child and returns a graceful `Ok` with
    /// `timed_out = true`. Caller can then surface a `tracing::warn!`
    /// and continue the build instead of hanging for ~50 minutes the
    /// way EC2 2026-04-27 19:00 demonstrated. Pre-B11.8 this contract
    /// was owned by an outer wall-clock; that path killed slow-but-
    /// working scans on large projects (observed on our AWS test instance) so we removed it.
    #[tokio::test]
    async fn audit_subprocess_timeout_kills_hung_child() {
        // Spawn a child that would sleep for 30s. Set the line budget
        // very tight (200ms) so the test runs fast — the line-timeout
        // alone is now what kills the child.
        let mut child = spawn_hung_child(30);
        let pid = child.id().expect("child must report pid before exit");
        let line = Duration::from_millis(200);

        let test_started = Instant::now();
        let outcome = stream_scanner_output(&mut child, line, None, Severity::Info)
            .await
            .expect("stream_scanner_output must return Ok even on timeout");
        let elapsed = test_started.elapsed();

        assert!(
            outcome.timed_out,
            "outcome.timed_out must be set when the per-line budget fires"
        );
        assert!(
            outcome.findings.is_empty(),
            "no stdout was produced; findings must be empty"
        );
        // line + 5s kill+drain slack — should comfortably finish under
        // 30s, in practice <2s. We allow generous slack to absorb any
        // PowerShell startup tax on cold Windows boxes.
        assert!(
            elapsed < Duration::from_secs(30),
            "stream_scanner_output must return well under 30s on line timeout; took {elapsed:?}"
        );

        // The child must be reaped by the time we returned. `try_wait`
        // returns `Ok(Some(_))` for a reaped child. We poll briefly to
        // absorb any platform-specific reap latency after start_kill.
        let mut reaped = false;
        for _ in 0..50 {
            match child.try_wait() {
                Ok(Some(_)) => {
                    reaped = true;
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(_) => break,
            }
        }
        assert!(
            reaped,
            "hung child PID {pid} must be reaped after stream_scanner_output returns timed_out=true"
        );
    }

    /// Sanity: when the child completes well within the line budget, no
    /// timeout fires and the outcome carries `timed_out = false`. This
    /// guards against an over-zealous line budget that would treat every
    /// audit as "hung" even when the worker behaves correctly.
    #[tokio::test]
    async fn audit_subprocess_completes_normally_when_fast() {
        let mut child = spawn_fast_exit_child();
        // Generous line budget so cmd / sh startup easily finishes
        // before it fires.
        let line = Duration::from_secs(5);

        let outcome = stream_scanner_output(&mut child, line, None, Severity::Info)
            .await
            .expect("stream_scanner_output must return Ok on a clean fast exit");

        assert!(
            !outcome.timed_out,
            "timed_out must NOT be set when the child exits cleanly"
        );
        // No JSON was produced, so findings + summary + error are empty.
        assert!(
            outcome.findings.is_empty(),
            "no stdout was produced; findings must be empty"
        );
        assert!(outcome.summary.is_none(), "no _done line was produced");
        assert!(
            outcome.subprocess_error.is_none(),
            "no _error line was produced"
        );
        assert_eq!(
            outcome.persisted, 0,
            "no findings were emitted, so persisted must be 0"
        );

        // Reap so we don't leak.
        let _ = child.wait().await;
    }

    /// `parse_env_secs_or` accepts positive integers and rejects 0 and
    /// junk. The zero-rejection is load-bearing: a zero would make the
    /// line timeout fire instantly on every audit, breaking every build
    /// for any user who set the env var to "disable" the timeout. We
    /// force them onto the default instead.
    #[test]
    fn audit_env_overrides_reject_zero_and_junk() {
        let fb = Duration::from_secs(42);
        // Use scoped env writes inside a single-threaded context. These
        // tests share process state with other env reads but the names
        // are unique.
        std::env::set_var("MNEME_TEST_PROBE_OUTER", "120");
        assert_eq!(
            parse_env_secs_or("MNEME_TEST_PROBE_OUTER", fb),
            Duration::from_secs(120)
        );

        std::env::set_var("MNEME_TEST_PROBE_OUTER", "0");
        assert_eq!(
            parse_env_secs_or("MNEME_TEST_PROBE_OUTER", fb),
            fb,
            "zero must fall back — never disable the timeout"
        );

        std::env::set_var("MNEME_TEST_PROBE_OUTER", "not-a-number");
        assert_eq!(parse_env_secs_or("MNEME_TEST_PROBE_OUTER", fb), fb);

        std::env::remove_var("MNEME_TEST_PROBE_OUTER");
        assert_eq!(parse_env_secs_or("MNEME_TEST_PROBE_OUTER", fb), fb);
    }

    /// `process_scanner_line` correctly classifies the four line shapes
    /// the scanner emits: blank (skip), `_done` summary, `_error` line,
    /// finding line. We exercise it directly so the timeout helper's
    /// outcome accumulator stays small and tested.
    #[test]
    fn process_scanner_line_classifies_all_known_shapes() {
        let mut outcome = StreamOutcome::default();

        // Blank lines are silently skipped.
        process_scanner_line("", &mut outcome);
        process_scanner_line("   \t  ", &mut outcome);
        assert!(outcome.findings.is_empty());
        assert!(outcome.summary.is_none());
        assert!(outcome.subprocess_error.is_none());

        // _done summary.
        process_scanner_line(
            r#"{"_done":true,"scanned":3,"findings":1,"errors":0,"duration_ms":42}"#,
            &mut outcome,
        );
        let s = outcome.summary.as_ref().expect("summary must parse");
        assert_eq!(s.scanned, 3);

        // _error line.
        process_scanner_line(r#"{"_error":"boom"}"#, &mut outcome);
        assert_eq!(outcome.subprocess_error.as_deref(), Some("boom"));

        // A real finding.
        let f = mk_finding("theme.x", Severity::Warning, "foo.ts");
        let line = serde_json::to_string(&f).unwrap();
        process_scanner_line(&line, &mut outcome);
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].rule_id, "theme.x");
    }

    /// `tail_of` returns the last `max` chars of a string, respecting
    /// UTF-8 boundaries. Used to keep stderr-snippet log lines bounded.
    #[test]
    fn tail_of_respects_max_and_utf8_boundary() {
        // Short string passes through unchanged.
        assert_eq!(tail_of("hello", 100), "hello");
        // Long ASCII string is truncated to last `max` chars.
        let long = "a".repeat(200);
        let tail = tail_of(&long, 64);
        assert_eq!(tail.len(), 64);
        // Multi-byte UTF-8: must not split mid-codepoint.
        let s = format!("{}漢字", "x".repeat(60));
        // Each kanji is 3 bytes in UTF-8. Cap at 5 → must align to a
        // boundary ≥ 5, never produce invalid UTF-8.
        let tail = tail_of(&s, 5);
        assert!(tail.is_char_boundary(0));
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
    }

    /// Bug M9 (D-window class): the scanners-worker subprocess spawned
    /// from `run_direct_subprocess` must include the Windows
    /// `CREATE_NO_WINDOW` flag (`0x08000000`). When `mneme audit` is
    /// invoked from a hook context (windowless parent), a missing flag
    /// flashes a console window for the duration of the scanner pass.
    /// The fix exposes a pure-Rust `windows_audit_subprocess_flags()`
    /// helper that returns the canonical flag bitfield; this test pins
    /// the contract so future edits cannot silently regress it.
    #[test]
    fn windows_audit_subprocess_flags() {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let flags = super::windows_audit_subprocess_flags();
        assert_eq!(
            flags & CREATE_NO_WINDOW,
            CREATE_NO_WINDOW,
            "audit scanners-worker spawn must set CREATE_NO_WINDOW (0x08000000); got {flags:#010x}"
        );
    }

    /// B12 (v0.3.2): `flush_findings` writes the buffered findings to
    /// the FindingsWriter, increments the `persisted` counter, and
    /// CLEARS the buffer — even on writer error — so the streaming
    /// caller's flush schedule (size + time triggers) cannot get into
    /// a state where the buffer never drains. This is the data-loss
    /// fix's invariant.
    #[test]
    fn flush_findings_writes_and_clears_buffer() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("findings.db");
        let mut writer = FindingsWriter::open(&path).unwrap();
        let mut buffer = vec![
            mk_finding("theme.hardcoded-hex", Severity::Warning, "a.tsx"),
            mk_finding("security.eval", Severity::Critical, "b.ts"),
            mk_finding("a11y.alt", Severity::Error, "c.tsx"),
        ];
        let mut persisted = 0usize;

        flush_findings(Some(&mut writer), &mut buffer, &mut persisted);

        assert_eq!(persisted, 3, "all 3 findings must be persisted");
        assert!(buffer.is_empty(), "buffer must be cleared after flush");

        // Second flush of the cleared buffer is a no-op.
        flush_findings(Some(&mut writer), &mut buffer, &mut persisted);
        assert_eq!(persisted, 3, "no-op flush must not change persisted");

        // Verify rows actually landed in the DB.
        let count = writer.open_findings_count().unwrap();
        assert_eq!(count, 3);
    }

    /// B12: a None writer (open() failed earlier) must not panic; it
    /// drains the buffer silently and leaves `persisted` unchanged.
    /// This preserves the legacy "warn-and-continue" contract.
    #[test]
    fn flush_findings_no_writer_drains_silently() {
        let mut buffer = vec![mk_finding("theme.x", Severity::Info, "a.ts")];
        let mut persisted = 0usize;

        flush_findings(None, &mut buffer, &mut persisted);

        assert_eq!(persisted, 0, "None writer leaves persisted at zero");
        assert!(
            buffer.is_empty(),
            "buffer must be cleared even when the writer is None"
        );
    }
}
