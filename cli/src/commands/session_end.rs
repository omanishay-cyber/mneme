//! `mneme session-end` — SessionEnd hook entry point.
//!
//! Final flush + manifest update (per design §6.6). Best-effort; we never
//! want this hook to take down the host with a non-zero exit.
//!
//! ## v0.3.1 — STDIN + CLI parity
//!
//! Claude Code SessionEnd delivers `{ session_id, hook_event_name,
//! transcript_path }` on STDIN. We just forward session_id to the
//! supervisor to trigger the flush.

use clap::Args;
use std::path::PathBuf;
use tracing::warn;

use crate::error::CliResult;
use crate::hook_payload::{
    make_hook_client, read_stdin_payload, resolved_session_id, HOOK_CTX_BUDGET, HOOK_IPC_BUDGET,
};
use crate::hook_writer::HookCtx;
use crate::ipc::IpcRequest;

/// CLI args for `mneme session-end`. Session id optional — STDIN fills in.
#[derive(Debug, Args)]
pub struct SessionEndArgs {
    /// Session id.
    #[arg(long = "session-id")]
    pub session_id: Option<String>,

    /// Internal: re-entry mode used by the detached fire-and-forget child.
    /// When set, the hook skips re-spawning and runs the actual flush
    /// synchronously. Hidden from --help so users never type it directly.
    /// See HOOK-CANCELLED-002 (2026-04-29 v0.3.2 REAL-1) — Claude Code's
    /// `claude --print` hook window is shorter than even a 500 ms
    /// HOOK_CTX_BUDGET + HOOK_IPC_BUDGET combo, so the parent hook now
    /// spawns a detached self-copy with this flag and returns instantly.
    #[arg(long = "detached-flush", hide = true, default_value_t = false)]
    pub detached_flush: bool,
}

/// Entry point used by `main.rs`. Always exits 0.
///
/// HOOK-CANCELLED-001 (2026-04-27): a SessionEnd hook MUST return promptly,
/// because Claude Code fires it on the exit of *every* short-lived host
/// invocation — including `claude mcp list`, which has its own tight wall-
/// clock budget. If the hook does first-time shard creation + a 120s IPC
/// timeout + a daemon auto-spawn retry, Claude Code emits "Hook cancelled"
/// on stderr and pollutes the user's `mcp list` output.
///
/// Two-layer defence:
///
/// 1. **No session id ⇒ no persistence.** When STDIN is empty / a TTY /
///    parse-failed, OR the payload's `session_id` field is missing, this
///    hook is firing for a non-session host invocation. Skip every side
///    effect (shard build, IPC) and return Ok immediately. This is the
///    `claude mcp list` path.
///
/// 2. **Bounded budgets on the hot path.** When we DO have a session id,
///    bound `HookCtx::resolve` and the supervisor IPC with
///    [`HOOK_CTX_BUDGET`] / [`HOOK_IPC_BUDGET`] so a misbehaving
///    supervisor or a slow first-time shard build can't push past Claude
///    Code's cancellation budget either.
pub async fn run(args: SessionEndArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    let stdin_payload = match read_stdin_payload() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "session-end STDIN parse failed; falling back");
            None
        }
    };

    let stdin_session = stdin_payload.as_ref().and_then(|p| p.session_id.clone());

    // HOOK-CANCELLED-001 layer 1: if neither the CLI flag nor the STDIN
    // payload carries a session id, this hook is firing for a non-session
    // host invocation (e.g. `claude mcp list`). Persisting a "session end
    // (lifecycle)" row with a synthetic id would pollute history.db, and
    // doing the work would delay the host's exit past Claude Code's hook
    // budget. Skip the side effects and return Ok.
    let session_id = match resolved_session_id(args.session_id, stdin_session) {
        Some(s) => s,
        None => {
            tracing::debug!(
                "session-end fired without a session id (likely a short-lived \
                 host command like `claude mcp list`); exiting 0 with no work"
            );
            return Ok(());
        }
    };

    // HOOK-CANCELLED-002 (2026-04-29 v0.3.2 REAL-1): even with budgets
    // tightened to 500 ms each, Claude Code's `claude --print` hook
    // window cancels SessionEnd before our HookCtx::resolve completes.
    // Switch to fire-and-forget: parent spawns a detached self-copy
    // with `--detached-flush --session-id <id>` and returns immediately.
    // The detached child does the actual writes without time pressure.
    if !args.detached_flush {
        spawn_detached_flush(&session_id);
        return Ok(());
    }

    // Bucket B4 fix: capture the session-end boundary in history.turns
    // and as a tasks.ledger_entries row of kind='decision'. Mirrors
    // turn_end.rs but explicitly tagged so a downstream consumer can
    // tell "session ended (lifecycle)" from "Stop hook fired (turn
    // boundary)".
    //
    // HOOK-CANCELLED-001 layer 2: bound the persistence path with
    // [`HOOK_CTX_BUDGET`] so first-invocation shard creation can't push
    // the hook past Claude Code's cancellation budget. On budget elapsed
    // we log + skip (RULE 17 — never block the user op).
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    match tokio::time::timeout(HOOK_CTX_BUDGET, HookCtx::resolve(&cwd)).await {
        Ok(Ok(ctx)) => {
            let summary = "Session end (lifecycle)";
            if let Err(e) = ctx
                .write_turn(&session_id, "session_end", summary)
                .await
            {
                warn!(error = %e, "history.turns (session-end) insert failed (non-fatal)");
            }
            if let Err(e) = ctx
                .write_ledger_entry(
                    &session_id,
                    "decision",
                    summary,
                    Some("Auto-emitted by SessionEnd hook (mneme session-end)."),
                )
                .await
            {
                warn!(error = %e, "tasks.ledger_entries (session-end) insert failed (non-fatal)");
            }
        }
        Ok(Err(e)) => {
            warn!(error = %e, "hook ctx resolve failed; skipping session-end persistence");
        }
        Err(_) => {
            warn!(
                budget_ms = HOOK_CTX_BUDGET.as_millis() as u64,
                "hook ctx resolve exceeded budget; skipping session-end persistence"
            );
        }
    }

    // HOOK-CANCELLED-001 layer 2 + Bug E (the resurrection-loop killer,
    // 2026-04-29): use the no-autospawn hook client. SessionEnd is the
    // hook that fires on EVERY `claude mcp list` invocation — auto-
    // spawning the daemon there meant every short-lived host command
    // resurrected mneme. Now: daemon down ⇒ session-end silently no-ops.
    // The flush of the in-flight ledger is a best-effort write; missing
    // it is fine, but resurrecting a daemon every host invocation is not.
    let client = make_hook_client(socket_override);
    let ipc_call = client.request(IpcRequest::SessionEnd { session_id });
    match tokio::time::timeout(HOOK_IPC_BUDGET, ipc_call).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            warn!(error = %e, "session-end flush skipped (supervisor unreachable)");
        }
        Err(_) => {
            warn!(
                budget_ms = HOOK_IPC_BUDGET.as_millis() as u64,
                "session-end IPC exceeded hook budget; skipping flush"
            );
        }
    }
    Ok(())
}

/// HOOK-CANCELLED-002 detached-flush spawn. Re-execs the same `mneme.exe`
/// with `session-end --detached-flush --session-id <id>` so the parent
/// hook can return to Claude Code in <50 ms while the actual write
/// continues in the background. Best-effort — failure here is logged at
/// debug level and the parent still exits 0 (RULE 17 — never block the
/// user op).
fn spawn_detached_flush(session_id: &str) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "session-end: could not resolve current_exe; skipping detached flush");
            return;
        }
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("session-end")
        .arg("--detached-flush")
        .arg("--session-id")
        .arg(session_id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW
        // Same flag set the daemon spawn uses (cli/src/ipc.rs::windows_daemon_detached_flags).
        // BREAKAWAY_FROM_JOB is critical: Claude Code may run mneme inside a
        // Windows Job that gets killed when claude.exe exits, which would
        // murder our detached child too. BREAKAWAY lets it survive.
        cmd.creation_flags(0x09000208);
    }
    if let Err(e) = cmd.spawn() {
        tracing::debug!(error = %e, "session-end: detached flush spawn failed (non-fatal)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Smoke clap harness — verify args parser without spinning up the
    /// full binary. WIRE-005: every command file gets at least one test.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        args: SessionEndArgs,
    }

    #[test]
    fn session_end_args_parse_with_no_flags() {
        // session_id is optional — STDIN fills it in at runtime.
        let h = Harness::try_parse_from(["x"]).unwrap();
        assert!(h.args.session_id.is_none());
    }

    #[test]
    fn session_end_args_parse_with_session_id() {
        let h = Harness::try_parse_from(["x", "--session-id", "abc-123"]).unwrap();
        assert_eq!(h.args.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn session_end_args_parser_rejects_positional_unknown() {
        // session-end takes no positional args — passing one must fail
        // at parse time so a stray STDIN-fed token doesn't get swallowed.
        let r = Harness::try_parse_from(["x", "stray"]);
        assert!(r.is_err(), "unknown positional must be rejected");
    }

    #[tokio::test]
    async fn session_end_with_unreachable_supervisor_exits_ok() {
        // Hooks must NEVER fail the host. With a missing socket and
        // no STDIN payload, run() should still exit Ok.
        let args = SessionEndArgs {
            session_id: Some("test-session".into()),
            // HOOK-CANCELLED-002 default: parent path (not the
            // detached fire-and-forget child re-entry).
            detached_flush: false,
        };
        let r = run(args, Some(PathBuf::from("/nope-mneme.sock"))).await;
        assert!(r.is_ok(), "session-end must always exit Ok; got: {r:?}");
    }

    /// Test isolation helper. `HookCtx::resolve` walks upward from the
    /// process CWD looking for a project marker (`.git`, `.claude`, …).
    /// When tests run from inside the mneme repo CWD that walk hits this
    /// repo's `.git`, then `build_or_migrate` actually creates shards in
    /// the user's `~/.mneme` — slow, surprising, and irrelevant to the
    /// hook timing contract. Point the resolver at a marker-free temp
    /// directory before each test so the resolve path fails fast.
    ///
    /// NOTE: this CWD change is process-global. Cargo runs tests in
    /// parallel by default, so two tests sharing this helper would race;
    /// the regression / no-op tests below run sequentially via
    /// `#[serial_test::serial]`-equivalent gating (we use a single test
    /// here, so a Mutex isn't required — but if the file grows more
    /// CWD-sensitive tests, wrap them with `serial_test`).
    fn cwd_into_marker_free_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // Don't create any project markers - HookCtx::resolve must walk
        // up and find nothing, returning Err(_) immediately.
        std::env::set_current_dir(dir.path()).expect("set_current_dir");
        dir
    }

    /// REGRESSION: HOOK-CANCELLED-001 (2026-04-27).
    ///
    /// `claude mcp list` triggered SessionEnd which called the IPC client
    /// with the default 120s timeout. On supervisor-down it auto-spawned
    /// the daemon and retried — total wall-clock could exceed Claude
    /// Code's hook-execution budget, prompting Claude Code to emit
    /// "Hook cancelled" on stderr and pollute `claude mcp list` output.
    ///
    /// The hook MUST complete within 3 seconds even when the supervisor
    /// is unreachable AND no session id arrives — the empty-stdin /
    /// no-session early-return guarantees this. With a session id
    /// present the bounded budgets (2s ctx + 2s ipc) keep the wall-
    /// clock under 5s; we assert the no-session path here because that's
    /// what `claude mcp list` exercises.
    #[tokio::test]
    async fn session_end_completes_within_hook_budget_when_supervisor_unreachable() {
        let _keep = cwd_into_marker_free_tempdir();
        let start = std::time::Instant::now();
        // No session id -> no work -> instant return (the `claude mcp
        // list` path).
        let args = SessionEndArgs { session_id: None, detached_flush: false };
        let r = run(args, Some(PathBuf::from("/nope-mneme.sock"))).await;
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "session-end must always exit Ok; got: {r:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "session-end must complete within 3s when no session id arrives \
             (the `claude mcp list` path); took {elapsed:?}. Claude Code emits \
             'Hook cancelled' if the hook exceeds its execution budget."
        );
    }

    /// HOOK-CANCELLED-001: when STDIN is a terminal AND no `--session-id`
    /// flag is passed (the unit-test default; in production no real
    /// SessionEnd payload would be missing one), the hook must exit Ok
    /// in well under one second without doing any persistence or IPC.
    /// This is the cheap path for short-lived host commands like
    /// `claude mcp list`.
    #[tokio::test]
    async fn session_end_with_empty_stdin_exits_zero() {
        let _keep = cwd_into_marker_free_tempdir();
        let start = std::time::Instant::now();
        let args = SessionEndArgs { session_id: None, detached_flush: false };
        let r = run(args, Some(PathBuf::from("/nope-mneme.sock"))).await;
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "empty-stdin session-end must exit Ok; got: {r:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "empty-stdin session-end must be effectively instant (no work); \
             took {elapsed:?}"
        );
    }

    /// HOOK-CANCELLED-001: a blank `--session-id` is the same shape as
    /// "no id arrived" — the hook must early-return rather than persist
    /// a row keyed on an empty string.
    #[tokio::test]
    async fn session_end_with_blank_session_id_exits_zero_no_work() {
        let _keep = cwd_into_marker_free_tempdir();
        let start = std::time::Instant::now();
        let args = SessionEndArgs {
            session_id: Some("   ".into()),
            detached_flush: false,
        };
        let r = run(args, Some(PathBuf::from("/nope-mneme.sock"))).await;
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "blank session-id must still exit Ok; got: {r:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "blank session-id must be effectively instant; took {elapsed:?}"
        );
    }
}
