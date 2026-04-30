//! Anthropic Claude Code adapter.
//!
//! Manifest: `CLAUDE.md` (optional @include line, opt-in via `mneme link-claude-md`)
//! MCP:      `~/.claude.json` (user-scope) or `.mcp.json` (project-scope)
//! Hooks:    `~/.claude/settings.json` (registered BY DEFAULT in v0.3.2 — K1 fix.
//!           Pass `--no-hooks` / `--skip-hooks` to `mneme install` to opt out.)
//!
//! Architecture note (v0.3.2 — 2026-04-26)
//! ========================================
//! Prior to v0.3.1 mneme wrote an 8-event hook map into
//! `~/.claude/settings.json`. That code emitted a flat `{command, owner}`
//! shape which Claude Code's schema validator rejected, causing the
//! validator to discard the entire file (not just the malformed entries).
//! Every unrelated hook, permission, and plugin the user had configured
//! became silently inert on next boot.
//!
//! The self-trap was amplified because mneme's hook binaries required
//! `--tool / --params / --session-id` CLI flags, while Claude Code delivers
//! payload on STDIN as JSON. Every PreToolUse call exited non-zero, which
//! Claude Code correctly interpreted as BLOCK — locking the agent out of
//! every tool, including the ones it needed to roll mneme back.
//!
//! v0.3.1 fix: did not register hooks at all (no-op).
//!
//! v0.3.2 fix (this file, K1): hook registration is **DEFAULT-ON** —
//! `mneme install` writes the 8 hook entries under
//! `~/.claude/settings.json::hooks` automatically. Without those hooks
//! the persistent-memory pipeline (history.db, tasks.db, tool_cache.db,
//! livestate.db) stays empty and mneme degrades to a query-only MCP
//! surface — exactly the keystone Phase A bug. Pass `--no-hooks` /
//! `--skip-hooks` to opt out. The five RULE-18 prerequisites are
//! satisfied:
//!
//!   (a) hook JSON shape — we emit the canonical Claude Code schema:
//!       `{ "matcher": "*", "hooks": [{ "type": "command", "command": "..." }] }`
//!       wrapped per event under the top-level `hooks` key.
//!   (b) hook STDIN contract — every mneme hook binary reads payload
//!       from STDIN via `crate::hook_payload::read_stdin_payload`.
//!   (c) rollback receipt — `commands/install.rs` records a
//!       `FileModified` action with sha256_before/after, plus the
//!       timestamped `.mneme-*.bak` snapshot lives next to the file.
//!   (d) `settings.json` lock — `claude_code_likely_running()` in
//!       `commands/install.rs` warns if Claude Code is open.
//!   (e) escape hatch — `--no-hooks` / `--skip-hooks` is the explicit
//!       opt-out. Default install registers the 8 hooks (K1).
//!       `mneme uninstall` reverses the registration via the same
//!       marker block.
//!
//! Marker pattern (JSON-flavoured)
//! -------------------------------
//! Plain `<!-- mneme-start -->` HTML comments are illegal inside JSON.
//! Instead, every mneme-managed hook entry carries a sibling
//! `"_mneme": { "version": "1.0", "managed": true }` field. On
//! `mneme uninstall` we walk every event array under `hooks`, drop any
//! entry whose innermost command list contains a mneme-marked command,
//! and prune empty parent arrays. Surrounding non-mneme hooks (the user's
//! own custom hooks, plugin hooks) are preserved verbatim.
//!
//! See `report-002.md §F-011 / §F-012` in the mneme install-report for
//! the forensic record of the v0.3.0 incident this fix prevents.

use std::path::{Path, PathBuf};

use crate::error::{CliError, CliResult};
use crate::platforms::{
    backup_then_write, AdapterContext, InstallScope, McpFormat, Platform, PlatformAdapter,
};

/// Marker stamped onto every mneme-managed hook entry. Lets `uninstall`
/// find exactly the entries we own without touching the user's other
/// hooks.
pub const HOOK_MARKER_KEY: &str = "_mneme";

/// Hook marker version — bump this if we change the hook entry shape
/// in a breaking way.
pub const HOOK_MARKER_VERSION: &str = "1.0";

/// One Claude Code hook event mneme registers.
///
/// `event` is the top-level key under `hooks` in `settings.json`.
/// `args` are the CLI args appended to `mneme(.exe)` to invoke the
/// matching hook binary. `matcher` is the Claude Code event matcher
/// pattern; `*` means "every invocation of this event".
#[derive(Debug, Clone, Copy)]
pub struct HookSpec {
    /// Claude Code event name (e.g. `"PreToolUse"`).
    pub event: &'static str,
    /// Args appended to the mneme exe (e.g. `&["pre-tool"]`).
    pub args: &'static [&'static str],
    /// Matcher pattern. Use `"*"` for catch-all.
    pub matcher: &'static str,
}

/// The 8 hook events mneme registers by default (K1 fix, v0.3.2).
/// Skipped only when the user passes `--no-hooks` / `--skip-hooks`.
/// Order matches the install banner so users can audit one-to-one.
pub const HOOK_SPECS: &[HookSpec] = &[
    HookSpec {
        event: "SessionStart",
        args: &["session-prime"],
        matcher: "*",
    },
    HookSpec {
        event: "UserPromptSubmit",
        args: &["inject"],
        matcher: "*",
    },
    HookSpec {
        event: "PreToolUse",
        args: &["pre-tool"],
        matcher: "*",
    },
    HookSpec {
        event: "PostToolUse",
        args: &["post-tool"],
        matcher: "*",
    },
    HookSpec {
        event: "Stop",
        args: &["turn-end"],
        matcher: "*",
    },
    HookSpec {
        event: "PreCompact",
        args: &["turn-end", "--pre-compact"],
        matcher: "*",
    },
    HookSpec {
        event: "SubagentStop",
        args: &["turn-end", "--subagent"],
        matcher: "*",
    },
    HookSpec {
        event: "SessionEnd",
        args: &["session-end"],
        matcher: "*",
    },
];

/// Adapter struct (zero-sized; all behaviour is in the impl).
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCode;

impl PlatformAdapter for ClaudeCode {
    fn platform(&self) -> Platform {
        Platform::ClaudeCode
    }

    /// Per design §21.4.2, Claude Code is "always tried" — the detector
    /// returns true unconditionally so even fresh users get configured.
    fn detect(&self, _ctx: &AdapterContext) -> bool {
        true
    }

    /// Manifest location — v0.3.1 fix for F-008 / F-017.
    ///
    /// Claude Code reads user-scope instructions from `~/.claude/CLAUDE.md`.
    /// Earlier mneme versions wrote to `~/CLAUDE.md` (the user's home
    /// directory root), which is a different file — it's only loaded by
    /// Claude Code when it happens to be the current working directory
    /// project file. That caused the manifest to be picked up
    /// inconsistently and left a stray `CLAUDE.md` in user homes across
    /// projects. Correct target is `~/.claude/CLAUDE.md`.
    ///
    /// Project-scope stays `<project_root>/CLAUDE.md` (that IS the
    /// correct per-project file).
    fn manifest_path(&self, ctx: &AdapterContext) -> PathBuf {
        match ctx.scope {
            InstallScope::Project => ctx.project_root.join("CLAUDE.md"),
            InstallScope::User | InstallScope::Global => {
                ctx.home.join(".claude").join("CLAUDE.md")
            }
        }
    }

    fn mcp_config_path(&self, ctx: &AdapterContext) -> PathBuf {
        match ctx.scope {
            InstallScope::Project => ctx.project_root.join(".mcp.json"),
            InstallScope::User | InstallScope::Global => ctx.home.join(".claude.json"),
        }
    }

    fn mcp_format(&self) -> McpFormat {
        McpFormat::JsonObject
    }

    /// Claude Code stores hooks in `~/.claude/settings.json` (user scope)
    /// or `<project>/.claude/settings.json` (project scope).
    fn hooks_path(&self, ctx: &AdapterContext) -> Option<PathBuf> {
        Some(match ctx.scope {
            InstallScope::Project => {
                ctx.project_root.join(".claude").join("settings.json")
            }
            InstallScope::User | InstallScope::Global => {
                ctx.home.join(".claude").join("settings.json")
            }
        })
    }

    /// Registers the 8-event mneme hook map into `~/.claude/settings.json`.
    ///
    /// **Default ON (K1 fix, v0.3.2).** Returns `Ok(None)` only if the
    /// user explicitly opts out via `mneme install --no-hooks` /
    /// `--skip-hooks`. Without these hooks the persistent-memory
    /// pipeline is unreachable — exactly the keystone Phase A bug.
    ///
    /// Every entry mneme writes carries the `_mneme` marker (see
    /// [`HOOK_MARKER_KEY`]) so `remove_hooks` can find and strip exactly
    /// the entries we own without touching user-owned hooks.
    fn write_hooks(&self, ctx: &AdapterContext) -> CliResult<Option<PathBuf>> {
        if !ctx.enable_hooks {
            return Ok(None);
        }
        let path = match self.hooks_path(ctx) {
            Some(p) => p,
            None => return Ok(None),
        };
        write_hooks_json_marker(&path, &ctx.exe_path, ctx)?;
        Ok(Some(path))
    }

    /// Inverse of [`Self::write_hooks`]. Strips every mneme-marked entry
    /// from the hooks tree and prunes empty parent arrays. Always runs
    /// (regardless of `enable_hooks`) so a stray previous registration
    /// can be cleaned up by `mneme uninstall` even if the current
    /// invocation doesn't carry the flag.
    fn remove_hooks(&self, ctx: &AdapterContext) -> CliResult<()> {
        let path = match self.hooks_path(ctx) {
            Some(p) => p,
            None => return Ok(()),
        };
        if !path.exists() {
            return Ok(());
        }
        remove_hooks_json_marker(&path, ctx)
    }
}

/// Build the JSON value mneme writes for a single hook entry.
///
/// Shape (Claude Code schema):
/// ```json
/// {
///   "matcher": "*",
///   "hooks": [
///     { "type": "command", "command": "<abs path to mneme(.exe)> <args...>" }
///   ],
///   "_mneme": { "version": "1.0", "managed": true }
/// }
/// ```
///
/// The `_mneme` sibling field is the marker `remove_hooks` keys off — it
/// distinguishes mneme-owned entries from user-owned entries that happen
/// to share the same matcher. Claude Code ignores unknown keys at this
/// level (verified against the published schema 2026-04-26).
fn build_hook_entry(spec: &HookSpec, exe_path: &Path) -> serde_json::Value {
    // K1+v0.3.2 fix: emit forward-slash paths on Windows. Claude Code
    // shells hook commands through bash on every platform (including
    // Windows-via-Git-Bash), and bash treats `C:\Users\…` as escape
    // sequences, mangling `\U` → `U`, `\A` → `A`, etc. The result is
    // `C:UsersAdministrator.mnemebinmneme.exe` — "command not found".
    // Forward slashes work in BOTH cmd.exe and bash on Windows, and
    // are obviously fine on POSIX.
    let mut command = exe_path.to_string_lossy().replace('\\', "/");
    for a in spec.args {
        command.push(' ');
        command.push_str(a);
    }
    serde_json::json!({
        "matcher": spec.matcher,
        "hooks": [
            {
                "type": "command",
                "command": command,
                // HOOK-CANCELLED-002 (v0.3.2): give every mneme hook a
                // generous 30s timeout. The actual hook returns in <50 ms
                // for SessionEnd (detached fire-and-forget) and <500 ms
                // for the others (HOOK_CTX_BUDGET + HOOK_IPC_BUDGET caps).
                // 30s is the safety net for cold-start shard creation on
                // the first SessionEnd ever — Claude Code's default
                // SessionEnd window for `claude --print` is otherwise
                // shorter than our HookCtx::resolve, producing cosmetic
                // "Hook cancelled" stderr noise even when the hook IS
                // doing the right thing in the background.
                "timeout": 30,
            }
        ],
        HOOK_MARKER_KEY: {
            "version": HOOK_MARKER_VERSION,
            "managed": true,
        },
    })
}

/// Read-modify-write `~/.claude/settings.json` to inject mneme's 8 hook
/// entries. Idempotent: re-running replaces any prior mneme-marked entry
/// without touching user-owned siblings.
///
/// Atomic: writes go through `backup_then_write` -> `atomic_write` (REG-021).
pub fn write_hooks_json_marker(
    path: &Path,
    exe_path: &Path,
    ctx: &AdapterContext,
) -> CliResult<()> {
    if let Some(parent) = path.parent() {
        if !ctx.dry_run {
            std::fs::create_dir_all(parent).map_err(|e| CliError::io(parent, e))?;
        }
    }

    let existing = if path.exists() {
        std::fs::read_to_string(path).map_err(|e| CliError::io(path, e))?
    } else {
        String::new()
    };

    let merged = inject_hooks_json(&existing, exe_path)?;

    if ctx.dry_run {
        tracing::info!(
            path = %path.display(),
            bytes = merged.len(),
            "dry-run: would write hooks JSON"
        );
        return Ok(());
    }

    backup_then_write(path, merged.as_bytes())?;
    Ok(())
}

/// Pure-function variant of `write_hooks_json_marker` body — takes the
/// existing file contents and returns the merged JSON text. Split out so
/// it's unit-testable without touching the filesystem.
pub fn inject_hooks_json(existing: &str, exe_path: &Path) -> CliResult<String> {
    let trimmed = strip_bom(existing);

    let mut value: serde_json::Value = if trimmed.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(trimmed)?
    };

    let root = value
        .as_object_mut()
        .ok_or_else(|| CliError::Other("settings.json root is not a JSON object".into()))?;

    // Get-or-create top-level "hooks" object.
    let hooks_entry = root
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks_entry
        .as_object_mut()
        .ok_or_else(|| CliError::Other("settings.json `hooks` is not a JSON object".into()))?;

    // For each event mneme registers: drop any prior mneme-marked entry
    // for that event, then append the freshly-built entry. User-owned
    // entries (those without our marker) are preserved verbatim.
    for spec in HOOK_SPECS {
        let arr_entry = hooks_obj
            .entry(spec.event.to_string())
            .or_insert_with(|| serde_json::json!([]));
        let arr = arr_entry.as_array_mut().ok_or_else(|| {
            CliError::Other(format!(
                "settings.json `hooks.{}` is not a JSON array",
                spec.event
            ))
        })?;

        // Drop prior mneme entries for this event (idempotent re-install).
        arr.retain(|v| !is_mneme_marked_entry(v));

        // Append fresh entry.
        arr.push(build_hook_entry(spec, exe_path));
    }

    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// Read-modify-write inverse: strip every mneme-marked entry from the
/// `hooks` tree, prune empty parent arrays, atomic-write the result.
pub fn remove_hooks_json_marker(path: &Path, ctx: &AdapterContext) -> CliResult<()> {
    let existing = std::fs::read_to_string(path).map_err(|e| CliError::io(path, e))?;
    let stripped = strip_hooks_json(&existing)?;
    if ctx.dry_run {
        tracing::info!(
            path = %path.display(),
            bytes = stripped.len(),
            "dry-run: would strip mneme hook entries"
        );
        return Ok(());
    }
    // backup_then_write keeps a `.bak` snapshot in case the user wants
    // to recover their pre-uninstall state. atomic_write underneath.
    backup_then_write(path, stripped.as_bytes())?;
    Ok(())
}

/// Pure-function variant of `remove_hooks_json_marker` body. Returns the
/// settings.json text with every mneme-marked hook entry removed and any
/// empty event arrays pruned.
pub fn strip_hooks_json(existing: &str) -> CliResult<String> {
    let trimmed = strip_bom(existing);
    if trimmed.trim().is_empty() {
        return Ok(existing.to_string());
    }
    let mut value: serde_json::Value = serde_json::from_str(trimmed)?;
    let root = match value.as_object_mut() {
        Some(o) => o,
        // Not an object — nothing we can clean. Leave verbatim.
        None => return Ok(existing.to_string()),
    };

    let hooks_obj = match root.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        Some(o) => o,
        None => return Ok(existing.to_string()),
    };

    let mut emptied: Vec<String> = Vec::new();
    for (event_name, arr_val) in hooks_obj.iter_mut() {
        if let Some(arr) = arr_val.as_array_mut() {
            arr.retain(|v| !is_mneme_marked_entry(v));
            if arr.is_empty() {
                emptied.push(event_name.clone());
            }
        }
    }
    // Prune empty arrays so we leave settings.json clean.
    for k in emptied {
        hooks_obj.remove(&k);
    }
    // If the whole `hooks` object is now empty, prune it too.
    if hooks_obj.is_empty() {
        root.remove("hooks");
    }

    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// True iff `entry` is a hook entry mneme owns. We key off the `_mneme`
/// sibling marker — never off command text, since the user might have a
/// hook that happens to invoke `mneme`.
fn is_mneme_marked_entry(entry: &serde_json::Value) -> bool {
    entry
        .get(HOOK_MARKER_KEY)
        .and_then(|m| m.as_object())
        .and_then(|o| o.get("managed"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// B-012: fallback recognizer for hook entries written by older mneme
/// installs (pre-marker era) OR hand-edited by users who lost the
/// `_mneme.managed=true` marker. Walks the entry's `hooks` array
/// looking for an inner hook whose `command` string mentions a path
/// segment ending in `/.mneme/bin/mneme[.exe]` AND whose argv tail
/// matches the spec's `args` (e.g. `["session-prime"]`).
///
/// Without this, `mneme doctor` falsely reported `0/8` for users whose
/// hooks were registered correctly but lacked the marker — a cosmetic
/// regression that scared users into believing their install was
/// broken when it wasn't.
fn entry_command_matches_spec(entry: &serde_json::Value, spec: &HookSpec) -> bool {
    let inner = match entry.get("hooks").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return false,
    };
    for h in inner {
        let cmd = match h.get("command").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        // Path heuristic: must reference a `mneme[.exe]` binary under
        // a `.mneme/bin` directory (matches install.ps1's exe-path
        // template). Use case-insensitive contains because Windows
        // paths can vary `C:\Users\...` vs `c:\users\...`.
        let cmd_lc = cmd.to_lowercase();
        let path_match = cmd_lc.contains("\\.mneme\\bin\\mneme")
            || cmd_lc.contains("/.mneme/bin/mneme");
        if !path_match {
            continue;
        }
        // Argv tail heuristic: every arg in `spec.args` must appear in
        // the command string, in order (consecutive whitespace
        // separation). Cheap substring check is enough here — the
        // path-match above already filtered to mneme.exe invocations.
        let needle = spec.args.join(" ");
        if cmd.contains(&needle) {
            return true;
        }
    }
    false
}

/// Strip a UTF-8 BOM (EF BB BF) from the head of a string, if present.
/// PowerShell's `Set-Content -Encoding UTF8` adds a BOM that makes
/// `serde_json::from_str` fail; same defensive approach as
/// `platforms::mod::strip_bom`.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// File-level state for the `~/.claude/settings.json` probe — separates
/// "the file is genuinely absent" from "we read bytes but couldn't make
/// sense of them" so callers (`mneme doctor`) can give the user a real
/// reason instead of a silent `0/8`.
///
/// B-AGENT-C-1 (v0.3.2): the previous `count_registered_mneme_hooks`
/// collapsed every error path into `(0, expected)` with no diagnostic.
/// When Claude Code rewrote `settings.json` mid-doctor (overwriting
/// mneme's entries with its in-memory copy that lacked them), the user
/// got `0/8` and no clue why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookFileReadState {
    /// The settings.json file does not exist on disk. This is the
    /// "fresh user, never installed" state — distinct from "install
    /// happened then file got clobbered".
    Missing,
    /// File exists but `std::fs::read` returned an error (locked, no
    /// permissions, mid-write share violation on Windows). The string
    /// payload is the io::Error display.
    UnreadableIo(String),
    /// File exists, bytes read OK. Used both when JSON parsed cleanly
    /// AND when parse failed (the `parse_error` field on the parent
    /// result tells the parse story separately).
    Read,
}

/// Detailed result of probing `~/.claude/settings.json` for mneme
/// hooks. Replacement / refinement for `count_registered_mneme_hooks`'s
/// silent-zero behaviour.
///
/// All fields are pub so `mneme doctor` (and tests) can reason over
/// them without going through extra accessors.
#[derive(Debug, Clone)]
pub struct HookCountResult {
    /// Number of mneme-owned hook events found in the file.
    pub count: usize,
    /// Total number of events mneme registers (currently 8 — the
    /// `HOOK_SPECS` length).
    pub expected: usize,
    /// File-level outcome. `Missing` and `UnreadableIo` short-circuit
    /// before parsing.
    pub read_state: HookFileReadState,
    /// Filled when parsing or schema validation failed AFTER reading
    /// bytes. Surfaces the concrete reason so doctor can say "json
    /// parse failed: trailing comma at line 12" instead of silent 0/8.
    pub parse_error: Option<String>,
}

/// Detailed variant of [`count_registered_mneme_hooks`] — every error
/// path is surfaced via [`HookCountResult`] so the caller can render an
/// honest diagnostic.
///
/// B-AGENT-C-1 (v0.3.2): Anish's reproduction was `mneme install` →
/// (Claude Code, still running, auto-saves its in-memory settings.json,
/// stripping mneme's entries) → `mneme doctor` reports `0/8` with no
/// clue. The previous helper swallowed io / utf-8 / json failures and
/// returned `(0, expected)` indistinguishably from the "no hooks
/// present" case. This variant lets `mneme doctor` distinguish them
/// and tell the user the truth.
pub fn count_registered_mneme_hooks_detailed(path: &Path) -> HookCountResult {
    let expected = HOOK_SPECS.len();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return HookCountResult {
                count: 0,
                expected,
                read_state: HookFileReadState::Missing,
                parse_error: None,
            };
        }
        Err(e) => {
            return HookCountResult {
                count: 0,
                expected,
                read_state: HookFileReadState::UnreadableIo(e.to_string()),
                parse_error: None,
            };
        }
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => {
            return HookCountResult {
                count: 0,
                expected,
                read_state: HookFileReadState::Read,
                parse_error: Some(format!("settings.json is not valid UTF-8: {e}")),
            };
        }
    };
    let trimmed = strip_bom(text);
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            return HookCountResult {
                count: 0,
                expected,
                read_state: HookFileReadState::Read,
                parse_error: Some(format!("settings.json failed to parse as JSON: {e}")),
            };
        }
    };
    let hooks = match value.get("hooks").and_then(|v| v.as_object()) {
        Some(h) => h,
        None => {
            return HookCountResult {
                count: 0,
                expected,
                read_state: HookFileReadState::Read,
                parse_error: None,
            };
        }
    };

    let mut count = 0usize;
    for spec in HOOK_SPECS {
        if let Some(arr) = hooks.get(spec.event).and_then(|v| v.as_array()) {
            // B-012: count an event as "registered" if EITHER
            // (a) any matcher entry has the `_mneme.managed=true` marker
            //     (the new path), OR
            // (b) any matcher entry's inner `hooks[].command` references
            //     `~/.mneme/bin/mneme[.exe]` with the spec's argv tail
            //     (back-compat for installs that pre-date the marker
            //     scheme OR for users who hand-edited and lost the marker
            //     while preserving the actual command). Without (b),
            //     `mneme doctor` would report 0/8 even when all 8 hooks
            //     are functional, which is a cosmetic false-negative
            //     that scared users into bogus reinstalls.
            let registered = arr.iter().any(|e| {
                is_mneme_marked_entry(e) || entry_command_matches_spec(e, spec)
            });
            if registered {
                count += 1;
            }
        }
    }
    HookCountResult {
        count,
        expected,
        read_state: HookFileReadState::Read,
        parse_error: None,
    }
}

/// Probe an existing `settings.json` and report (mneme_entry_count,
/// expected_count). Compatibility wrapper around
/// [`count_registered_mneme_hooks_detailed`] for callers that only need
/// the headline counts. New code should prefer the detailed variant —
/// it surfaces concrete read / parse errors instead of collapsing them
/// into a silent zero.
///
/// Returns `(0, HOOK_SPECS.len())` if the file is missing, unreadable,
/// or the hooks block is absent.
pub fn count_registered_mneme_hooks(path: &Path) -> (usize, usize) {
    let r = count_registered_mneme_hooks_detailed(path);
    (r.count, r.expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn fake_exe() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\test\.mneme\bin\mneme.exe")
        } else {
            PathBuf::from("/home/test/.mneme/bin/mneme")
        }
    }

    #[test]
    fn write_hooks_no_op_when_not_enabled() {
        let dir = tempdir().unwrap();
        let ctx = AdapterContext::new(InstallScope::User, dir.path().to_path_buf());
        // K1 (v0.3.2): the high-level `commands/install.rs` flips this
        // to `true` by default via `.with_enable_hooks(!args.skip_hooks)`.
        // The low-level `AdapterContext::new` default stays `false` so
        // adapters that don't go through the CLI (tests, raw API
        // consumers) get a predictable no-op when they don't opt in.
        assert!(!ctx.enable_hooks);
        let result = ClaudeCode.write_hooks(&ctx).unwrap();
        assert!(
            result.is_none(),
            "raw AdapterContext with enable_hooks=false must not register hooks"
        );
    }

    #[test]
    fn inject_hooks_json_into_empty_file() {
        let exe = fake_exe();
        let merged = inject_hooks_json("", &exe).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        let hooks = parsed["hooks"].as_object().unwrap();
        // All 8 events registered.
        for spec in HOOK_SPECS {
            assert!(hooks.contains_key(spec.event), "missing event {}", spec.event);
            let arr = hooks[spec.event].as_array().unwrap();
            assert_eq!(arr.len(), 1, "exactly one entry per event");
            assert!(is_mneme_marked_entry(&arr[0]));
            // Command field must be the absolute exe path + args.
            // K1+v0.3.2 fix: paths are emitted forward-slash for bash
            // compatibility on Windows; compare against the normalised
            // form rather than the raw OsStr.
            let cmd = arr[0]["hooks"][0]["command"].as_str().unwrap();
            let exe_norm = exe.to_string_lossy().replace('\\', "/");
            assert!(cmd.contains(&exe_norm), "cmd `{}` missing exe `{}`", cmd, exe_norm);
            for a in spec.args {
                assert!(cmd.contains(a), "{}'s command lost arg {}", spec.event, a);
            }
        }
    }

    #[test]
    fn inject_hooks_json_preserves_user_hooks() {
        let exe = fake_exe();
        let starting = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            { "type": "command", "command": "echo user-hook" }
                        ]
                    }
                ]
            }
        }"#;
        let merged = inject_hooks_json(starting, &exe).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&merged).unwrap();
        let pre = parsed["hooks"]["PreToolUse"].as_array().unwrap();
        // User's hook must survive; mneme's hook must be appended.
        assert_eq!(pre.len(), 2);
        assert!(pre.iter().any(|v| {
            v.get("matcher").and_then(|m| m.as_str()) == Some("Bash")
                && !is_mneme_marked_entry(v)
        }));
        assert!(pre.iter().any(is_mneme_marked_entry));
    }

    #[test]
    fn inject_hooks_json_idempotent_on_re_install() {
        let exe = fake_exe();
        let once = inject_hooks_json("", &exe).unwrap();
        let twice = inject_hooks_json(&once, &exe).unwrap();
        // Re-injecting must not duplicate mneme entries.
        let parsed: serde_json::Value = serde_json::from_str(&twice).unwrap();
        for spec in HOOK_SPECS {
            let arr = parsed["hooks"][spec.event].as_array().unwrap();
            let mneme_count = arr.iter().filter(|v| is_mneme_marked_entry(v)).count();
            assert_eq!(
                mneme_count, 1,
                "double-injection must leave exactly one mneme entry for {}",
                spec.event
            );
        }
    }

    #[test]
    fn strip_hooks_json_removes_only_mneme_entries() {
        let exe = fake_exe();
        let starting = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user" }] }
                ]
            }
        }"#;
        let injected = inject_hooks_json(starting, &exe).unwrap();
        let stripped = strip_hooks_json(&injected).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        // User's hook still present.
        let pre = parsed["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert!(!is_mneme_marked_entry(&pre[0]));
        // Events that only had mneme entries are pruned entirely.
        assert!(parsed["hooks"].get("SessionStart").is_none());
    }

    #[test]
    fn strip_hooks_json_prunes_empty_hooks_object() {
        let exe = fake_exe();
        let injected = inject_hooks_json("", &exe).unwrap();
        let stripped = strip_hooks_json(&injected).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stripped).unwrap();
        // No user hooks were present, so the whole `hooks` key should be gone.
        assert!(parsed.get("hooks").is_none());
    }

    #[test]
    fn strip_hooks_json_handles_bom() {
        let exe = fake_exe();
        let injected = inject_hooks_json("", &exe).unwrap();
        let with_bom = format!("\u{feff}{}", injected);
        // Should not error.
        let _ = strip_hooks_json(&with_bom).unwrap();
    }

    #[test]
    fn write_hooks_returns_path_when_enabled() {
        let dir = tempdir().unwrap();
        let ctx = AdapterContext::new(InstallScope::User, dir.path().to_path_buf())
            .with_enable_hooks(true);
        // The settings.json path resolves under ctx.home, which is the
        // process's actual home dir. We use scope=Project to redirect.
        let project_ctx = AdapterContext::new(InstallScope::Project, dir.path().to_path_buf())
            .with_enable_hooks(true)
            .with_exe_path(fake_exe());
        let result = ClaudeCode.write_hooks(&project_ctx).unwrap();
        assert!(result.is_some());
        // File must exist + parse + contain mneme entries.
        let written = result.unwrap();
        assert!(written.exists(), "settings.json should have been created");
        let contents = std::fs::read_to_string(&written).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        for spec in HOOK_SPECS {
            assert!(parsed["hooks"][spec.event].is_array());
        }
        // Avoid touching the User scope in tests — the user-scope ctx is
        // here so the test compiler still type-checks the Builder API.
        let _ = ctx;
    }

    #[test]
    fn count_registered_mneme_hooks_reports_zero_on_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let (n, expected) = count_registered_mneme_hooks(&path);
        assert_eq!(n, 0);
        assert_eq!(expected, HOOK_SPECS.len());
    }

    #[test]
    fn count_registered_mneme_hooks_reports_full_after_install() {
        let exe = fake_exe();
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, inject_hooks_json("", &exe).unwrap()).unwrap();
        let (n, expected) = count_registered_mneme_hooks(&path);
        assert_eq!(n, expected);
    }

    // -----------------------------------------------------------------
    // B-AGENT-C-1 (v0.3.2): the silent-zero error path in
    // `count_registered_mneme_hooks` is the bug Anish hit when Claude
    // Code rewrote settings.json mid-doctor. Every io / utf-8 / json
    // failure historically returned `(0, expected)` with no diagnostic.
    // The detailed variant surfaces the concrete reason so doctor can
    // distinguish "no hooks present" from "could not read the file"
    // from "JSON failed to parse" from "schema shape mismatched".
    // Tests pin the new contract.
    // -----------------------------------------------------------------

    #[test]
    fn count_registered_detailed_reports_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let r = count_registered_mneme_hooks_detailed(&path);
        assert_eq!(r.count, 0);
        assert_eq!(r.expected, HOOK_SPECS.len());
        assert!(matches!(r.read_state, HookFileReadState::Missing));
        assert!(r.parse_error.is_none());
    }

    #[test]
    fn count_registered_detailed_reports_malformed_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ this is not valid json ").unwrap();
        let r = count_registered_mneme_hooks_detailed(&path);
        assert_eq!(r.count, 0);
        assert!(matches!(r.read_state, HookFileReadState::Read));
        assert!(
            r.parse_error.is_some(),
            "malformed JSON must surface a parse_error rather than silently returning 0/8"
        );
    }

    #[test]
    fn count_registered_detailed_reports_invalid_utf8() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Lone continuation byte — not valid UTF-8.
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x80, 0x80]).unwrap();
        let r = count_registered_mneme_hooks_detailed(&path);
        assert_eq!(r.count, 0);
        assert!(matches!(r.read_state, HookFileReadState::Read));
        assert!(
            r.parse_error.is_some(),
            "invalid UTF-8 must surface as a parse_error, not a silent zero"
        );
    }

    #[test]
    fn count_registered_detailed_full_install_reports_present() {
        let exe = fake_exe();
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, inject_hooks_json("", &exe).unwrap()).unwrap();
        let r = count_registered_mneme_hooks_detailed(&path);
        assert_eq!(r.count, HOOK_SPECS.len());
        assert_eq!(r.expected, HOOK_SPECS.len());
        assert!(matches!(r.read_state, HookFileReadState::Read));
        assert!(r.parse_error.is_none());
    }

    #[test]
    fn count_registered_detailed_recognises_b012_unmarked_entry() {
        // A Claude-Code-flavoured entry that lacks the `_mneme` marker
        // but DOES carry the canonical `~/.mneme/bin/mneme.exe` command
        // shape. B-012 fallback should still recognise it; the detailed
        // result must reflect that.
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let cmd = if cfg!(windows) {
            "C:/Users/test/.mneme/bin/mneme.exe session-prime"
        } else {
            "/home/test/.mneme/bin/mneme session-prime"
        };
        let body = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "matcher": "*",
                        "hooks": [{ "type": "command", "command": cmd }]
                    }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
        let r = count_registered_mneme_hooks_detailed(&path);
        // Only one event registered (SessionStart). The other 7 will
        // not match — but the one that does must be picked up via the
        // command-shape fallback, not silently dropped.
        assert_eq!(r.count, 1, "B-012 command-shape fallback must still trigger");
        assert!(r.parse_error.is_none());
    }

    #[test]
    fn hook_specs_match_the_eight_required_events() {
        // The 8 events Claude Code's hook surface defines that mneme
        // wires up. If this list grows we want the test to fail loudly
        // so the install banner gets updated to match.
        let events: Vec<&str> = HOOK_SPECS.iter().map(|h| h.event).collect();
        assert_eq!(
            events,
            vec![
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Stop",
                "PreCompact",
                "SubagentStop",
                "SessionEnd",
            ]
        );
    }
}
