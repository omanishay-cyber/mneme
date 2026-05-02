//! `mneme pre-tool` — PreToolUse hook entry point.
//!
//! Per design §6.3 the hook can short-circuit the tool call by returning
//! `{"skip": true, "result": "<cached>"}` (e.g. when an identical Read /
//! Bash hits the tool-call cache, §21.6.2 row 1) or pass through with
//! enrichment metadata that the supervisor's brain layer adds.
//!
//! ## v0.3.1 — STDIN + CLI parity
//!
//! Claude Code PreToolUse payload shape:
//! ```json
//! { "session_id": "...", "hook_event_name": "PreToolUse",
//!   "tool_name": "...", "tool_input": { ... } }
//! ```
//!
//! `tool_input` is an opaque object — we forward it to the supervisor
//! as a JSON string (the existing IPC contract expects `params: String`).
//!
//! The hook ALWAYS exits 0 on internal error. Claude Code treats
//! non-zero as BLOCK — blocking a tool call because mneme's supervisor
//! is down would be a regression of F-012.

use clap::Args;
use serde_json::json;
use std::path::PathBuf;
use tracing::warn;

use crate::error::CliResult;
use crate::hook_payload::{
    choose, make_hook_client, read_stdin_payload, HOOK_CTX_BUDGET, HOOK_IPC_BUDGET,
};
use crate::hook_writer::HookCtx;
use crate::ipc::{IpcRequest, IpcResponse};

/// CLI args for `mneme pre-tool`. All optional — STDIN fills in.
#[derive(Debug, Args)]
pub struct PreToolArgs {
    /// Tool name about to be invoked.
    #[arg(long)]
    pub tool: Option<String>,

    /// JSON-encoded tool params.
    #[arg(long)]
    pub params: Option<String>,

    /// Session id.
    #[arg(long = "session-id")]
    pub session_id: Option<String>,
}

/// Entry point used by `main.rs`.
pub async fn run(args: PreToolArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    let stdin_payload = match read_stdin_payload() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "pre-tool STDIN parse failed; passing through");
            None
        }
    };

    let stdin_tool = stdin_payload.as_ref().and_then(|p| p.tool_name.clone());
    let stdin_params = stdin_payload
        .as_ref()
        .and_then(|p| p.tool_input.as_ref().map(|v| v.to_string()));
    let stdin_session = stdin_payload.as_ref().and_then(|p| p.session_id.clone());

    let tool = choose(args.tool, stdin_tool, String::new());
    let params = choose(args.params, stdin_params, "{}".to_string());
    let session_id = choose(args.session_id, stdin_session, "unknown".to_string());

    // HOOK-CANCELLED-001 layer 1: short-lived host invocations may fire
    // PreToolUse with no tool name. Without a tool name there's nothing
    // to cache or short-circuit; emit `skip: false` immediately so the
    // host moves on without Claude Code cancelling us for budget overrun.
    if tool.trim().is_empty() {
        tracing::debug!("pre-tool fired without a tool name; passing through");
        println!("{}", serde_json::to_string(&json!({ "skip": false }))?);
        return Ok(());
    }

    // Bucket B4 fix: for filesystem-mutating tools, capture a livestate
    // file_events row so the live-bus view of "what's being touched"
    // stays current even when the supervisor's PreTool IPC handler is
    // missing (which is always — see the wider B4 horror).
    //
    // HOOK-CANCELLED-001 layer 2: bound `HookCtx::resolve` so first-time
    // shard creation can't push past Claude Code's hook budget.
    if matches!(
        tool.as_str(),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit"
    ) {
        let file_path = extract_file_path(&params);
        if let Some(fp) = file_path {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            match tokio::time::timeout(HOOK_CTX_BUDGET, HookCtx::resolve(&cwd)).await {
                Ok(Ok(ctx)) => {
                    let event_type = match tool.as_str() {
                        "Edit" | "MultiEdit" | "NotebookEdit" => "edit_pending",
                        "Write" => "write_pending",
                        _ => "tool_pending",
                    };
                    if let Err(e) = ctx.write_file_event(&fp, event_type, &session_id).await {
                        warn!(error = %e, "livestate.file_events insert failed (non-fatal)");
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "hook ctx resolve failed; skipping livestate write");
                }
                Err(_) => {
                    warn!(
                        budget_ms = HOOK_CTX_BUDGET.as_millis() as u64,
                        "hook ctx resolve exceeded budget; skipping livestate write"
                    );
                }
            }
        }
    }

    // HOOK-CANCELLED-001 layer 2 + Bug E (the resurrection-loop killer,
    // 2026-04-29): use the no-autospawn hook client. Supervisor down ⇒
    // hook silently no-ops. We do NOT resurrect the daemon on every tool
    // call — that path produced 22 cmd windows per tool call on our AWS test
    // host (postmortem §3.E + §12.5). User runs `mneme daemon start`
    // explicitly to activate context capture.
    let client = make_hook_client(socket_override);
    let ipc_call = client.request(IpcRequest::PreTool {
        tool,
        params,
        session_id,
    });
    let response = match tokio::time::timeout(HOOK_IPC_BUDGET, ipc_call).await {
        Ok(r) => r,
        Err(_) => {
            warn!(
                budget_ms = HOOK_IPC_BUDGET.as_millis() as u64,
                "pre-tool IPC exceeded hook budget; passing through"
            );
            Err(crate::error::CliError::Ipc("hook budget exceeded".into()))
        }
    };

    // The supervisor's response either carries `skip + result` (cache hit
    // or constraint violation) or `skip: false` (let the tool run). Any
    // error path emits `skip: false` so the tool always runs — we never
    // BLOCK on our own bug.
    let body = match response {
        Ok(IpcResponse::Ok { message }) => json!({ "skip": false, "note": message }),
        Ok(IpcResponse::Error { message }) => {
            warn!(error = %message, "pre-tool supervisor error; passing through");
            json!({ "skip": false })
        }
        Ok(IpcResponse::Pong)
        | Ok(IpcResponse::Status { .. })
        | Ok(IpcResponse::Logs { .. })
        | Ok(IpcResponse::JobQueued { .. })
        | Ok(IpcResponse::JobQueue { .. })
        | Ok(IpcResponse::RecallResults { .. })
        | Ok(IpcResponse::BlastResults { .. })
        | Ok(IpcResponse::GodNodesResults { .. }) => json!({ "skip": false }),
        Ok(_) => json!({ "skip": false }),
        Err(e) => {
            warn!(error = %e, "pre-tool supervisor unreachable; passing through");
            json!({ "skip": false })
        }
    };

    println!("{}", serde_json::to_string(&body)?);
    Ok(())
}

/// Pull `file_path` (or `notebook_path`) out of a PreToolUse `tool_input`
/// blob. Returns `None` when the JSON does not carry a recognisable
/// path field, so callers can decide to skip the livestate write.
///
/// Recognised keys (any one of these wins):
///   - `file_path`     — Edit / Write / MultiEdit
///   - `notebook_path` — NotebookEdit
///
/// Best-effort: parse failures degrade silently to `None` since blocking
/// the tool call here would violate RULE 17.
fn extract_file_path(params_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(params_json).ok()?;
    if let Some(p) = value.get("file_path").and_then(|v| v.as_str()) {
        return Some(p.to_string());
    }
    if let Some(p) = value.get("notebook_path").and_then(|v| v.as_str()) {
        return Some(p.to_string());
    }
    None
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
        args: PreToolArgs,
    }

    #[test]
    fn pre_tool_args_parse_with_no_flags() {
        // All optional — STDIN fills them in at runtime.
        let h = Harness::try_parse_from(["x"]).unwrap();
        assert!(h.args.tool.is_none());
        assert!(h.args.params.is_none());
        assert!(h.args.session_id.is_none());
    }

    #[test]
    fn pre_tool_args_parse_with_all_flags() {
        let h = Harness::try_parse_from([
            "x",
            "--tool",
            "Read",
            "--params",
            r#"{"file_path":"/x"}"#,
            "--session-id",
            "s-42",
        ])
        .unwrap();
        assert_eq!(h.args.tool.as_deref(), Some("Read"));
        assert_eq!(h.args.session_id.as_deref(), Some("s-42"));
        assert!(h.args.params.is_some());
    }

    #[test]
    fn extract_file_path_finds_edit_target() {
        let p = extract_file_path(r#"{"file_path":"/x/y.rs","old_string":"a","new_string":"b"}"#);
        assert_eq!(p.as_deref(), Some("/x/y.rs"));
    }

    #[test]
    fn extract_file_path_finds_notebook_target() {
        let p = extract_file_path(r#"{"notebook_path":"/n.ipynb","cell_id":"c1"}"#);
        assert_eq!(p.as_deref(), Some("/n.ipynb"));
    }

    #[test]
    fn extract_file_path_none_for_bash() {
        let p = extract_file_path(r#"{"command":"ls -la"}"#);
        assert!(p.is_none());
    }

    #[test]
    fn extract_file_path_none_for_garbage_json() {
        let p = extract_file_path("{not json");
        assert!(p.is_none());
    }

    /// Test isolation helper. See session_end.rs for rationale.
    fn cwd_into_marker_free_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_current_dir(dir.path()).expect("set_current_dir");
        dir
    }

    /// HOOK-CANCELLED-001: pre-tool with no tool name emits the
    /// pass-through `skip: false` JSON and exits Ok promptly.
    #[tokio::test]
    async fn pre_tool_with_empty_stdin_exits_zero() {
        let _keep = cwd_into_marker_free_tempdir();
        let start = std::time::Instant::now();
        let args = PreToolArgs {
            tool: None,
            params: None,
            session_id: None,
        };
        let r = run(args, Some(PathBuf::from("/nope-mneme.sock"))).await;
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "pre-tool with no tool must exit Ok; got: {r:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "pre-tool no-op path must be effectively instant; took {elapsed:?}"
        );
    }
}
