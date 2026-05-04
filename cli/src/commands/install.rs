//! `mneme install` — provision mneme into one or more AI platforms.
//!
//! Behaviour
//! =========
//!
//! - `mneme install` (no args)            → auto-detect every installed
//!                                              platform and configure each
//! - `mneme install --platform=cursor`    → configure exactly one
//! - `mneme install --dry-run`            → print what would change, do
//!                                              not write anything
//! - `mneme install --scope=project`      → write into the active project
//!                                              (default: user)
//! - `mneme install --force`              → overwrite even if the user
//!                                              edited mneme's marker block
//!
//! Per design §21.4.1 / §25.5: every write is marker-wrapped (idempotent),
//! every config write makes a `.bak` first, and the operation is safe to
//! re-run. See [`crate::markers`].

use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use crate::commands::doctor::{install_hint_for, probe_all_toolchain, ToolProbe, ToolSeverity};
use crate::error::CliResult;
use crate::platforms::{prune_baks, AdapterContext, InstallScope, Platform, PlatformDetector};
use crate::receipts::{sha256_of_file, Receipt, ReceiptAction};

/// Idempotent-1: how many `<path>.mneme-YYYYMMDD-HHMMSS.bak` snapshots
/// to retain for any one source path. The most recent (the one referenced
/// by the just-written receipt) is always preserved; older snapshots
/// beyond this count are deleted post-write. Override via
/// `mneme cache prune --baks --keep N` for explicit cleanup runs.
pub(crate) const DEFAULT_BAK_RETAIN: usize = 5;

/// CLI args for `mneme install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Restrict installation to a single platform (id from the matrix in
    /// design §21.4 — e.g. `claude-code`, `cursor`, `codex`). When omitted,
    /// every detected platform is configured.
    #[arg(long)]
    pub platform: Option<String>,

    /// Print what would change but write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Where to install — defaults to `user`.
    #[arg(long, default_value = "user")]
    pub scope: String,

    /// Project root override. Defaults to CWD.
    #[arg(long)]
    pub project: Option<PathBuf>,

    /// Overwrite mneme's marker block even if the user has hand-edited it.
    #[arg(long)]
    pub force: bool,

    /// Skip the MCP-config writes (manifest + hooks only). Useful when the
    /// user is registering MCP through their platform UI.
    #[arg(long)]
    pub skip_mcp: bool,

    /// Skip hook registration. K1 fix (v0.3.2): hooks are now OPT-OUT.
    /// By default `mneme install` writes 8 hook entries under
    /// `~/.claude/settings.json::hooks` (SessionStart, UserPromptSubmit,
    /// PreToolUse, PostToolUse, Stop, PreCompact, SubagentStop,
    /// SessionEnd). Each carries the `_mneme: { managed: true }` marker
    /// so `mneme uninstall` can strip them without touching unrelated
    /// hooks. Without these hooks the persistent-memory pipeline
    /// (history.db, tasks.db, tool_cache.db, livestate.db) stays empty
    /// and mneme degrades to a query-only MCP surface — exactly the
    /// "not talking to Claude" symptom Phase A flagged as the keystone
    /// bug. Pass this flag (or `--no-hooks`) only if you have a reason
    /// to bypass that pipeline.
    ///
    /// The v0.3.0 install incident that originally motivated opt-in
    /// behaviour is architecturally impossible now: every hook binary
    /// reads STDIN JSON via `crate::hook_payload::read_stdin_payload`
    /// and exits 0 on any internal error, so a mneme bug can never
    /// block the user's tool calls.
    #[arg(long, alias = "no-hooks")]
    pub skip_hooks: bool,

    /// DEPRECATED — kept for backward compat with existing scripts.
    /// As of v0.3.2 (K1 fix) hooks are registered by default. This flag
    /// is now a no-op: present for compat, has no effect. Use
    /// `--no-hooks` / `--skip-hooks` to opt out.
    #[arg(long)]
    pub enable_hooks: bool,

    /// Skip the CLAUDE.md / AGENTS.md manifest write (MCP + hooks only).
    /// The one-line installer (`scripts/install.ps1`) sets this so a
    /// clean install touches only the platform's MCP registry and
    /// nothing else. Power users who want the manifest block can run
    /// `mneme install --platform=claude-code` without this flag later.
    #[arg(long)]
    pub skip_manifest: bool,
}

/// Entry point used by `main.rs`.
pub async fn run(args: InstallArgs) -> CliResult<()> {
    let scope: InstallScope = args.scope.parse()?;
    let project_root = args
        .project
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // K1 fix: hooks default ON. ctx.enable_hooks is true UNLESS the user
    // explicitly opts out via `--no-hooks` / `--skip-hooks`. The legacy
    // `--enable-hooks` flag is kept for compat (no-op, hooks already on).
    let ctx = AdapterContext::new(scope, project_root.clone())
        .with_dry_run(args.dry_run)
        .with_force(args.force)
        .with_enable_hooks(!args.skip_hooks);

    let targets: Vec<Platform> = match args.platform.as_deref() {
        Some(id) => vec![Platform::from_id(id)?],
        None => {
            let detected = PlatformDetector::detect_installed(scope, &project_root);
            info!(count = detected.len(), "auto-detected platforms");
            detected
        }
    };

    if targets.is_empty() {
        warn!("no platforms detected; nothing to install");
        return Ok(());
    }

    // NEW-002: clear warning when this is the rust-only path. Anyone
    // who runs `mneme install` directly (not through scripts/install.ps1
    // or scripts/install.sh) gets the platform manifest + MCP wiring
    // but NOT the binary placement, PATH update, Defender exclusion,
    // or daemon spawn that the shell installer performs. State the
    // gap explicitly so users don't end up in silent-partial-state
    // (the v0.3.0 install regression footprint).
    if std::env::var_os("MNEME_INSTALLED_BY_SCRIPT").is_none() {
        warn!(
            "running cli-only install — script-level steps are skipped: \
             binary placement, PATH update, Windows Defender exclusion, daemon spawn. \
             For a complete install run scripts/install.ps1 (Windows) or scripts/install.sh (POSIX)."
        );
    }

    // Gentle guardrail — warn if Claude Code appears to be running and
    // we're about to modify its files. In v0.3.1 MCP-only installs are
    // safe while CC is running (CC re-reads mcpServers on next launch),
    // but manifest writes can create stale cached state. Not a block;
    // just a heads-up.
    let claude_is_target = targets.iter().any(|p| matches!(p, Platform::ClaudeCode));
    let writing_anything_but_mcp = !args.skip_manifest || !args.skip_hooks;
    if claude_is_target && writing_anything_but_mcp && !args.dry_run && claude_code_likely_running()
    {
        warn!(
            "Claude Code appears to be running — close it before \
             re-launching so it picks up mneme cleanly. Continuing."
        );
    }

    let bar = make_bar(targets.len() as u64);
    let mut report: Vec<InstallReport> = Vec::with_capacity(targets.len());
    // Tracks how many hook entries got written across all platforms so
    // the closing banner can show a single concrete number.
    let mut hooks_written_total: usize = 0;
    let mut any_platform_supports_hooks = false;

    // Receipt — records every file write, MCP registration, etc.
    // Persisted at `~/.mneme/install-receipts/<stamp>-<id>.json` at the
    // end of the install so `mneme rollback` can reverse it atomically.
    // Only written on non-dry-run.
    let mut receipt = Receipt::new();

    // I-1 / NEW-002: record the absolute mneme exe path that will land
    // in every MCP `command` field. Lets `mneme rollback` confirm no
    // host config still has a stale absolute path (or the bare "mneme"
    // shape v0.3.0 used to write).
    if !args.dry_run {
        receipt.push(ReceiptAction::ResolvedExePath {
            exe_path: ctx.exe_path.clone(),
        });
    }

    for platform in targets {
        bar.set_message(platform.display_name().to_string());
        // Track whether this platform exposes a hooks_path BEFORE
        // calling install_one so a writes-zero result (because
        // --enable-hooks wasn't passed) can still be counted toward
        // the "we have a hook surface" banner branch.
        if platform.adapter().hooks_path(&ctx).is_some() {
            any_platform_supports_hooks = true;
        }
        let r = install_one(
            platform,
            &ctx,
            args.skip_mcp,
            args.skip_hooks,
            args.skip_manifest,
            &mut receipt,
        );
        match &r {
            Ok(per_platform) => {
                hooks_written_total += per_platform.hook_entries_written;
            }
            Err(e) => {
                warn!(platform = platform.id(), error = %e, "install failed for platform");
            }
        }
        report.push(InstallReport {
            platform,
            outcome: match &r {
                Ok(_) => "ok".into(),
                Err(e) => format!("error: {e}"),
            },
        });
        bar.inc(1);
    }

    bar.finish_with_message("done");

    // Persist the receipt so `mneme rollback` can reverse this install.
    // Skipped in dry-run — no writes happened so no rollback is possible.
    if !args.dry_run && !receipt.actions.is_empty() {
        match receipt.save() {
            Ok(path) => info!(path = %path.display(), "install receipt written"),
            Err(e) => {
                warn!(error = %e, "failed to write install receipt (install succeeded but rollback will be manual)")
            }
        }
    }

    // K19 / Bug H fix: drop a self-contained `~/.mneme/uninstall.ps1` so
    // the user has a recovery path even if the binary becomes broken /
    // version-skewed / partially installed. The script doesn't depend on
    // mneme.exe — it does taskkill/PATH/Defender/hook-strip/dir-remove
    // via pure PowerShell.
    //
    // Bug H (postmortem §6, 2026-04-29 AWS install test): this used to be
    // `if let Err(e) = ... { warn!(...) }` — a silent fail mode that
    // produced the symptom "file does not exist after install" with
    // zero visible error to the user. Now propagated via `?` and the
    // function itself round-trip-verifies the bytes it wrote, so a
    // partial / truncated / tampered write fails the install loudly
    // instead of slipping through.
    if !args.dry_run {
        drop_standalone_uninstaller().map_err(crate::error::CliError::DropUninstaller)?;
    }

    println!();
    println!("{:<14}  {:<8}  result", "platform", "scope");
    for entry in &report {
        println!(
            "{:<14}  {:<8}  {}",
            entry.platform.id(),
            scope_label(scope),
            entry.outcome
        );
    }
    if args.dry_run {
        println!("\n(dry-run: no files were written)");
    }

    // K1: announce hook-registration status. The #1 critical Phase A
    // finding was that mneme installs were silently skipping hook
    // registration, leaving users with a query-only surface (MCP tools)
    // instead of the persistent-memory layer they thought they'd installed.
    // Always tell the user which path they got and how to flip it.
    if !args.dry_run && any_platform_supports_hooks {
        println!();
        if hooks_written_total > 0 {
            println!(
                "✓ Hooks registered ({} entries in ~/.claude/settings.json)",
                hooks_written_total
            );
            println!(
                "  Persistent-memory features active: cache hits, step ledger, drift injection,"
            );
            println!("  conversation capture, decision recall across compactions.");
            println!("  Disable via:  mneme uninstall --platform=claude-code");
        } else if args.skip_hooks {
            println!("⚠ Hooks NOT registered — --skip-hooks / --no-hooks was passed.");
            println!("  Persistent-memory features inactive: no conversation capture, no decision");
            println!(
                "  recall, no compaction-resilient step ledger. MCP tools still work on demand."
            );
            println!("  Re-run without --skip-hooks to activate.");
        } else {
            // Should be impossible after K1 fix — ctx.enable_hooks is
            // !skip_hooks, write_hooks fires for every adapter that
            // exposes a hooks_path. If we somehow land here the platform
            // returned 0 entries (shape error?) — surface that explicitly
            // rather than silently shrugging.
            println!(
                "⚠ Hooks NOT registered despite default-on policy — likely a platform-adapter error."
            );
            println!(
                "  Run `mneme doctor --strict` to diagnose. File issue at omanishay-cyber/mneme."
            );
        }
    }

    // G12: capability summary. After every install (non-dry-run), tell
    // the user exactly which dev-toolchain pieces are present + which
    // are missing, with a one-line install hint per missing tool. Lets
    // them spot a degraded install before they hit a cryptic failure
    // later (e.g. "tauri build" failing because cargo isn't on PATH).
    //
    // Pulled from the canonical `KNOWN_TOOLCHAIN` list in `doctor.rs`
    // — both `mneme doctor --strict` and this summary read the same
    // set so the two surfaces never drift apart.
    if !args.dry_run {
        // A1-013: pass the user's --platform flag through so the summary
        // only renders rows that actually affect the chosen integration.
        // Cursor users no longer see red-X on Tauri-CLI / Tesseract /
        // Java that they don't need.
        print_capability_summary_for(args.platform.as_deref());
    }

    Ok(())
}

/// G12: print the post-install capability summary. Renders one row per
/// G1-G10 tool, marking missing HIGH-severity tools with a fix hint
/// and ending with an overall verdict. Always called *after* the
/// install report so the box-and-table flow reads top-to-bottom.
/// A1-013 (2026-05-04): platform-relevant tools only. The previous
/// implementation rendered all G1-G10 (now G1-G12) regardless of which
/// platform the user passed `--platform=` to. Cursor users saw red ✗
/// on Tauri-CLI / Tesseract / etc. that they don't actually need.
///
/// This function returns the set of issue_ids relevant to a given
/// platform; an unspecified platform (None) still shows everything
/// (default install scenario where user wants all integrations).
fn relevant_tools_for_platform(platform: Option<&str>) -> Option<&'static [&'static str]> {
    match platform.map(|s| s.to_ascii_lowercase()) {
        // Cursor / VS Code / Zed / Codex / etc. need only baseline
        // toolchain (Rust + Node + Git). Tauri / Tesseract / Java are
        // for the standalone build pipeline and multimodal-bridge.
        Some(p) if matches!(p.as_str(), "cursor" | "vscode" | "vs-code" | "zed" | "codex" | "windsurf" | "qoder" | "qwen" | "gemini") => {
            Some(&["G1", "G3", "G5"]) // Rust, Node, Git only
        }
        // Claude Code, the canonical default, exercises every integration.
        Some(p) if p == "claude-code" || p == "claude_code" => None,
        // Unknown platform → show everything to be safe.
        _ => None,
    }
}

fn print_capability_summary_for(platform: Option<&str>) {
    let all_probes = probe_all_toolchain();
    let scope = relevant_tools_for_platform(platform);
    let probes: Vec<_> = match scope {
        Some(ids) => all_probes
            .into_iter()
            .filter(|p| ids.contains(&p.entry.issue_id))
            .collect(),
        None => all_probes,
    };

    println!();
    println!("--------------------------------------------------------");
    if let Some(p) = platform {
        println!("  developer-toolchain capability check (--platform={})", p);
    } else {
        println!("  developer-toolchain capability check (G1-G12)");
    }
    println!("--------------------------------------------------------");
    for probe in &probes {
        render_capability_row(probe);
    }

    let any_high_missing = probes
        .iter()
        .any(|p| !p.is_present() && p.entry.severity == ToolSeverity::High);
    let any_missing = probes.iter().any(|p| !p.is_present());
    // UX-1: total missing-tool count surfaces in the closing line so
    // operators see "3 missing" at a glance without grepping the rows.
    let missing_count = probes.iter().filter(|p| !p.is_present()).count();

    println!();
    if any_high_missing {
        println!(
            "  mneme installed with REDUCED capability — {} dev tool(s) missing",
            missing_count
        );
        println!("  (one or more HIGH-severity). Run `mneme doctor --strict` for");
        println!("  capability gap analysis + per-tool fix instructions.");
    } else if any_missing {
        println!(
            "  mneme installed. {} MEDIUM/LOW-severity dev tool(s) missing —",
            missing_count
        );
        println!("  core capability intact. Run `mneme doctor --strict` for");
        println!("  capability gap analysis + per-tool fix instructions.");
    } else {
        println!("  mneme installed at FULL capability -- every relevant dev");
        println!("  tool detected. Run `mneme doctor --strict` to re-verify.");
    }
}

/// A1-013 (2026-05-04): back-compat wrapper. Existing callers that
/// don't know about platform filtering get the legacy behavior (show
/// every G1-G12 row). Currently no caller; reserved for tests + any
/// future code that wants the unfiltered view.
#[allow(dead_code)]
fn print_capability_summary() {
    print_capability_summary_for(None)
}

/// Render one row of the post-install capability summary. Marks present
/// tools with a green check + version, missing tools with a red cross +
/// install hint matching the host OS.
fn render_capability_row(probe: &ToolProbe) {
    if probe.is_present() {
        let v = probe
            .version
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        println!("  ✓ {} detected{}", probe.entry.display, v);
    } else {
        // Severity in the line so the user sees at a glance whether
        // this is a blocker (HIGH) or just nice-to-have.
        println!(
            "  ✗ {} NOT detected — [{}] {}",
            probe.entry.display,
            probe.entry.severity.label().trim(),
            probe.entry.purpose,
        );
        println!("      Install: {}", install_hint_for(&probe.entry));
    }
}

/// What happened while installing one platform. Bubbled up so the
/// outer summary banner can announce hook-registration status
/// without re-reading every settings.json after the fact.
#[derive(Debug, Default)]
struct PlatformInstallOutcome {
    /// Number of hook entries (event handlers) mneme wrote into the
    /// platform's settings file during this run. Zero means either the
    /// platform doesn't expose a hooks file OR `--enable-hooks` wasn't
    /// passed OR `--skip-hooks` was passed.
    hook_entries_written: usize,
}

/// Run one platform's adapter. Order matters: manifest first (so the user
/// sees mneme even if MCP/hook write fails), then MCP, then hooks.
///
/// Each write records a [`ReceiptAction`] into `receipt` so
/// `mneme rollback` can reverse this install later.
fn install_one(
    platform: Platform,
    ctx: &AdapterContext,
    skip_mcp: bool,
    skip_hooks: bool,
    skip_manifest: bool,
    receipt: &mut Receipt,
) -> CliResult<PlatformInstallOutcome> {
    let mut outcome = PlatformInstallOutcome::default();
    let adapter = platform.adapter();

    if !skip_manifest {
        let target = adapter.manifest_path(ctx);
        let existed_before = target.exists();
        let sha_before = if existed_before {
            sha256_of_file(&target)
        } else {
            String::new()
        };

        let manifest = adapter.write_manifest(ctx)?;
        info!(platform = platform.id(), path = %manifest.display(), "manifest written");

        if !ctx.dry_run {
            let sha_after = sha256_of_file(&manifest);
            if existed_before {
                if let Some(backup) = find_latest_mneme_bak(&manifest) {
                    receipt.push(ReceiptAction::FileModified {
                        path: manifest.clone(),
                        backup_path: backup,
                        sha256_before: sha_before,
                        sha256_after: sha_after,
                    });
                }
                // Idempotent-1: trim accumulated snapshots for this path,
                // keeping the most recent N (which includes the one we
                // just wrote and the receipt references).
                if let Err(e) = prune_baks(&manifest, DEFAULT_BAK_RETAIN) {
                    warn!(path = %manifest.display(), error = %e, "prune_baks: manifest snapshot prune failed");
                }
            } else {
                receipt.push(ReceiptAction::FileCreated {
                    path: manifest.clone(),
                    sha256_after: sha_after,
                });
            }
        }
    } else {
        info!(
            platform = platform.id(),
            "manifest skipped (--skip-manifest)"
        );
    }

    if !skip_mcp {
        let target = adapter.mcp_config_path(ctx);
        let existed_before = target.exists();
        let sha_before = if existed_before {
            sha256_of_file(&target)
        } else {
            String::new()
        };

        let mcp = adapter.write_mcp_config(ctx)?;
        info!(platform = platform.id(), path = %mcp.display(), "mcp config written");

        if !ctx.dry_run {
            let sha_after = sha256_of_file(&mcp);
            if existed_before {
                if let Some(backup) = find_latest_mneme_bak(&mcp) {
                    receipt.push(ReceiptAction::FileModified {
                        path: mcp.clone(),
                        backup_path: backup,
                        sha256_before: sha_before,
                        sha256_after: sha_after,
                    });
                }
                // Idempotent-1: trim accumulated snapshots for this path.
                if let Err(e) = prune_baks(&mcp, DEFAULT_BAK_RETAIN) {
                    warn!(path = %mcp.display(), error = %e, "prune_baks: mcp snapshot prune failed");
                }
            } else {
                receipt.push(ReceiptAction::FileCreated {
                    path: mcp.clone(),
                    sha256_after: sha_after,
                });
            }
            // Also record the MCP registration semantically — lets
            // `mneme rollback` strip the mneme entry specifically
            // without touching neighbors in mcp_config_path.
            receipt.push(ReceiptAction::McpRegistered {
                platform: platform.id().to_string(),
                host_file: mcp.clone(),
            });
        }
    }

    if !skip_hooks {
        // K1: capture pre-write existed_before + sha so the receipt
        // records FileModified-vs-FileCreated correctly. Rollback for
        // FileCreated = delete; for FileModified = restore from .bak.
        let target_path = adapter.hooks_path(ctx);
        let existed_before = target_path.as_ref().map(|p| p.exists()).unwrap_or(false);
        let sha_before = match (&target_path, existed_before) {
            (Some(p), true) => sha256_of_file(p),
            _ => String::new(),
        };

        if let Some(hooks) = adapter.write_hooks(ctx)? {
            info!(platform = platform.id(), path = %hooks.display(), "hooks written");
            // K1: count entries actually registered so the closing
            // install banner can print a concrete number. Today only
            // the ClaudeCode adapter writes hooks; the count is the
            // length of HOOK_SPECS.
            outcome.hook_entries_written = crate::platforms::claude_code::HOOK_SPECS.len();

            if !ctx.dry_run {
                let sha_after = sha256_of_file(&hooks);
                if existed_before {
                    if let Some(backup) = find_latest_mneme_bak(&hooks) {
                        receipt.push(ReceiptAction::FileModified {
                            path: hooks.clone(),
                            backup_path: backup,
                            sha256_before: sha_before,
                            sha256_after: sha_after,
                        });
                    }
                    // Idempotent-1: trim accumulated snapshots for this path.
                    if let Err(e) = prune_baks(&hooks, DEFAULT_BAK_RETAIN) {
                        warn!(path = %hooks.display(), error = %e, "prune_baks: hooks snapshot prune failed");
                    }
                } else {
                    receipt.push(ReceiptAction::FileCreated {
                        path: hooks.clone(),
                        sha256_after: sha_after,
                    });
                }
            }
        }
    }
    Ok(outcome)
}

/// Find the newest `<target>.mneme-YYYYMMDD-HHMMSS.bak` alongside `target`
/// — that's the timestamped backup `backup_then_write` just created.
/// Returns None if no such file exists (e.g. target is a fresh create).
fn find_latest_mneme_bak(target: &std::path::Path) -> Option<PathBuf> {
    let parent = target.parent()?;
    let stem = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|name| {
                    // Match <stem>.mneme-<timestamp>.bak where stem = target's filename
                    // or the pre-extension stem (we accept both shapes).
                    name.starts_with(&format!("{stem}.mneme-"))
                        || name.starts_with(&format!(
                            "{}.mneme-",
                            target.file_stem().and_then(|s| s.to_str()).unwrap_or("")
                        ))
                })
                .unwrap_or(false)
        })
        .collect();
    candidates.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    candidates.into_iter().next()
}

/// K19 / Bug H fix: drop a self-contained PowerShell uninstaller at
/// `~/.mneme/uninstall.ps1` so the user always has a recovery path
/// regardless of whether the mneme binary works. The script is bundled
/// into the CLI at build time via `include_str!`.
///
/// Idempotent — overwrites any prior copy.
///
/// **Bug H (postmortem §6, 2026-04-29 AWS install test):** Earlier this
/// function was wired but the call site at line 243 used
/// `if let Err(e) = ... { warn!(...) }`, swallowing every failure into
/// log noise the user never saw. The fix here has three parts:
///
/// 1. Return any IO error to the caller so the call site can either
///    propagate via `?` or decide explicitly to ignore it.
/// 2. After `fs::write`, read the file back and assert byte-equality
///    with the `include_str!` constant. A partial write (Defender
///    interception, AV mid-scan, antivirus-renamed-on-the-fly, the
///    classic Windows file-locking race that produced the §6 ghost
///    failure) now fails loudly with `InvalidData` instead of looking
///    like success.
/// 3. The `pub` visibility lets the integration test in
///    `cli/tests/install_writes_standalone_uninstaller.rs` exercise
///    this without standing up the full `install::run` pipeline.
///
/// On non-Windows hosts this is a no-op (the recovery path on POSIX is
/// `scripts/uninstall.sh`, out of scope for K19).
pub fn drop_standalone_uninstaller() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const UNINSTALL_PS1: &str = include_str!("../../../scripts/uninstall.ps1");
        // Bug H + HOME-bypass-install: resolve the mneme root through
        // `mneme_common::paths::PathManager::default_root()` so MNEME_HOME
        // is honored uniformly with the rest of the codebase. PathManager's
        // discovery order is MNEME_HOME → dirs::home_dir().join(".mneme").
        // This makes the function exercisable from integration tests under
        // a tempdir AND keeps the function's behavior consistent with every
        // other path-touching site in the CLI.
        let mneme_dir = common::paths::PathManager::default_root()
            .root()
            .to_path_buf();
        // Idempotent: succeeds whether or not the directory already exists.
        // Without this, a fresh-machine install where ~/.mneme/ has not
        // yet been created would fail at the fs::write below with
        // ErrorKind::NotFound — exactly the silent symptom postmortem
        // §6 captured.
        std::fs::create_dir_all(&mneme_dir)?;
        let target = mneme_dir.join("uninstall.ps1");
        std::fs::write(&target, UNINSTALL_PS1)?;

        // Bug H: post-write verification. Read the file back and confirm
        // the bytes match the include_str! constant. Catches partial
        // writes (truncated due to AV interception, low disk, race with
        // a parallel `mneme uninstall` reading the file mid-write) and
        // unauthorised post-write tampering. If the bytes drift we'd
        // rather fail the install than ship a broken recovery script.
        let written = std::fs::read(&target)?;
        if written.as_slice() != UNINSTALL_PS1.as_bytes() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "uninstaller content drift: wrote {} bytes, read back {} \
                     bytes that do not match the embedded script — refusing \
                     to silently ship a corrupt recovery path",
                    UNINSTALL_PS1.len(),
                    written.len(),
                ),
            ));
        }

        info!(
            path = %target.display(),
            bytes = written.len(),
            "standalone uninstaller dropped (post-write verified)"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        // A1-011 (2026-05-04): drop ~/.mneme/uninstall.sh on POSIX, mode
        // 0o755. Originally Windows-only (K19) because the audit
        // observed only the Windows failure mode; in practice Linux +
        // macOS users hit the same class of issue (corrupt update,
        // partial install, broken binary) and need a recovery script
        // they can run without a working `mneme` binary.
        //
        // The script content lives at scripts/uninstall.sh and is
        // bundled via include_str! so the same byte-equality + chmod
        // pattern as the Windows .ps1 case applies.
        const UNINSTALL_SH_BYTES: &[u8] = include_bytes!("../../../scripts/uninstall.sh");
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return Ok(()),
        };
        let mneme_dir = home.join(".mneme");
        if !mneme_dir.exists() {
            std::fs::create_dir_all(&mneme_dir)?;
        }
        let script_path = mneme_dir.join("uninstall.sh");
        std::fs::write(&script_path, UNINSTALL_SH_BYTES)?;
        // Post-write byte-equality check.
        let on_disk = std::fs::read(&script_path)?;
        if on_disk != UNINSTALL_SH_BYTES {
            return Err(std::io::Error::other(format!(
                "uninstall.sh write verify failed: on-disk {} bytes != expected {} bytes",
                on_disk.len(),
                UNINSTALL_SH_BYTES.len()
            )));
        }
        // chmod +x (mode 0o755).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &script_path,
            std::fs::Permissions::from_mode(0o755),
        )?;
        info!(
            path = %script_path.display(),
            bytes = UNINSTALL_SH_BYTES.len(),
            "POSIX standalone uninstaller dropped (post-write verified, mode 0755)"
        );
        Ok(())
    }
}

fn make_bar(n: u64) -> ProgressBar {
    let bar = ProgressBar::new(n);
    bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.cyan} [{bar:24.cyan/blue}] {pos}/{len} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-"),
    );
    bar.enable_steady_tick(Duration::from_millis(80));
    bar
}

fn scope_label(scope: InstallScope) -> &'static str {
    match scope {
        InstallScope::Project => "project",
        InstallScope::User => "user",
        InstallScope::Global => "global",
    }
}

/// One row of the per-platform install report. Kept private to the module
/// so we can change its shape freely.
#[derive(Debug)]
struct InstallReport {
    platform: Platform,
    outcome: String,
}

/// Lightweight probe: is Claude Code likely running on this host?
///
/// Windows: shells out to `tasklist /FI "IMAGENAME eq Claude.exe"` and
/// checks stdout for the executable name. Falsy on any error (the probe
/// is advisory, not authoritative).
///
/// Unix: shells out to `pgrep -f claude` and checks the exit code. Same
/// falsy-on-error semantics.
///
/// This is deliberately a *warning* not a *block* — authoritative
/// process checks require more plumbing (sysinfo crate, elevated lookups
/// on Windows) than the v0.3.1 scope allows. The architectural fix for
/// the v0.3.0 settings.json poisoning (see `platforms/claude_code.rs`)
/// means even running-Claude-Code installs are safe today; this probe
/// exists to prevent the cosmetic "stale config in memory" issue.
/// A1-012 (2026-05-04): consolidated to use `doctor::is_claude_code_running`.
///
/// The original implementation shelled out to `tasklist` (Windows) /
/// `pgrep` (POSIX) with image-name-only matching. doctor.rs does the
/// same job using sysinfo (already a workspace dep), tightened post
/// A1-009 to match argv[0]/exe-path only (no false positives from
/// editor-open files referencing claude-code). Both surfaces now read
/// the same answer; install.rs's warning and the doctor's "Claude is
/// RUNNING" overlay can never disagree.
fn claude_code_likely_running() -> bool {
    crate::commands::doctor::is_claude_code_running().is_some()
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helper surfaces above.
//
// WIRE-005 follow-up: `install.rs` previously had ZERO unit tests despite
// hosting four leaf-level helpers safe to exercise in isolation
// (`scope_label`, `find_latest_mneme_bak`, `claude_code_likely_running`, and
// `make_bar`). The latter three return either constant strings or progress
// bars — easy to pin without side-effects on disk or network.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // ---- scope_label ---------------------------------------------------

    #[test]
    fn scope_label_returns_canonical_strings() {
        // The exact tokens are part of the user-visible install report —
        // changing them would silently break log scrapers.
        assert_eq!(scope_label(InstallScope::Project), "project");
        assert_eq!(scope_label(InstallScope::User), "user");
        assert_eq!(scope_label(InstallScope::Global), "global");
    }

    // ---- find_latest_mneme_bak -----------------------------------------

    #[test]
    fn find_latest_mneme_bak_returns_none_when_no_backups_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("settings.json");
        std::fs::write(&target, b"{}").expect("write target");
        assert!(find_latest_mneme_bak(&target).is_none());
    }

    #[test]
    fn find_latest_mneme_bak_picks_newest_by_filename_sort() {
        // Backups are named `<file>.mneme-YYYYMMDD-HHMMSS.bak`; the
        // helper sorts descending by filename, so the largest timestamp
        // wins regardless of mtime.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("settings.json");
        std::fs::write(&target, b"{}").expect("write target");
        let older = dir.path().join("settings.json.mneme-20260101-000000.bak");
        let newer = dir.path().join("settings.json.mneme-20260426-120000.bak");
        std::fs::write(&older, b"old").expect("write older");
        std::fs::write(&newer, b"new").expect("write newer");

        let got = find_latest_mneme_bak(&target).expect("must find a backup");
        assert_eq!(got.file_name(), newer.file_name());
    }

    #[test]
    fn find_latest_mneme_bak_matches_against_pre_extension_stem() {
        // Helper accepts both `<full-name>.mneme-...` and
        // `<stem>.mneme-...` shapes — second branch covers files whose
        // backups dropped the extension.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("settings.json");
        std::fs::write(&target, b"{}").expect("write target");
        let stem_form = dir.path().join("settings.mneme-20260426-120000.bak");
        std::fs::write(&stem_form, b"stem").expect("write stem-form");
        let got = find_latest_mneme_bak(&target);
        assert!(got.is_some(), "stem-form backup should still match");
    }

    // ---- claude_code_likely_running -----------------------------------

    #[test]
    fn claude_code_likely_running_never_panics() {
        // Either the platform tool exists (then we get a bool) or the
        // helper falls back to false on spawn-error. Either way it is
        // total — no panic, no unhandled error.
        let _ = claude_code_likely_running();
    }

    // ---- make_bar ------------------------------------------------------

    #[test]
    fn make_bar_records_total_length() {
        // Sanity check: the helper must hand back a ProgressBar whose
        // length matches the requested value, regardless of style flags.
        let bar = make_bar(7);
        assert_eq!(bar.length(), Some(7));
        bar.finish_and_clear();
    }
}
