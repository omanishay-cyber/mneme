//! Shared STDIN payload parsing for every hook entry point.
//!
//! Claude Code (and every other AI platform that supports hooks in the MCP
//! ecosystem) delivers hook payloads as a single JSON object written to
//! the hook binary's STDIN. The binary is expected to:
//!
//!   * read the entire STDIN,
//!   * parse a JSON object (UTF-8, no framing),
//!   * exit 0 to allow the underlying operation (or emit any STDOUT the
//!     host's hook protocol expects),
//!   * exit non-zero ONLY to block the operation.
//!
//! Before v0.3.1 mneme's hook binaries required `--tool / --params /
//! --session-id / --prompt / --cwd` as CLI flags. Claude Code never
//! passes flags — it passes JSON on STDIN. The result was the self-trap
//! documented in `report-002.md §F-012` / `txt proof.txt lines 426+,
//! 6369-6393`: every invocation exited non-zero with "required arguments
//! not provided", Claude Code interpreted that as BLOCK, every tool call
//! was denied, and the user was muted at the UserPromptSubmit hook.
//!
//! This module supplies:
//!
//!   * [`HookPayload`]   — the shape Claude Code writes on STDIN
//!   * [`read_stdin_payload`] — read + parse, TTY-aware
//!   * [`choose`]        — helper combinator: CLI flag wins over STDIN
//!                         field wins over default
//!
//! Individual hook commands call `read_stdin_payload()` once at the top,
//! then resolve each field via `choose(args.foo, payload.foo, default)`.
//! CLI flags retain their priority so manual testing (`mneme pre-tool
//! --tool Bash --params '{}' --session-id t`) still works.

use serde::Deserialize;
use serde_json::Value;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::time::Duration;

use crate::ipc::IpcClient;

/// HOOK-CANCELLED-001 (2026-04-27, tightened 2026-04-29 v0.3.2 REAL-1):
/// hard ceiling on every hook IPC call so a misbehaving / unreachable
/// supervisor never pushes the hook past Claude Code's hook-execution
/// budget. The default `IpcClient` timeout is 120 s plus a
/// daemon-auto-spawn retry — both fine for `mneme build` / `mneme
/// recall`, but a catastrophe for short-lived host invocations like
/// `claude mcp list` AND `claude --print` where Claude Code emits
/// "Hook cancelled" on stderr if the hook doesn't return promptly.
///
/// Tightened from 2 s → 500 ms after the v0.3.2 EC2 REAL-1 acceptance
/// run observed `SessionEnd hook ... failed: Hook cancelled` on every
/// `claude --print` invocation: cumulative HOOK_CTX_BUDGET (2s) +
/// HOOK_IPC_BUDGET (2s) = up to 4 s, exceeding Claude Code's
/// short-session hook window. 500 ms each → 1 s cumulative max →
/// always returns inside any reasonable hook budget. Misses on the
/// VERY FIRST session-end (before shards are built) are acceptable
/// — the install pipeline already eagerly creates ~/.mneme structure,
/// and subsequent invocations are well under 100 ms.
pub const HOOK_IPC_BUDGET: Duration = Duration::from_millis(500);

/// HOOK-CANCELLED-001 sibling ceiling for `HookCtx::resolve` — the
/// project-root walk + lazy `build_or_migrate` shard creation. First-
/// invocation shard creation can take seconds on Windows (26 SQLite
/// opens + pragma applies + schema execs); on a host command that
/// fires SessionEnd on exit the user would never see those rows
/// anyway. Bound the wall-clock so the hook always returns 0 promptly.
///
/// Tightened from 2 s → 500 ms in v0.3.2 (2026-04-29) — see
/// `HOOK_IPC_BUDGET` rationale above. The first-invocation skip is
/// not a regression: `mneme install` creates the per-project shards
/// the moment a user runs `mneme build .` for the first time; the
/// SessionEnd lifecycle marker is pure best-effort observability.
pub const HOOK_CTX_BUDGET: Duration = Duration::from_millis(500);

/// Bug E (the resurrection-loop killer, 2026-04-29): build the
/// no-autospawn IPC client every hook command must use.
///
/// ## Why hooks must NEVER auto-spawn
///
/// Hooks fire on every Claude Code tool call (PreToolUse, PostToolUse,
/// UserPromptSubmit, Stop, SubagentStop, SessionStart, SessionEnd,
/// PreCompact). Each hook is a short-lived `mneme <hook>` invocation;
/// the supervisor not running is the steady state when the user has
/// not yet run `mneme daemon start`.
///
/// Pre-fix: every hook used the default [`IpcClient`] which on connect-
/// failure called `spawn_daemon_detached()` and then waited up to 3 s
/// for the daemon to come up. Combined with Bug D (worker spawns
/// flashing visible cmd.exe windows), the result on our AWS test host
/// was 22 cmd windows appearing on every tool call — the "hydra heads"
/// from postmortem 2026-04-29 §3.E + §12.5.
///
/// ## The new contract
///
/// **Daemon down ⇒ hook silently no-ops.** No spawn, no warning to
/// the user, no `<mneme-status>` sermon. Mneme is intentionally
/// inactive when the supervisor is not running; the user runs
/// `mneme daemon start` to activate context capture. Hooks must NOT
/// ambush the user with a daemon they didn't ask for.
///
/// The returned client:
/// 1. Has [`IpcClient::with_no_autospawn`] set, so connect failure
///    returns `Err(CliError::Ipc)` immediately — no
///    `spawn_daemon_detached()`, no 3 s `wait_for_supervisor` poll.
/// 2. Has the [`HOOK_IPC_BUDGET`] timeout applied so a wedged-but-up
///    supervisor still can't push the hook past Claude Code's
///    "Hook cancelled" wall-clock.
///
/// Pass `socket_override` from the global `--socket` flag (mirrors
/// the existing `make_client` shape from `commands/build.rs`).
pub fn make_hook_client(socket_override: Option<PathBuf>) -> IpcClient {
    let base = match socket_override {
        Some(p) => IpcClient::new(p),
        None => IpcClient::default_path(),
    };
    base.with_no_autospawn().with_timeout(HOOK_IPC_BUDGET)
}

/// The payload Claude Code writes to a hook binary's STDIN.
///
/// Field coverage is a superset of the Claude Code hook schema — every
/// known hook event (PreToolUse, PostToolUse, UserPromptSubmit, Stop,
/// SubagentStop, SessionStart, SessionEnd, PreCompact) delivers some
/// subset of these fields, so we keep them all `Option` and let each
/// callsite pick what it needs.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HookPayload {
    /// Host session id — every event includes this.
    pub session_id: Option<String>,
    /// Hook event name (e.g. `"PreToolUse"`). Informational; the binary
    /// usually already knows what event it handles.
    pub hook_event_name: Option<String>,
    /// Tool name (PreToolUse / PostToolUse only).
    pub tool_name: Option<String>,
    /// Tool parameters (PreToolUse only). Opaque JSON.
    pub tool_input: Option<Value>,
    /// Tool result (PostToolUse only). Opaque JSON.
    pub tool_response: Option<Value>,
    /// User's typed prompt (UserPromptSubmit only).
    pub prompt: Option<String>,
    /// Working directory (UserPromptSubmit / SessionStart).
    pub cwd: Option<PathBuf>,
    /// Transcript file path (SessionStart / SessionEnd / PreCompact).
    pub transcript_path: Option<PathBuf>,
    /// How the session started ("startup" / "resume" / "clear" / "compact")
    /// on SessionStart only.
    pub source: Option<String>,
    /// True when the Stop hook is re-firing because a previous Stop hook
    /// already emitted `decision: "block"`. Binaries should short-circuit
    /// to prevent infinite retry loops.
    pub stop_hook_active: Option<bool>,
}

/// Read STDIN if it isn't a terminal and parse it as `HookPayload`.
///
/// Returns:
///   * `Ok(None)`  — STDIN is a TTY (interactive invocation with flags)
///                    OR STDIN is empty. Caller should use CLI flags.
///   * `Ok(Some)`  — STDIN had JSON; payload is populated.
///   * `Err(msg)`  — STDIN wasn't a TTY and wasn't empty, but parsing
///                    failed. Hook should exit 0 with a warning — we
///                    never block on our own parse bug.
pub fn read_stdin_payload() -> Result<Option<HookPayload>, String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    stdin
        .lock()
        .read_to_string(&mut buf)
        .map_err(|e| format!("stdin read: {e}"))?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let payload: HookPayload =
        serde_json::from_str(trimmed).map_err(|e| format!("hook JSON parse: {e}"))?;
    Ok(Some(payload))
}

/// Three-way fallback: prefer CLI flag value, then STDIN-payload value,
/// then a caller-supplied default. Used by every hook command so the
/// three-way merge is consistent across binaries.
pub fn choose<T>(cli: Option<T>, stdin_val: Option<T>, default: T) -> T {
    cli.or(stdin_val).unwrap_or(default)
}

/// HOOK-CANCELLED-001: resolve the active session id from CLI + STDIN,
/// returning `None` for the "no real session" case (no flag, no payload
/// session_id, OR the resolved id is whitespace-only). Hooks that don't
/// have a meaningful session to track (the `claude mcp list` path, fresh
/// stdin-less invocations, etc.) MUST skip every persistence + IPC side
/// effect and return Ok(()) immediately so Claude Code's hook budget
/// is never violated.
///
/// Every hook entry point that produces a session-keyed row (turn,
/// ledger, tool_call, file_event) calls this at the top, then bails
/// out on `None`. Hooks like `inject` that DO want to fall back to
/// `"unknown"` for non-empty prompts should keep using [`choose`]
/// instead.
pub fn resolved_session_id(cli: Option<String>, stdin_val: Option<String>) -> Option<String> {
    let raw = cli.or(stdin_val)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_prefers_cli() {
        let out = choose(Some(1u32), Some(2), 3);
        assert_eq!(out, 1);
    }

    #[test]
    fn choose_falls_back_to_stdin() {
        let out: u32 = choose(None, Some(2), 3);
        assert_eq!(out, 2);
    }

    #[test]
    fn choose_falls_back_to_default() {
        let out: u32 = choose(None, None, 3);
        assert_eq!(out, 3);
    }

    #[test]
    fn payload_parses_claude_code_pre_tool_shape() {
        let json = r#"{
            "session_id": "abc",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"}
        }"#;
        let p: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.tool_name.as_deref(), Some("Bash"));
        assert!(p.tool_input.is_some());
        assert!(p.prompt.is_none());
    }

    #[test]
    fn payload_parses_user_prompt_submit_shape() {
        let json = r#"{
            "session_id": "xyz",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "help me debug",
            "cwd": "/some/dir"
        }"#;
        let p: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.prompt.as_deref(), Some("help me debug"));
        assert_eq!(p.cwd.as_deref().and_then(|p| p.to_str()), Some("/some/dir"));
    }

    #[test]
    fn payload_ignores_unknown_fields() {
        // Claude Code may add fields in future versions; we must not
        // error on them.
        let json = r#"{
            "session_id": "abc",
            "some_future_field": {"anything": true}
        }"#;
        let p: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn payload_empty_object_ok() {
        let p: HookPayload = serde_json::from_str("{}").unwrap();
        assert!(p.session_id.is_none());
    }

    #[test]
    fn resolved_session_id_prefers_cli() {
        let id = resolved_session_id(Some("cli-id".into()), Some("stdin-id".into()));
        assert_eq!(id.as_deref(), Some("cli-id"));
    }

    #[test]
    fn resolved_session_id_falls_back_to_stdin() {
        let id = resolved_session_id(None, Some("stdin-id".into()));
        assert_eq!(id.as_deref(), Some("stdin-id"));
    }

    #[test]
    fn resolved_session_id_returns_none_when_both_missing() {
        let id = resolved_session_id(None, None);
        assert!(id.is_none());
    }

    #[test]
    fn resolved_session_id_returns_none_when_blank() {
        // Blank / whitespace-only ids are treated as "no real session".
        // The host command (e.g. `claude mcp list`) might emit one as
        // a placeholder; we must not key DB rows on a blank string.
        let id = resolved_session_id(Some("   ".into()), None);
        assert!(id.is_none(), "blank cli id should resolve to None");
        let id = resolved_session_id(None, Some("\n\t".into()));
        assert!(id.is_none(), "whitespace stdin id should resolve to None");
    }

    #[test]
    fn resolved_session_id_trims_whitespace() {
        let id = resolved_session_id(Some("  abc  ".into()), None);
        assert_eq!(id.as_deref(), Some("abc"));
    }
}
