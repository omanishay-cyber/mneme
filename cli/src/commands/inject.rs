//! `mneme inject` — UserPromptSubmit hook entry point.
//!
//! Claude Code calls this after the user submits a prompt. We forward the
//! prompt to the supervisor, which composes a "smart inject bundle"
//! (§4.2): recent decisions, active constraints, blast-radius previews,
//! drift redirect, and the current step from the ledger.
//!
//! ## v0.3.1 — STDIN + CLI parity
//!
//! Claude Code delivers the payload on STDIN as JSON:
//!
//! ```json
//! { "session_id": "...", "hook_event_name": "UserPromptSubmit",
//!   "prompt": "...", "cwd": "..." }
//! ```
//!
//! Manual testing from a shell uses `--prompt`, `--session-id`, `--cwd`.
//! Both paths work; CLI flags win when both present. See
//! [`crate::hook_payload`] for the merge logic.
//!
//! If STDIN is a TTY and no flags are passed, all fields default to
//! safe empty values and we emit an empty `additional_context`. The
//! rule is hard: **this hook NEVER exits non-zero**. It was the
//! deepest-blast-radius hook in the v0.3.0 self-trap (it gated
//! UserPromptSubmit — a non-zero exit muted the user), and must never
//! block a prompt because of an internal failure of mneme.
//!
//! ## v0.3.1+ — skill prescription
//!
//! When the payload carries a `prompt`, the hook also runs a minimal
//! in-process skill matcher against `~/.mneme/plugin/skills/` (see
//! [`crate::skill_matcher`]) and, if the top suggestion fires at
//! `medium` or `high` confidence, appends a
//! `<mneme-skill-prescription>` block to the emitted
//! `additional_context`. Pass `--no-skill-hint` to skip this.
//!
//! Output format is the JSON shape Claude Code expects from a
//! UserPromptSubmit hook:
//!
//! ```json
//! { "hookEventName": "UserPromptSubmit",
//!   "additional_context": "<mneme-context>...</mneme-context>" }
//! ```

use clap::Args;
use serde_json::json;
use std::path::{Path, PathBuf};
use tracing::warn;

use common::{ids::ProjectId, layer::DbLayer, paths::PathManager};

use crate::error::CliResult;
use crate::hook_payload::{
    choose, make_hook_client, read_stdin_payload, HOOK_CTX_BUDGET, HOOK_IPC_BUDGET,
};
use crate::hook_writer::HookCtx;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::secrets_redact::redact;
use crate::skill_matcher::{reason_for, suggest, Confidence, Suggestion};

/// J9: cap on the rendered `<mneme-context>` block so the recall payload
/// never dominates Claude's context window. ~4 KB is the budget agreed
/// on in NEXT-PATH §B1.4 — large enough for 5 turns + 3 ledger entries
/// + a handful of file_intent rows, small enough that the user's actual
/// prompt is still the dominant token cost on every turn.
const RECALL_BLOCK_BYTE_CAP: usize = 4_000;

/// J9: how many recent rows of each kind to surface in the recall block.
/// Tuned per NEXT-PATH §B1.4: enough continuity to be useful, not so much
/// that the block bloats past [`RECALL_BLOCK_BYTE_CAP`].
const RECALL_TURNS_LIMIT: usize = 5;
const RECALL_LEDGER_LIMIT: usize = 3;
const RECALL_FILE_INTENT_LIMIT: usize = 8;

/// Default staleness threshold (days) when `staleness_warn_days` is not
/// set in `<project_root>/.claude/mneme.json`. Audit-L12 acceptance.
const DEFAULT_STALENESS_WARN_DAYS: i64 = 7;

/// CLI args for `mneme inject`. All optional — STDIN JSON fills in
/// anything missing.
#[derive(Debug, Args)]
pub struct InjectArgs {
    /// The user prompt as captured by the hook. If absent, read from
    /// STDIN payload `.prompt` or treated as empty.
    #[arg(long)]
    pub prompt: Option<String>,

    /// Session id assigned by the host. If absent, read from STDIN
    /// `.session_id` or defaulted to `"unknown"`.
    #[arg(long = "session-id")]
    pub session_id: Option<String>,

    /// Working directory at the time the hook fired. If absent, read
    /// from STDIN `.cwd` or the process CWD.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Skip the `<mneme-skill-prescription>` block. Useful when the
    /// user wants the supervisor's context without any skill-router
    /// nudge.
    #[arg(long = "no-skill-hint", default_value_t = false)]
    pub no_skill_hint: bool,
}

/// Entry point used by `main.rs`.
pub async fn run(args: InjectArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    // Read STDIN payload; log and continue on any parse error so we never
    // block the user's prompt because of our own bug. See module docs.
    let stdin_payload = match read_stdin_payload() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "hook STDIN parse failed; falling back to CLI flags / empty");
            None
        }
    };

    let stdin_prompt = stdin_payload.as_ref().and_then(|p| p.prompt.clone());
    let stdin_session = stdin_payload.as_ref().and_then(|p| p.session_id.clone());
    let stdin_cwd = stdin_payload.as_ref().and_then(|p| p.cwd.clone());

    let prompt = choose(args.prompt, stdin_prompt, String::new());
    let session_id = choose(args.session_id, stdin_session, "unknown".to_string());
    let cwd = choose(
        args.cwd,
        stdin_cwd,
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );

    // Capture cwd before we move it into the IPC request - we still need
    // it locally for the staleness probe.
    let cwd_for_staleness = cwd.clone();

    // Bucket B4 fix: persist the user turn to history.db BEFORE the IPC
    // call. The supervisor's ControlCommand enum has no `Inject` variant
    // (the IPC will return an Error), so without this direct write the
    // turn would never land — leaving history.db permanently empty for
    // hook-driven projects. Per RULE 17 we never block the user op:
    // any failure is logged as a warning and we keep going.
    //
    // SEC-2: scrub credential / token shapes from the prompt BEFORE the
    // shard write. `hook_writer::write_turn` runs `redact` again as a
    // defence-in-depth pass — both layers are cheap and idempotent.
    if !prompt.trim().is_empty() {
        let redacted_prompt = redact(&prompt);
        // HOOK-CANCELLED-001 layer 2: bound `HookCtx::resolve` so first-time
        // shard creation can't push past Claude Code's hook budget.
        match tokio::time::timeout(HOOK_CTX_BUDGET, HookCtx::resolve(&cwd_for_staleness)).await {
            Ok(Ok(ctx)) => {
                if let Err(e) = ctx.write_turn(&session_id, "user", &redacted_prompt).await {
                    warn!(error = %e, "history.turns insert failed (non-fatal)");
                }
            }
            Ok(Err(e)) => {
                warn!(error = %e, "hook ctx resolve failed; skipping history.turns write");
            }
            Err(_) => {
                warn!(
                    budget_ms = HOOK_CTX_BUDGET.as_millis() as u64,
                    "hook ctx resolve exceeded budget; skipping history.turns write"
                );
            }
        }
    }

    // HOOK-CANCELLED-001 layer 2 + Bug E (the resurrection-loop killer,
    // 2026-04-29): use the no-autospawn hook client. Supervisor down ⇒
    // hook silently no-ops (the daemon-down branch below converts the IPC
    // Err into an empty additional_context block with no `<mneme-status>`
    // sermon — see the response handler below).
    let client = make_hook_client(socket_override);
    let ipc_call = client.request(IpcRequest::Inject {
        prompt: prompt.clone(),
        session_id,
        cwd,
    });
    let response = match tokio::time::timeout(HOOK_IPC_BUDGET, ipc_call).await {
        Ok(r) => r,
        Err(_) => {
            warn!(
                budget_ms = HOOK_IPC_BUDGET.as_millis() as u64,
                "inject IPC exceeded hook budget; emitting empty additional_context"
            );
            Err(crate::error::CliError::Ipc("hook budget exceeded".into()))
        }
    };

    // Bug E (2026-04-29): when the supervisor is unreachable, the hook
    // silently no-ops with an empty additional_context. The previous
    // UX-3 behaviour was to render a `<mneme-status>` block every prompt
    // — that combined with the hook auto-spawn loop produced the
    // "hydra heads" on our AWS test host (postmortem §3.E + §12.5).
    //
    // The new contract: mneme is intentionally inactive when the user
    // has not run `mneme daemon start`. Silence on the prompt is the
    // correct behaviour; a status sermon every prompt is not. If the
    // user wants context, they activate the daemon.
    //
    // We still NEVER block the user's prompt (RULE 17 / hook never exits
    // non-zero) — Err just collapses to an empty payload that downstream
    // skill-prescription / additional_context blocks can still append to.
    let mut payload = match response {
        Ok(IpcResponse::Ok { message }) => message.unwrap_or_default(),
        Ok(IpcResponse::Error { message }) => {
            warn!(error = %message, "supervisor returned error; emitting empty additional_context");
            String::new()
        }
        Ok(IpcResponse::Pong)
        | Ok(IpcResponse::Status { .. })
        | Ok(IpcResponse::Logs { .. })
        | Ok(IpcResponse::JobQueued { .. })
        | Ok(IpcResponse::JobQueue { .. })
        | Ok(IpcResponse::RecallResults { .. })
        | Ok(IpcResponse::BlastResults { .. })
        | Ok(IpcResponse::GodNodesResults { .. })
        | Ok(IpcResponse::Dispatched { .. })
        | Ok(IpcResponse::GraphifyCorpusQueued { .. })
        | Ok(IpcResponse::SnapshotCombined { .. })
        | Ok(IpcResponse::RebuildAcked { .. })
        | Ok(IpcResponse::BadRequest { .. }) => String::new(),
        Err(e) => {
            tracing::debug!(error = %e, "daemon down, inject hook silent no-op");
            String::new()
        }
    };

    // Append the skill-router recommendation when:
    //   - the user actually typed something (skip empty prompts),
    //   - the caller did not pass --no-skill-hint,
    //   - the top suggestion fires at medium or high confidence.
    if !args.no_skill_hint && !prompt.trim().is_empty() {
        match std::panic::catch_unwind(|| suggest(&prompt, 1)) {
            Ok(hits) => {
                if let Some(hit) = hits.into_iter().next() {
                    if matches!(hit.confidence, Confidence::Medium | Confidence::High) {
                        let block = render_skill_block(&prompt, &hit);
                        if payload.is_empty() {
                            payload = block;
                        } else {
                            payload.push_str("\n\n");
                            payload.push_str(&block);
                        }
                    }
                }
            }
            Err(_) => {
                warn!("skill matcher panicked; dropping skill prescription");
            }
        }
    }

    // Append the staleness nag (audit-L12) when the project has been
    // built before but not in the configured threshold window. Failure
    // here is silent: this hook NEVER blocks the user's prompt, and a
    // missing meta.db row is a different problem (project not yet built)
    // with a different fix (run `mneme build`), not this user's concern.
    let paths = PathManager::default_root();
    if let Some(block) = render_staleness_block(&paths, &cwd_for_staleness) {
        if payload.is_empty() {
            payload = block;
        } else {
            payload.push_str("\n\n");
            payload.push_str(&block);
        }
    }

    // J9: persistent-memory recall block. Reads recent turns + ledger
    // decisions/impls + per-file intent annotations from the project's
    // shards (read-only) and renders a `<mneme-context>` block so Claude
    // is never cold across sessions. This is the moment mneme stops
    // being a capture pipeline and starts being a real memory layer
    // (NEXT-PATH §B1.4).
    //
    // Failure mode: any I/O / SQL failure produces an empty block — the
    // hook NEVER blocks the user's prompt because of a recall miss.
    if let Some(project_root) = find_project_root_for_cwd(&cwd_for_staleness) {
        if let Ok(project_id) = ProjectId::from_path(&project_root) {
            let recall_block = build_recall_context_block(&paths, &project_id, &prompt);
            if !recall_block.is_empty() {
                if payload.is_empty() {
                    payload = recall_block;
                } else {
                    payload.push_str("\n\n");
                    payload.push_str(&recall_block);
                }
            }
        }
    }

    let out = json!({
        "hookEventName": "UserPromptSubmit",
        "additional_context": payload,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// UX-3 (deprecated by Bug E, 2026-04-29): render a small
/// `<mneme-status>` block when the supervisor is unreachable.
///
/// Kept for reference but no longer wired in. Bug E flipped the policy:
/// daemon-down is now silent (postmortem §3.E + §12.5 — a per-prompt
/// status block was contributing to the user-visible noise that masked
/// the hydra-heads loop). When mneme is inactive the user runs
/// `mneme daemon start` to activate it; the prompt itself stays clean.
#[allow(dead_code)]
fn render_supervisor_unreachable_block() -> String {
    "<mneme-status>supervisor unreachable - start with 'mneme daemon start'</mneme-status>"
        .to_string()
}

/// Render a single `<mneme-skill-prescription>` block. Kept ASCII-only
/// — the user's Windows cp1252 terminal breaks on em-dashes and other
/// fancy punctuation.
fn render_skill_block(prompt: &str, hit: &Suggestion) -> String {
    let excerpt = excerpt(prompt, 120);
    // `to_load` is a plain `cat` against the absolute SKILL.md path so
    // the assistant can load the skill without the MCP server being up.
    // The path is the one mneme actually parsed, so dev-tree runs work
    // the same as installed-plugin runs.
    let source = hit.source_path.to_string_lossy();
    let reason = reason_for(hit);
    format!(
        concat!(
            "<mneme-skill-prescription>\n",
            "  task: {task}\n",
            "  recommended_skill: {skill}\n",
            "  confidence: {confidence}\n",
            "  reason: {reason}\n",
            "  to_load: cat {path}\n",
            "</mneme-skill-prescription>",
        ),
        task = excerpt,
        skill = hit.skill,
        confidence = hit.confidence.as_str(),
        reason = reason,
        path = source,
    )
}

/// Collapse whitespace + truncate so the excerpt never blows out the
/// hook JSON. Keeps output single-line-friendly.
fn excerpt(raw: &str, max_chars: usize) -> String {
    let collapsed: String = raw
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

// ---------------------------------------------------------------------------
// L12: stale-index nag - `<mneme-primer-staleness>` block
// ---------------------------------------------------------------------------

/// Render the `<mneme-primer-staleness>` block for the inject hook.
///
/// Returns `None` (suppressed) when:
/// - The project has never been built (no `meta.db` row, or
///   `last_indexed_at` is NULL - that's a different message,
///   surfaced by the build path itself).
/// - The project was indexed within the threshold window
///   (default 7 days, configurable via
///   `<project_root>/.claude/mneme.json::staleness_warn_days`).
/// - Any I/O / SQL error occurs (silent: this hook NEVER blocks the
///   user's prompt because of an internal mneme failure).
///
/// Returns `Some(block)` only when `last_indexed_at` is populated and
/// older than the configured threshold.
fn render_staleness_block(paths: &PathManager, cwd: &Path) -> Option<String> {
    let project = find_project_root_for_cwd(cwd)?;
    let project_id = ProjectId::from_path(&project).ok()?;
    let last_indexed = read_last_indexed(paths, &project_id)?;
    let age_days = age_in_days(&last_indexed)?;
    let threshold = staleness_threshold_days(&project);
    if age_days <= threshold {
        return None;
    }
    Some(format_staleness_block(age_days, threshold))
}

/// Format the staleness block. Kept ASCII-only - Windows cp1252
/// terminals corrupt non-ASCII characters in additional_context.
fn format_staleness_block(age_days: i64, threshold_days: i64) -> String {
    format!(
        concat!(
            "<mneme-primer-staleness>\n",
            "Project last indexed {age} days ago (threshold: {threshold} days).\n",
            "Recall + blast results may not reflect recent edits.\n",
            "Run `mneme build` to refresh, or `mneme rebuild` for a clean reset.\n",
            "</mneme-primer-staleness>",
        ),
        age = age_days,
        threshold = threshold_days,
    )
}

/// Mirror of MCP's findProjectRoot: walk up from `cwd` looking for
/// any of the standard project markers. Returns the first match, or
/// `None` if we hit the filesystem root without finding any.
fn find_project_root_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let markers = [
        ".git",
        ".claude",
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
    ];
    let mut cur: PathBuf = cwd.to_path_buf();
    for _ in 0..40 {
        for m in markers.iter() {
            if cur.join(m).exists() {
                return Some(cur);
            }
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => return None,
        }
    }
    None
}

/// Read `meta.db::projects.last_indexed_at` for `project_id`. Returns
/// `None` on any error or when the column is NULL.
fn read_last_indexed(paths: &PathManager, project_id: &ProjectId) -> Option<String> {
    let meta_path = paths.meta_db();
    if !meta_path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &meta_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let result: Result<Option<String>, rusqlite::Error> = conn.query_row(
        "SELECT last_indexed_at FROM projects WHERE id = ?1",
        rusqlite::params![project_id.as_str()],
        |r| r.get(0),
    );
    result.ok().flatten()
}

/// Compute integer days between SQLite `datetime('now')` format
/// ("YYYY-MM-DD HH:MM:SS", UTC) and the current wall clock. Returns
/// `None` on parse failure.
fn age_in_days(stamp: &str) -> Option<i64> {
    // SQLite datetime('now') is UTC and uses space separator. chrono's
    // RFC3339 parser does not accept that directly - use NaiveDateTime.
    let parsed = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S").ok()?;
    let parsed_utc = parsed.and_utc();
    let now = chrono::Utc::now();
    let delta = now.signed_duration_since(parsed_utc);
    let days = delta.num_seconds() / 86_400;
    Some(days)
}

/// Read `<project_root>/.claude/mneme.json` and return its
/// `staleness_warn_days` key. Falls back to [`DEFAULT_STALENESS_WARN_DAYS`]
/// on any of the silent-default conditions: missing file, parse error,
/// missing key, non-integer value, or non-positive value.
fn staleness_threshold_days(project_root: &Path) -> i64 {
    let path = project_root.join(".claude").join("mneme.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DEFAULT_STALENESS_WARN_DAYS;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return DEFAULT_STALENESS_WARN_DAYS;
    };
    let Some(n) = value.get("staleness_warn_days").and_then(|v| v.as_i64()) else {
        return DEFAULT_STALENESS_WARN_DAYS;
    };
    if n <= 0 {
        return DEFAULT_STALENESS_WARN_DAYS;
    }
    n
}

// ---------------------------------------------------------------------------
// J9: persistent-memory recall block - `<mneme-context>` (NEXT-PATH §B1.4)
// ---------------------------------------------------------------------------

/// Build the `<mneme-context>` block that primes Claude with prior project
/// context on every UserPromptSubmit. Reads three signals (read-only):
///
///   1. Last [`RECALL_TURNS_LIMIT`] turns from `history.db::turns`
///      (DESC by id) — gives Claude continuity across sessions.
///   2. Last [`RECALL_LEDGER_LIMIT`] entries from
///      `tasks.db::ledger_entries` where `kind IN ('decision', 'impl')`
///      (DESC by timestamp) — surfaces the most recent decisions / impls.
///   3. `memory.db::file_intent` rows for any path tokens that look like
///      a file reference inside `prompt` — gives Claude per-file
///      freeze / stable / experimental annotations.
///
/// Returns the empty string when:
///   - The project's shards do not exist yet (never built).
///   - Every query returned zero rows (clean slate).
///   - Any I/O / SQL error fires (silent — RULE 17, never block hook).
///
/// Total output is hard-capped at [`RECALL_BLOCK_BYTE_CAP`] bytes so the
/// recall payload never crowds the user's actual prompt out of context.
fn build_recall_context_block(paths: &PathManager, project_id: &ProjectId, prompt: &str) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(turns_block) = render_recent_turns(paths, project_id) {
        sections.push(turns_block);
    }
    if let Some(ledger_block) = render_recent_ledger(paths, project_id) {
        sections.push(ledger_block);
    }
    if let Some(intent_block) = render_file_intent(paths, project_id, prompt) {
        sections.push(intent_block);
    }

    if sections.is_empty() {
        return String::new();
    }

    let body = sections.join("\n");
    let raw = format!("<mneme-context>\n{}\n</mneme-context>", body);
    cap_block_size(raw, RECALL_BLOCK_BYTE_CAP)
}

/// Render the "Recent turns" subsection. Returns `None` on any failure
/// (missing shard, SQL error, zero rows).
fn render_recent_turns(paths: &PathManager, project_id: &ProjectId) -> Option<String> {
    let db = paths.shard_db(project_id, DbLayer::History);
    if !db.exists() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT role, session_id, content FROM turns \
             ORDER BY id DESC LIMIT ?1",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![RECALL_TURNS_LIMIT as i64], |r| {
            let role: String = r.get(0)?;
            let session_id: String = r.get(1)?;
            let content: String = r.get(2)?;
            Ok((role, session_id, content))
        })
        .ok()?;

    let mut lines: Vec<String> = Vec::new();
    for row in rows.flatten() {
        let (role, session_id, content) = row;
        let one_line = excerpt(&content, 160);
        lines.push(format!("- [{}, {}] {}", role, session_id, one_line));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Recent turns (from history.db.turns)\n{}",
        lines.join("\n")
    ))
}

/// Render the "Recent decisions" subsection. Filters to
/// `kind IN ('decision', 'impl')` and orders by timestamp DESC.
fn render_recent_ledger(paths: &PathManager, project_id: &ProjectId) -> Option<String> {
    let db = paths.shard_db(project_id, DbLayer::Tasks);
    if !db.exists() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT kind, summary, rationale FROM ledger_entries \
             WHERE kind IN ('decision', 'impl') \
             ORDER BY timestamp DESC LIMIT ?1",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![RECALL_LEDGER_LIMIT as i64], |r| {
            let kind: String = r.get(0)?;
            let summary: String = r.get(1)?;
            let rationale: Option<String> = r.get(2)?;
            Ok((kind, summary, rationale))
        })
        .ok()?;

    let mut lines: Vec<String> = Vec::new();
    for row in rows.flatten() {
        let (kind, summary, rationale) = row;
        let summary_line = excerpt(&summary, 160);
        let line = match rationale {
            Some(r) if !r.trim().is_empty() => {
                let rationale_line = excerpt(&r, 160);
                format!("- [{}] {} - {}", kind, summary_line, rationale_line)
            }
            _ => format!("- [{}] {}", kind, summary_line),
        };
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Recent decisions (from tasks.db.ledger_entries)\n{}",
        lines.join("\n")
    ))
}

/// Render the "File intent" subsection. Extracts file-like path tokens
/// from `prompt`, queries `memory.db::file_intent` for each, and returns
/// any matched rows.
fn render_file_intent(paths: &PathManager, project_id: &ProjectId, prompt: &str) -> Option<String> {
    let db = paths.shard_db(project_id, DbLayer::Memory);
    if !db.exists() {
        return None;
    }
    let mentioned = extract_file_tokens(prompt);
    if mentioned.is_empty() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;

    let mut lines: Vec<String> = Vec::new();
    for token in mentioned.iter().take(RECALL_FILE_INTENT_LIMIT) {
        // `LIKE '%token%'` keeps us tolerant of relative-vs-absolute path
        // differences (the prompt rarely has the exact key we wrote into
        // file_intent.file_path). Capped to one row per token so a noisy
        // prompt doesn't blow the byte budget.
        let result: Result<Option<(String, String, Option<String>)>, rusqlite::Error> = conn
            .query_row(
                "SELECT file_path, intent, reason FROM file_intent \
                 WHERE file_path LIKE ?1 LIMIT 1",
                rusqlite::params![format!("%{}%", token)],
                |r| {
                    let path: String = r.get(0)?;
                    let intent: String = r.get(1)?;
                    let reason: Option<String> = r.get(2)?;
                    Ok(Some((path, intent, reason)))
                },
            );
        if let Ok(Some((path, intent, reason))) = result {
            let line = match reason {
                Some(r) if !r.trim().is_empty() => {
                    let reason_line = excerpt(&r, 160);
                    format!("- {}: {} - {}", path, intent, reason_line)
                }
                _ => format!("- {}: {}", path, intent),
            };
            // De-dup: if multiple tokens hit the same file_path, keep
            // only the first match.
            if !lines.iter().any(|l| l.starts_with(&format!("- {}:", path))) {
                lines.push(line);
            }
        }
    }

    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## File intent (from memory.db.file_intent for prompt-mentioned files)\n{}",
        lines.join("\n")
    ))
}

/// Tokenise a prompt and return whitespace-separated tokens that look
/// like file references (contain `/`, `\`, or a `.<ext>` suffix). Stripped
/// of trailing punctuation so `src/auth.ts.` returns `src/auth.ts`. The
/// caller queries each token via `LIKE '%token%'`, so over-matching is
/// fine — under-matching (missing a real file) is the only failure mode
/// to avoid.
fn extract_file_tokens(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in prompt.split_whitespace() {
        // Strip surrounding punctuation that commonly trails a file
        // reference in human prose: backticks, quotes, parens, commas,
        // colons, semicolons, periods (NB: trailing period is more
        // common than a real ".x" extension).
        let trimmed = raw.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '!' | '?'
            )
        });
        // Strip a trailing period only — keeps `src/auth.ts` intact but
        // turns `auth.ts.` into `auth.ts`.
        let trimmed = trimmed.trim_end_matches('.');
        if trimmed.is_empty() {
            continue;
        }
        // Heuristic: looks like a file if it has a path separator OR a
        // dot followed by 1-5 ASCII alphanum chars (extension).
        let has_sep = trimmed.contains('/') || trimmed.contains('\\');
        let has_ext = match trimmed.rfind('.') {
            Some(i) if i + 1 < trimmed.len() => {
                let ext = &trimmed[i + 1..];
                !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
            }
            _ => false,
        };
        if !(has_sep || has_ext) {
            continue;
        }
        let token = trimmed.to_string();
        if !out.contains(&token) {
            out.push(token);
        }
    }
    out
}

/// Truncate a rendered block to `cap` bytes on a char boundary. Kept as
/// a free fn so it can be unit-tested independently. Appends an ASCII
/// ellipsis marker when truncation actually fired.
fn cap_block_size(raw: String, cap: usize) -> String {
    if raw.len() <= cap {
        return raw;
    }
    // Find the largest char-boundary <= cap so we never split mid-glyph.
    let mut split = cap;
    while split > 0 && !raw.is_char_boundary(split) {
        split -= 1;
    }
    let mut out = String::with_capacity(split + 32);
    out.push_str(&raw[..split]);
    out.push_str("\n... [recall truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn excerpt_collapses_and_truncates() {
        let long =
            "  hello\n  world  this  is   a   very   long   prompt   that   must   be   truncated ";
        let out = excerpt(long, 30);
        assert!(out.starts_with("hello world"));
        assert!(out.len() <= 33); // 30 chars + "..."
        assert!(out.ends_with("..."));
    }

    #[test]
    fn render_block_is_ascii() {
        let hit = Suggestion {
            skill: "fireworks-debug".to_string(),
            triggers_matched: vec!["debug".to_string()],
            tags_matched: Vec::new(),
            confidence: Confidence::Medium,
            source_path: PathBuf::from("/tmp/SKILL.md"),
            score: 2,
        };
        let block = render_skill_block("debug a test", &hit);
        assert!(block.is_ascii());
        assert!(block.contains("recommended_skill: fireworks-debug"));
        assert!(block.contains("confidence: medium"));
        assert!(block.contains("to_load: cat /tmp/SKILL.md"));
    }

    // ---------- L12 staleness nag tests ----------

    /// Build a complete fake mneme env: a project root with a `.git`
    /// marker (so find_project_root_for_cwd resolves it), and a
    /// `~/.mneme/meta.db` containing a single `projects` row with a
    /// custom `last_indexed_at` value. Returns the kept tempdir, the
    /// path manager, and the project root.
    fn fixture_with_indexed(
        last_indexed: Option<&str>,
    ) -> (tempfile::TempDir, PathManager, PathBuf) {
        let dir = tempdir().expect("tempdir");
        let mneme_root = dir.path().join("mneme-home");
        std::fs::create_dir_all(&mneme_root).unwrap();
        let paths = PathManager::with_root(mneme_root);

        let project_root = dir.path().join("proj");
        std::fs::create_dir_all(project_root.join(".git")).unwrap();

        // Build meta.db with the same shape as schema::META_SQL.
        let conn = rusqlite::Connection::open(paths.meta_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE projects (
                id TEXT PRIMARY KEY,
                root TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_indexed_at TEXT,
                schema_version INTEGER NOT NULL
             );",
        )
        .unwrap();

        let id = ProjectId::from_path(&project_root).unwrap();
        match last_indexed {
            Some(ts) => {
                conn.execute(
                    "INSERT INTO projects(id, root, name, last_indexed_at, schema_version) VALUES(?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![id.as_str(), project_root.to_string_lossy(), "fixture", ts, 1],
                )
                .unwrap();
            }
            None => {
                conn.execute(
                    "INSERT INTO projects(id, root, name, schema_version) VALUES(?1, ?2, ?3, ?4)",
                    rusqlite::params![id.as_str(), project_root.to_string_lossy(), "fixture", 1],
                )
                .unwrap();
            }
        }

        (dir, paths, project_root)
    }

    fn ts_days_ago(days: i64) -> String {
        let stamp = chrono::Utc::now() - chrono::Duration::days(days);
        stamp.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    #[test]
    fn staleness_block_emitted_when_index_is_old() {
        let stamp = ts_days_ago(15);
        let (_keep, paths, project_root) = fixture_with_indexed(Some(&stamp));
        let block = render_staleness_block(&paths, &project_root)
            .expect("expected a staleness block for a 15-day-old index");
        assert!(block.is_ascii());
        assert!(block.starts_with("<mneme-primer-staleness>"));
        assert!(block.ends_with("</mneme-primer-staleness>"));
        assert!(block.contains("threshold: 7 days"));
        assert!(block.contains("Run `mneme build`"));
    }

    #[test]
    fn staleness_block_suppressed_when_index_is_fresh() {
        let stamp = ts_days_ago(1);
        let (_keep, paths, project_root) = fixture_with_indexed(Some(&stamp));
        let block = render_staleness_block(&paths, &project_root);
        assert!(
            block.is_none(),
            "expected NO block for a 1-day-old index, got {block:?}"
        );
    }

    #[test]
    fn staleness_block_suppressed_when_never_indexed() {
        let (_keep, paths, project_root) = fixture_with_indexed(None);
        let block = render_staleness_block(&paths, &project_root);
        assert!(
            block.is_none(),
            "expected NO block for a never-built project (different problem)"
        );
    }

    #[test]
    fn staleness_threshold_default_is_seven() {
        let dir = tempdir().unwrap();
        // No .claude/mneme.json file.
        assert_eq!(
            staleness_threshold_days(dir.path()),
            DEFAULT_STALENESS_WARN_DAYS
        );
    }

    #[test]
    fn staleness_threshold_reads_mneme_json() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude/mneme.json"),
            r#"{"staleness_warn_days": 30}"#,
        )
        .unwrap();
        assert_eq!(staleness_threshold_days(dir.path()), 30);
    }

    #[test]
    fn staleness_threshold_silent_default_on_garbage() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(dir.path().join(".claude/mneme.json"), "not json").unwrap();
        assert_eq!(
            staleness_threshold_days(dir.path()),
            DEFAULT_STALENESS_WARN_DAYS
        );
    }

    #[test]
    fn staleness_threshold_silent_default_on_missing_key() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(dir.path().join(".claude/mneme.json"), r#"{"other": 5}"#).unwrap();
        assert_eq!(
            staleness_threshold_days(dir.path()),
            DEFAULT_STALENESS_WARN_DAYS
        );
    }

    #[test]
    fn staleness_threshold_honors_per_project_override() {
        // A 10-day-old index with threshold=20 should NOT trigger the block.
        let stamp = ts_days_ago(10);
        let (_keep, paths, project_root) = fixture_with_indexed(Some(&stamp));
        std::fs::create_dir_all(project_root.join(".claude")).unwrap();
        std::fs::write(
            project_root.join(".claude/mneme.json"),
            r#"{"staleness_warn_days": 20}"#,
        )
        .unwrap();
        let block = render_staleness_block(&paths, &project_root);
        assert!(
            block.is_none(),
            "10-day index with 20-day threshold must not warn"
        );

        // Same index with threshold=5 (override below default) MUST trigger.
        std::fs::write(
            project_root.join(".claude/mneme.json"),
            r#"{"staleness_warn_days": 5}"#,
        )
        .unwrap();
        let block = render_staleness_block(&paths, &project_root)
            .expect("10-day index with 5-day threshold MUST warn");
        assert!(block.contains("threshold: 5 days"));
    }

    // ---------- J9 recall context block tests ----------

    /// Build a fully-seeded mneme home with the per-shard schema rows
    /// `build_recall_context_block` reads. Returns the kept tempdir, the
    /// path manager rooted under it, and the `ProjectId` for the
    /// fixture project. Caller can then assert against the rendered
    /// block.
    fn fixture_with_recall_data() -> (tempfile::TempDir, PathManager, ProjectId) {
        let dir = tempdir().expect("tempdir");
        let mneme_root = dir.path().join("mneme-home");
        std::fs::create_dir_all(&mneme_root).unwrap();
        let paths = PathManager::with_root(mneme_root);

        let project_root = dir.path().join("proj");
        std::fs::create_dir_all(project_root.join(".git")).unwrap();
        let project_id = ProjectId::from_path(&project_root).unwrap();

        // Per-project shard dir that PathManager::shard_db expects.
        std::fs::create_dir_all(paths.project_root(&project_id)).unwrap();

        // history.db with two seeded turns.
        let history =
            rusqlite::Connection::open(paths.shard_db(&project_id, DbLayer::History)).unwrap();
        history
            .execute_batch(
                "CREATE TABLE turns (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    timestamp TEXT NOT NULL,
                    token_count INTEGER,
                    extra TEXT NOT NULL DEFAULT '{}'
                 );",
            )
            .unwrap();
        history
            .execute(
                "INSERT INTO turns(session_id, role, content, timestamp) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "sd1-test-1",
                    "user",
                    "hello from sd1 test",
                    "2026-04-26 00:00:00"
                ],
            )
            .unwrap();
        history
            .execute(
                "INSERT INTO turns(session_id, role, content, timestamp) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params!["sd1-test-2", "user", "second prompt", "2026-04-26 00:01:00"],
            )
            .unwrap();

        // tasks.db with a decision + impl + an ignored bug entry.
        let tasks =
            rusqlite::Connection::open(paths.shard_db(&project_id, DbLayer::Tasks)).unwrap();
        tasks
            .execute_batch(
                "CREATE TABLE ledger_entries (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    timestamp INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    rationale TEXT,
                    touched_files TEXT NOT NULL DEFAULT '[]',
                    touched_concepts TEXT NOT NULL DEFAULT '[]',
                    transcript_ref TEXT,
                    kind_payload TEXT NOT NULL,
                    embedding BLOB
                 );",
            )
            .unwrap();
        tasks
            .execute(
                "INSERT INTO ledger_entries(id, session_id, timestamp, kind, summary, rationale, kind_payload) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "id-decision-1",
                    "s1",
                    1_000_i64,
                    "decision",
                    "use AES-256-GCM",
                    "FIPS-compliant",
                    "{}"
                ],
            )
            .unwrap();
        tasks
            .execute(
                "INSERT INTO ledger_entries(id, session_id, timestamp, kind, summary, rationale, kind_payload) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "id-impl-1",
                    "s1",
                    2_000_i64,
                    "impl",
                    "wired AES-GCM into store",
                    Option::<String>::None,
                    "{}"
                ],
            )
            .unwrap();
        tasks
            .execute(
                "INSERT INTO ledger_entries(id, session_id, timestamp, kind, summary, rationale, kind_payload) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "id-bug-1",
                    "s1",
                    3_000_i64,
                    "bug",
                    "this should be filtered out",
                    Option::<String>::None,
                    "{}"
                ],
            )
            .unwrap();

        // memory.db with two file_intent rows.
        let memory =
            rusqlite::Connection::open(paths.shard_db(&project_id, DbLayer::Memory)).unwrap();
        memory
            .execute_batch(
                "CREATE TABLE file_intent (
                    file_path TEXT PRIMARY KEY,
                    intent TEXT NOT NULL,
                    reason TEXT,
                    source TEXT NOT NULL DEFAULT 'unknown',
                    confidence REAL NOT NULL DEFAULT 1.0,
                    annotated_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );",
            )
            .unwrap();
        memory
            .execute(
                "INSERT INTO file_intent(file_path, intent, reason) VALUES(?1, ?2, ?3)",
                rusqlite::params!["src/auth.ts", "frozen", "verbatim from VBA spec"],
            )
            .unwrap();
        memory
            .execute(
                "INSERT INTO file_intent(file_path, intent, reason) VALUES(?1, ?2, ?3)",
                rusqlite::params!["src/scratch.ts", "experimental", "WIP"],
            )
            .unwrap();

        (dir, paths, project_id)
    }

    #[test]
    fn extract_file_tokens_picks_up_paths_and_extensions() {
        let toks = extract_file_tokens("please review src/auth.ts and inject.rs.");
        assert!(toks.contains(&"src/auth.ts".to_string()));
        assert!(toks.contains(&"inject.rs".to_string()));
    }

    #[test]
    fn extract_file_tokens_ignores_plain_words() {
        let toks = extract_file_tokens("hello world this has no files");
        assert!(toks.is_empty(), "unexpected tokens: {toks:?}");
    }

    #[test]
    fn extract_file_tokens_strips_surrounding_punctuation() {
        let toks = extract_file_tokens("see (`src/auth.ts`) for details.");
        assert!(toks.contains(&"src/auth.ts".to_string()));
    }

    #[test]
    fn build_recall_context_block_emits_all_three_subsections() {
        let (_keep, paths, project_id) = fixture_with_recall_data();
        let block = build_recall_context_block(&paths, &project_id, "look at src/auth.ts please");
        assert!(
            !block.is_empty(),
            "expected a non-empty block from a seeded fixture"
        );
        assert!(block.starts_with("<mneme-context>"));
        assert!(block.ends_with("</mneme-context>"));
        // Recent turns subsection
        assert!(block.contains("Recent turns"));
        assert!(block.contains("[user, sd1-test-1] hello from sd1 test"));
        assert!(block.contains("[user, sd1-test-2] second prompt"));
        // Ledger subsection — decision + impl included; bug filtered.
        assert!(block.contains("Recent decisions"));
        assert!(block.contains("[decision] use AES-256-GCM - FIPS-compliant"));
        assert!(block.contains("[impl] wired AES-GCM into store"));
        assert!(
            !block.contains("this should be filtered out"),
            "bug rows must NOT appear in the recall block"
        );
        // File intent subsection — only the prompt-mentioned file fires.
        assert!(block.contains("File intent"));
        assert!(block.contains("src/auth.ts: frozen - verbatim from VBA spec"));
        assert!(
            !block.contains("src/scratch.ts"),
            "non-mentioned files must not surface"
        );
    }

    #[test]
    fn build_recall_context_block_returns_empty_on_missing_shards() {
        let dir = tempdir().expect("tempdir");
        let mneme_root = dir.path().join("mneme-home");
        std::fs::create_dir_all(&mneme_root).unwrap();
        let paths = PathManager::with_root(mneme_root);
        let project_root = dir.path().join("never-built");
        std::fs::create_dir_all(project_root.join(".git")).unwrap();
        let project_id = ProjectId::from_path(&project_root).unwrap();

        let block = build_recall_context_block(&paths, &project_id, "any prompt");
        assert!(
            block.is_empty(),
            "expected empty block for a never-built project, got: {block:?}"
        );
    }

    #[test]
    fn build_recall_context_block_does_not_panic_on_empty_prompt() {
        let (_keep, paths, project_id) = fixture_with_recall_data();
        // Empty prompt -> no file tokens -> no file_intent subsection,
        // but turns + ledger should still render.
        let block = build_recall_context_block(&paths, &project_id, "");
        assert!(!block.is_empty());
        assert!(block.contains("Recent turns"));
        assert!(block.contains("Recent decisions"));
        assert!(
            !block.contains("File intent"),
            "no file tokens means no file_intent subsection"
        );
    }

    #[test]
    fn cap_block_size_truncates_oversized_input() {
        let big = "x".repeat(10_000);
        let capped = cap_block_size(big, 1_000);
        assert!(capped.len() <= 1_000 + "\n... [recall truncated]".len());
        assert!(capped.ends_with("[recall truncated]"));
    }

    #[test]
    fn cap_block_size_passes_small_input_through() {
        let small = "tiny block".to_string();
        let capped = cap_block_size(small.clone(), 1_000);
        assert_eq!(capped, small);
    }

    #[test]
    fn build_recall_context_block_respects_byte_cap() {
        let (_keep, paths, project_id) = fixture_with_recall_data();
        let block = build_recall_context_block(&paths, &project_id, "src/auth.ts");
        assert!(
            block.len() <= RECALL_BLOCK_BYTE_CAP + 64,
            "block exceeded byte cap: len={}",
            block.len()
        );
    }
}
