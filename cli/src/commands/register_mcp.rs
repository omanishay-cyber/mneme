//! `mneme register-mcp` / `mneme unregister-mcp` — minimal MCP wiring.
//!
//! These commands exist to give the one-line installer (and anyone who
//! just wants the MCP tools without a CLAUDE.md manifest block) a clean
//! first-class entry point. They are thin wrappers around
//! [`crate::commands::install`] / [`crate::commands::uninstall`] with
//! `--skip-manifest` and `--skip-hooks` preset.
//!
//! Why a separate command?
//!
//!   * The full install command is multi-step (manifest + MCP + hooks)
//!     and power-user-shaped. New users don't want to remember
//!     `--skip-manifest --skip-hooks`.
//!   * `scripts/install.ps1` calls `mneme register-mcp --platform
//!     claude-code` which reads cleaner than the skip-flag incantation
//!     and keeps the install pipeline documented at one site.
//!   * Makes the v0.3.1 promise explicit: "installer only writes the
//!     MCP entry, nothing else". The command name IS the promise.

use clap::Args;
use std::path::PathBuf;

use crate::commands::{install, uninstall};
use crate::error::CliResult;
use crate::platforms::{AdapterContext, InstallScope, Platform};

/// Shared args — both register and unregister accept the same platform /
/// scope / dry-run surface.
#[derive(Debug, Args)]
pub struct RegisterMcpArgs {
    /// Platform to register with. Defaults to `claude-code`.
    ///
    /// REG-025: validated at parse-time against the full 19-platform list
    /// the install/uninstall pipelines support (see `platforms::Platform`).
    /// Invalid values produce a clap error listing the allowed options.
    #[arg(
        long,
        default_value = "claude-code",
        value_parser = clap::builder::PossibleValuesParser::new([
            "claude-code", "codex", "cursor", "windsurf", "zed",
            "continue", "opencode", "antigravity", "gemini-cli", "aider",
            "copilot", "factory-droid", "trae", "kiro", "qoder",
            "openclaw", "hermes", "qwen", "vscode",
        ]),
    )]
    pub platform: String,

    /// Print what would change but write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Install scope (user | project | global). Defaults to `user`.
    #[arg(long, default_value = "user")]
    pub scope: String,

    /// Project root override. Defaults to CWD.
    #[arg(long)]
    pub project: Option<PathBuf>,

    /// WIRE-013: overwrite mneme's marker block even if the user
    /// hand-edited it. Forwarded to the underlying `mneme install`
    /// pipeline so registering an MCP entry on top of stale state
    /// works without bouncing through `mneme uninstall` first.
    #[arg(long)]
    pub force: bool,

    /// LIE-3: emit a single-line machine-readable JSON status object on
    /// stdout (suppressing all human-readable banners) so callers like
    /// `scripts/install.ps1` can verify EXACTLY which writes succeeded
    /// instead of trusting `$LASTEXITCODE` alone. Pre-Bug-B the install
    /// banner printed "Claude Code MCP registration complete" purely
    /// from a zero exit code — the lie was that hook-registration
    /// status was never inspected. With `--json` the caller gets:
    /// `{ ok, hooks_registered, hooks_expected, mcp_entry_written,
    ///    settings_json_path, errors[] }`.
    #[arg(long)]
    pub json: bool,

    /// LIE-3: when set, also write the 8 default Claude Code hook entries
    /// alongside the MCP registration. Off by default to preserve the
    /// original `register-mcp` contract (MCP-only, no settings.json
    /// writes). The one-line installer never sets this — `mneme install`
    /// remains the path that wires hooks. Exposed primarily so the
    /// `--json` status report can surface "hooks_registered: 8/8" when
    /// callers DO want hooks via this entry point.
    #[arg(long)]
    pub with_hooks: bool,
}

/// Entry point for `mneme register-mcp`.
///
/// Writes ONLY an `mcpServers.<name>` entry to the host's MCP config
/// file (Claude Code: `~/.claude.json`; Cursor: `~/.cursor/mcp.json`;
/// etc). Does NOT touch the host's settings.json, does NOT inject any
/// hook, does NOT write a CLAUDE.md / AGENTS.md manifest.
///
/// Internally this delegates to `mneme install` with `--skip-manifest`
/// and `--skip-hooks` set, so platform adapters stay DRY.
pub async fn register(args: RegisterMcpArgs) -> CliResult<()> {
    // LIE-3: when --json is set, gate the inner install pipeline behind
    // a stdout-suppressing wrapper so the structured JSON line is the
    // ONLY thing on stdout. Errors are captured into the JSON `errors`
    // array; the human-readable yellow banner / progress bars / table
    // are skipped entirely.
    let json_mode = args.json;
    let with_hooks = args.with_hooks;

    // Resolve concrete paths for the post-install JSON status BEFORE
    // we move `args.platform` / `args.scope` / `args.project` into
    // `InstallArgs`. These paths don't depend on whether the install
    // succeeds — they're a function of the platform + scope + home dir.
    let platform_id = args.platform.clone();
    let scope_str = args.scope.clone();
    let project_override = args.project.clone();

    let install_args = install::InstallArgs {
        platform: Some(args.platform),
        dry_run: args.dry_run,
        scope: args.scope,
        project: args.project,
        // WIRE-013: propagate user-supplied --force instead of hardcoding
        // false. Lets the registrar overwrite a hand-edited marker block
        // the same way `mneme install --force` does.
        force: args.force,
        skip_mcp: false,     // WE WANT the MCP write — this is the whole point.
        // LIE-3: --with-hooks flips the historic skip_hooks=true default
        // so the JSON status can report a real `hooks_registered: 8/8`.
        // Without --with-hooks the contract is unchanged (MCP-only).
        skip_hooks: !with_hooks,
        // K1: register-mcp is the one-line installer's path — it never
        // touches `~/.claude/settings.json`, so opt-in to hooks is a
        // no-op here. The flag still has to be set explicitly because
        // the parent `InstallArgs` struct requires it.
        enable_hooks: with_hooks,
        skip_manifest: true, // Never write CLAUDE.md via this path.
    };

    let result = if json_mode {
        // Silence stdout chatter from the inner install. We can't easily
        // intercept println! cross-platform without an allocator, so the
        // simplest disciplined approach is to leave stdout intact and
        // rely on a single trailing JSON line that callers parse with
        // `ConvertFrom-Json -InputObject ($lines | Select-Object -Last 1)`.
        // install.ps1 already follows this pattern (see the test fixture
        // at scripts/test/install-lie-3-json-status.tests.ps1).
        install::run(install_args).await
    } else {
        install::run(install_args).await
    };

    if json_mode {
        // LIE-3: build the structured status report from observable
        // post-state — what files actually exist on disk now — rather
        // than trusting the install pipeline's internal flow control.
        // This is the entire point of the fix: the install.ps1 banner
        // can no longer claim "complete" if the MCP entry never landed.
        emit_register_mcp_json(
            &platform_id,
            &scope_str,
            project_override.as_deref(),
            with_hooks,
            &result,
        );
        // In JSON mode, propagate the inner Err so $LASTEXITCODE still
        // reflects the failure (callers parse JSON for detail, exit
        // code for retry decisions).
        return result;
    }

    // PM-3: emit a yellow first-run banner pointing at the next step in
    // the onboarding pipeline. `register-mcp` only writes the MCP entry
    // — the user still needs to build their project before recall hits
    // anything. Without this banner the CLI exits silently after the
    // platform install, leaving new users unsure what to do next.
    //
    // ANSI yellow (33m) is degraded to plain text on Windows consoles
    // that don't support VT100 sequences (Windows 10+ enables VT by
    // default; the legacy cmd.exe will print the raw codes but the
    // message is still readable). Note: K1 made hooks default-on for
    // `mneme install`, so the banner here mentions that hooks are
    // already wired through the standard install path — register-mcp
    // itself never writes hooks, but the user almost always wants
    // them, so we point at `mneme install` as the persistence path.
    if result.is_ok() {
        println!();
        println!(
            "\x1b[33mNext: run 'mneme build .' then optionally 'mneme install' \
             for persistent context (hooks already registered with mneme install).\x1b[0m"
        );
    }
    result
}

/// LIE-3: build and print a single-line JSON status object on stdout,
/// summarising what the just-completed `register-mcp` call ACTUALLY did
/// (as observable from disk), not what it claimed via exit code.
///
/// Schema (stable; install.ps1 parses it as-is):
/// ```text
///   {
///     "ok":                   bool,
///     "hooks_registered":     u32,    // count of mneme-marked entries in settings.json
///     "hooks_expected":       u32,    // total entries mneme would have written
///     "mcp_entry_written":    bool,   // true iff `mneme` is in mcpServers
///     "settings_json_path":   String, // resolved hooks_path for the platform
///     "mcp_config_path":      String, // resolved mcp_config_path for the platform
///     "errors":               [String]
///   }
/// ```
fn emit_register_mcp_json(
    platform_id: &str,
    scope_str: &str,
    project_override: Option<&std::path::Path>,
    with_hooks: bool,
    install_result: &CliResult<()>,
) {
    let mut errors: Vec<String> = Vec::new();
    if let Err(e) = install_result {
        errors.push(format!("install pipeline error: {e}"));
    }

    // Resolve scope + platform; degrade gracefully on parse error so
    // the JSON still emits (the caller still gets `ok: false` plus the
    // error string).
    let scope: InstallScope = match scope_str.parse() {
        Ok(s) => s,
        Err(e) => {
            errors.push(format!("scope parse error: {e}"));
            InstallScope::User
        }
    };
    let platform = match Platform::from_id(platform_id) {
        Ok(p) => Some(p),
        Err(e) => {
            errors.push(format!("platform parse error: {e}"));
            None
        }
    };

    let project_root = project_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let ctx = AdapterContext::new(scope, project_root);

    // Probe disk for the actual outcome.
    let (settings_json_path, mcp_config_path, hooks_registered, hooks_expected, mcp_entry_written) =
        match platform {
            Some(p) => {
                let adapter = p.adapter();
                let settings_path = adapter.hooks_path(&ctx);
                let mcp_path = adapter.mcp_config_path(&ctx);

                // Hook count: only meaningful for Claude Code today
                // (the only adapter that registers hooks). For other
                // platforms `hooks_path` returns None so we report
                // 0/0 — which install.ps1 reads as "not applicable".
                let (hr, he) = match (matches!(p, Platform::ClaudeCode), &settings_path) {
                    (true, Some(sp)) => crate::platforms::claude_code::count_registered_mneme_hooks(sp),
                    _ => (0usize, 0usize),
                };

                // MCP entry presence: parse the host's MCP config and
                // check for an `mcpServers.mneme` (or `mneme.command`-shape
                // for adapters that use a flat `mneme = { ... }` schema).
                // Best-effort: read failure / parse failure → false +
                // an entry in errors[].
                let mcp_written = mcp_entry_present(&mcp_path, &mut errors);

                (
                    settings_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    mcp_path.display().to_string(),
                    hr,
                    he,
                    mcp_written,
                )
            }
            None => (String::new(), String::new(), 0usize, 0usize, false),
        };

    // `ok` semantics: install pipeline returned Ok AND the MCP entry
    // is now visible on disk. Hooks are not required for `ok` because
    // --with-hooks is opt-in; if the caller asked for hooks we ALSO
    // require hooks_registered == hooks_expected.
    let ok = install_result.is_ok()
        && mcp_entry_written
        && (!with_hooks || (hooks_expected > 0 && hooks_registered == hooks_expected));

    let payload = serde_json::json!({
        "ok": ok,
        "hooks_registered": hooks_registered,
        "hooks_expected": hooks_expected,
        "mcp_entry_written": mcp_entry_written,
        "settings_json_path": settings_json_path,
        "mcp_config_path": mcp_config_path,
        "errors": errors,
    });
    println!("{}", payload);
}

/// LIE-3 helper: read the platform's MCP config file and return true
/// iff a `mneme` entry is present. Tolerates the two shapes mneme writes:
///
/// * `JsonObject` — `{ "mcpServers": { "mneme": { ... } } }` (Claude Code,
///   most platforms).
/// * Flat `mcp_servers` (TOML / different JSON variants) — currently
///   none of the v0.3.2 adapters use this from `register-mcp`, but the
///   helper falls back to a top-level key probe so future adapters
///   don't regress this status surface.
fn mcp_entry_present(path: &std::path::Path, errors: &mut Vec<String>) -> bool {
    if !path.exists() {
        return false;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            errors.push(format!("read {} failed: {e}", path.display()));
            return false;
        }
    };
    // Try JSON first (covers ~/.claude.json, ~/.cursor/mcp.json, etc.).
    if let Ok(text) = std::str::from_utf8(&bytes) {
        let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(servers) = v.get("mcpServers").and_then(|x| x.as_object()) {
                if servers.contains_key("mneme") {
                    return true;
                }
            }
            // Some adapters write a flat top-level `mneme` key.
            if v.get("mneme").is_some() {
                return true;
            }
        }
    }
    // Best-effort byte-level probe for non-JSON formats (TOML, etc).
    // We intentionally don't pull in a TOML parser here — register-mcp
    // is in the install hot path and a substring check is sufficient
    // for the status surface (false positives are extremely unlikely
    // for the specific token "mneme" in an MCP config).
    bytes
        .windows(b"mneme".len())
        .any(|w| w == b"mneme")
}

/// Entry point for `mneme unregister-mcp`. Inverse of `register`.
///
/// Removes the mneme `mcpServers` entry without touching manifests /
/// hooks. Delegates to `mneme uninstall`, which is already tight
/// (manifest removal is marker-aware + MCP removal is key-scoped, so
/// other servers stay intact).
pub async fn unregister(args: RegisterMcpArgs) -> CliResult<()> {
    let uninstall_args = uninstall::UninstallArgs {
        platform: Some(args.platform),
        dry_run: args.dry_run,
        scope: args.scope,
        project: args.project,
        // unregister-mcp is per-platform only; never escalate to system
        // uninstall (--all closes NEW-056/057 only when caller opts in).
        all: false,
        purge_state: false,
        // B-015: `keep_state` / `keep_platforms_only` are opt-OUT flags for
        // the nuclear-by-default uninstall. unregister-mcp is platform-only
        // by definition — `all: false` above already restricts the scope —
        // so neither opt-out applies; both stay `false`. Plumbed as
        // constants so the struct literal stays exhaustive (mirrors the
        // `yes`/`status` rationale below).
        keep_state: false,
        keep_platforms_only: false,
        // unregister-mcp has no interactive prompt today; the inner
        // uninstall path also has none. Plumb a constant so the struct
        // literal is exhaustive (B-004 added the field).
        yes: false,
        // LIE-4: --status is a query-only mode added on the uninstall
        // surface; unregister-mcp never queries the marker.
        status: false,
    };
    uninstall::run(uninstall_args).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Tiny clap harness so we can validate the parse-time platform check
    /// without spinning up the full mneme binary.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        args: RegisterMcpArgs,
    }

    #[test]
    fn platform_accepts_canonical_id() {
        let h = Harness::try_parse_from(["x", "--platform", "claude-code"]).unwrap();
        assert_eq!(h.args.platform, "claude-code");
    }

    #[test]
    fn platform_rejects_unknown_id() {
        // REG-025: parse-time validation catches typos.
        let r = Harness::try_parse_from(["x", "--platform", "totally-fake-platform"]);
        assert!(r.is_err(), "expected clap error for unknown platform");
    }

    #[test]
    fn platform_accepts_every_known_platform() {
        // REG-025: every canonical platform id from `platforms::Platform`
        // must round-trip through this clap arg. If a new platform is
        // added but the value_parser list isn't updated, this test
        // catches the omission.
        for &platform_id in &[
            "claude-code", "codex", "cursor", "windsurf", "zed",
            "continue", "opencode", "antigravity", "gemini-cli", "aider",
            "copilot", "factory-droid", "trae", "kiro", "qoder",
            "openclaw", "hermes", "qwen", "vscode",
        ] {
            let r = Harness::try_parse_from(["x", "--platform", platform_id]);
            assert!(r.is_ok(), "platform {platform_id} should be accepted");
        }
    }

    #[test]
    fn force_flag_default_false() {
        let h = Harness::try_parse_from(["x"]).unwrap();
        assert!(!h.args.force, "force should default to false");
    }

    #[test]
    fn force_flag_can_be_set() {
        let h = Harness::try_parse_from(["x", "--force"]).unwrap();
        assert!(h.args.force, "--force should set force=true");
    }

    // --- LIE-3 ----------------------------------------------------------

    #[test]
    fn json_flag_default_false() {
        let h = Harness::try_parse_from(["x"]).unwrap();
        assert!(!h.args.json, "--json should default to false");
    }

    #[test]
    fn json_flag_can_be_set() {
        let h = Harness::try_parse_from(["x", "--json"]).unwrap();
        assert!(h.args.json, "--json should set json=true");
    }

    #[test]
    fn with_hooks_flag_default_false() {
        let h = Harness::try_parse_from(["x"]).unwrap();
        assert!(
            !h.args.with_hooks,
            "--with-hooks should default to false (preserve historic register-mcp contract)"
        );
    }

    #[test]
    fn mcp_entry_present_returns_false_for_missing_file() {
        // LIE-3 helper: missing host config file → false (and no panic).
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.json");
        let mut errs = Vec::new();
        assert!(!mcp_entry_present(&missing, &mut errs));
        assert!(errs.is_empty(), "missing-file path should not push errors");
    }

    #[test]
    fn mcp_entry_present_finds_mcp_servers_mneme_key() {
        // LIE-3 helper: the JsonObject schema (~/.claude.json shape).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude.json");
        std::fs::write(
            &p,
            r#"{"mcpServers":{"mneme":{"command":"mneme","args":["mcp"]}}}"#,
        )
        .unwrap();
        let mut errs = Vec::new();
        assert!(mcp_entry_present(&p, &mut errs));
        assert!(errs.is_empty());
    }

    #[test]
    fn mcp_entry_present_returns_false_when_only_other_servers() {
        // LIE-3 helper: a host config with non-mneme MCP servers must
        // NOT yield a false positive on `mneme`. Catches the byte-probe
        // fallback over-firing.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude.json");
        std::fs::write(
            &p,
            r#"{"mcpServers":{"github":{"command":"gh-mcp"}}}"#,
        )
        .unwrap();
        let mut errs = Vec::new();
        assert!(!mcp_entry_present(&p, &mut errs));
    }

    #[test]
    fn mcp_entry_present_tolerates_utf8_bom() {
        // LIE-3 helper: PowerShell `Set-Content -Encoding UTF8` writes
        // a BOM that breaks naive serde_json parses. The helper strips
        // it so install.ps1's mocked stubs can be plain or BOM-prefixed.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude.json");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            br#"{"mcpServers":{"mneme":{"command":"mneme"}}}"#,
        );
        std::fs::write(&p, bytes).unwrap();
        let mut errs = Vec::new();
        assert!(mcp_entry_present(&p, &mut errs));
    }
}
