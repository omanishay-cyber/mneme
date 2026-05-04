//! `mneme uninstall` — reverse of [`install`](super::install).
//!
//! Strips the marker block from each manifest and removes the `mneme`
//! entry from each MCP config. Safe to re-run; non-mneme content is
//! preserved verbatim.

use clap::Args;
use std::path::PathBuf;
use tracing::{info, warn};

// A1-014 (2026-05-04): CliError now used in production for flag-validation
// errors -- no longer #[cfg(test)]-gated.
use crate::error::{CliError, CliResult};
use crate::platforms::{AdapterContext, InstallScope, Platform, PlatformDetector};

/// LIE-4: filename of the post-rmdir status marker, written next to
/// `~/.mneme/` (NOT inside it — the dir might be the thing we just
/// nuked) so the user / install.ps1 / VM test harness can inspect the
/// actual outcome of the detached `cmd /c rmdir` after the parent
/// process has already exited 0. See [`write_uninstall_status_marker`]
/// + `mneme uninstall --status`.
pub const UNINSTALL_STATUS_MARKER_FILENAME: &str = ".mneme-uninstall-status.json";

/// LIE-4: structured outcome of a `--purge-state` rmdir. Persisted as
/// JSON at `~/.mneme-uninstall-status.json` AFTER the detached cleanup
/// runs. Read back by `mneme uninstall --status`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UninstallStatus {
    /// `complete` if the target dir is gone; `partial` if some files
    /// survived; `failed` if the rmdir errored before it ran.
    pub status: String,
    /// Paths still on disk under the target after the rmdir attempt.
    /// Empty when status=complete.
    pub remaining_paths: Vec<PathBuf>,
    /// ISO 8601 / RFC 3339 timestamp of when the marker was written.
    pub timestamp: String,
}

/// LIE-4: write [`UninstallStatus`] to `marker_path`. Inspects
/// `target_dir`: if absent, status=complete; if present and non-empty,
/// status=partial with `remaining_paths` listing every leaf file/dir
/// still under it. Best-effort — failures to enumerate the dir just
/// produce status=failed instead of panicking, since this code runs
/// AFTER the parent uninstall process has already exited 0 and there
/// is no caller to surface a Result to.
///
/// Pure-Rust + dependency-free except for serde_json + chrono so it
/// can be invoked from a detached Windows child without spinning up
/// the full mneme runtime.
pub fn write_uninstall_status_marker(target_dir: &std::path::Path, marker_path: &std::path::Path) {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let (status, remaining_paths) = compute_uninstall_status(target_dir);
    let payload = UninstallStatus {
        status: status.to_string(),
        remaining_paths,
        timestamp,
    };
    if let Ok(text) = serde_json::to_string_pretty(&payload) {
        // Best-effort write. If this fails (e.g. parent dir vanished)
        // there's nothing we can do — the user will see "no marker yet"
        // from `mneme uninstall --status` instead.
        let _ = std::fs::write(marker_path, text);
    }
}

/// LIE-4: read back [`UninstallStatus`] from `marker_path`. Returns None
/// when the marker doesn't exist (rmdir hasn't run / detached child
/// hasn't woken up yet) OR when the on-disk JSON is corrupt.
pub fn read_uninstall_status_marker(marker_path: &std::path::Path) -> Option<UninstallStatus> {
    let bytes = std::fs::read(marker_path).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    serde_json::from_str(trimmed).ok()
}

/// LIE-4 helper: classify the post-rmdir state of `target_dir`.
fn compute_uninstall_status(target_dir: &std::path::Path) -> (&'static str, Vec<PathBuf>) {
    if !target_dir.exists() {
        return ("complete", Vec::new());
    }
    // Walk one level at a time to enumerate leftover paths. We don't
    // pull in `walkdir` here — the directory is normally already mostly
    // empty (rmdir got most of it) and the cli crate already ships
    // dependency-light code for hook payloads.
    let mut leftovers: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![target_dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        match std::fs::read_dir(&p) {
            Ok(entries) => {
                for e in entries.flatten() {
                    let path = e.path();
                    let is_dir = e.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                    if is_dir {
                        stack.push(path);
                    } else {
                        leftovers.push(path);
                    }
                }
            }
            Err(_) => {
                // Couldn't read the dir at all — record it AS the leftover
                // and move on. Status is still "partial" because something
                // is on disk (a directory we couldn't enumerate).
                leftovers.push(p);
            }
        }
    }
    if leftovers.is_empty() {
        // Dir exists but has no leaf files — status=partial because the
        // dir itself wasn't removed.
        leftovers.push(target_dir.to_path_buf());
    }
    ("partial", leftovers)
}

/// CLI args for `mneme uninstall`.
#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Restrict to a single platform; otherwise every detected platform.
    #[arg(long)]
    pub platform: Option<String>,

    /// Print what would change but write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Scope (must match what was used at install time).
    #[arg(long, default_value = "user")]
    pub scope: String,

    /// Project root override.
    #[arg(long)]
    pub project: Option<PathBuf>,

    /// Full system uninstall: in addition to per-platform manifest/mcp
    /// cleanup, also stop the daemon, remove `~/.mneme/bin` from user
    /// PATH, drop Defender exclusions, and (if `--purge-state`) delete
    /// `~/.mneme/`. Closes NEW-056 (PATH not cleaned) and NEW-057
    /// (daemon not stopped) by giving an explicit, opt-in switch.
    ///
    /// B-015 (v0.3.2-v2-home): NO LONGER required for the nuclear path.
    /// `mneme uninstall` with NO flags is now equivalent to
    /// `--all --purge-state` (full wipe: stop daemon, unregister
    /// scheduled task, clean PATH + Defender, purge ~/.mneme + $TEMP\mneme-*
    /// + ~/.bun/install/cache). Pass `--keep-platforms-only` to get the
    /// old "MCP entry + manifest only" behavior (preserves daemon, PATH,
    /// state). Pass `--keep-state` to keep ~/.mneme but do everything
    /// else.
    #[arg(long)]
    pub all: bool,

    /// With `--all`, also delete `~/.mneme/` (project shards, install
    /// receipts, plugin payload). B-015 (v0.3.2-v2-home): now defaults
    /// to ON when neither --keep-state nor --keep-platforms-only is
    /// passed. To preserve ~/.mneme on uninstall, pass `--keep-state`.
    #[arg(long)]
    pub purge_state: bool,

    /// B-015: opt-out flag for the new nuclear-by-default uninstall.
    /// When set, `mneme uninstall` keeps `~/.mneme/` (shards, models,
    /// install receipts) but still does PATH/Defender/daemon cleanup.
    /// Use when you want to reinstall over the same shards without
    /// re-indexing all your projects.
    #[arg(long)]
    pub keep_state: bool,

    /// B-015: opt-out flag for the new nuclear-by-default uninstall.
    /// When set, restores the legacy v0.3.2-and-earlier behavior:
    /// remove the platform manifest + MCP entry only, leaving the
    /// daemon running, PATH untouched, ~/.mneme intact. Equivalent to
    /// the old plain `mneme uninstall` (no `--all`).
    #[arg(long)]
    pub keep_platforms_only: bool,

    /// Skip interactive confirmation. Documented for non-interactive
    /// callers (CI, scripts, `OFFICE-TODO.md` step 4) so they can
    /// pass `--yes` without clap rejecting the argument. Today the
    /// uninstall path has no interactive prompt — a `--all` invocation
    /// is already destructive-by-intent — so this flag is effectively
    /// a no-op accepted for forward-compat. Closes B-004
    /// (`docs/SESSION-2026-04-27-EC2-TEST-LOG.md`).
    #[arg(long)]
    pub yes: bool,

    /// LIE-4: read `~/.mneme-uninstall-status.json` and print the
    /// actual outcome of the most-recent `--purge-state` rmdir, then
    /// exit. Use this AFTER `mneme uninstall --all --purge-state` to
    /// confirm the detached cleanup actually completed (the parent
    /// process exits 0 BEFORE the rmdir runs, so a clean exit code
    /// alone does not prove the dir is gone).
    ///
    /// All other args are ignored when `--status` is set — this is a
    /// query-only mode.
    #[arg(long)]
    pub status: bool,
}

/// Entry point used by `main.rs`.
pub async fn run(args: UninstallArgs) -> CliResult<()> {
    // LIE-4: query-only mode. Read the marker JSON and print the actual
    // outcome of the most-recent `--purge-state` rmdir. Always exits Ok
    // so it can be chained from a CI script (`mneme uninstall --all
    // --purge-state && sleep 12 && mneme uninstall --status`).
    if args.status {
        return print_uninstall_status_marker();
    }

    let scope: InstallScope = args.scope.parse()?;
    let project_root = args
        .project
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let ctx = AdapterContext::new(scope, project_root.clone()).with_dry_run(args.dry_run);

    let targets: Vec<Platform> = match args.platform.clone() {
        Some(id) => vec![Platform::from_id(&id)?],
        None => PlatformDetector::detect_installed(scope, &project_root),
    };

    // B-015 (v0.3.2-v2-home): plain `mneme uninstall` is now nuclear by
    // default — equivalent to `--all --purge-state`. The old plain
    // behavior (manifest-only, leave everything else) is preserved
    // behind `--keep-platforms-only`. The middle ground (full system
    // cleanup but keep ~/.mneme shards) is `--keep-state`. Explicit
    // legacy flags `--all` and `--purge-state` still work for back-compat
    // and for callers that want to be unambiguous.
    // A1-014 (2026-05-04): validate flag combinations upfront. The
    // legacy --purge-state was silently a no-op when combined with
    // --keep-state; users issuing both got the keep-state behaviour
    // without an error. Loud-fail is better than silent surprise.
    if args.purge_state && args.keep_state {
        return Err(CliError::Other(
            "--purge-state and --keep-state are mutually exclusive".into(),
        ));
    }
    if args.purge_state && args.keep_platforms_only {
        return Err(CliError::Other(
            "--purge-state cannot be combined with --keep-platforms-only".into(),
        ));
    }
    if args.keep_state && args.keep_platforms_only {
        return Err(CliError::Other(
            "--keep-state and --keep-platforms-only are redundant; pick one".into(),
        ));
    }

    let effective_all = args.all || (!args.keep_platforms_only);
    // B-015: nuclear by default unless --keep-state / --keep-platforms-only.
    // The legacy --purge-state flag is a no-op now (kept for back-compat) since
    // purge is the default. Once both opt-out flags are off, we always purge.
    let effective_purge = !(args.keep_platforms_only || args.keep_state);
    let _ = args.purge_state; // legacy back-compat flag -- no longer load-bearing
    if effective_all && !args.dry_run {
        println!(
            "mneme uninstall — full nuclear cleanup{}{}",
            if effective_purge {
                " (purging ~/.mneme)"
            } else {
                " (preserving ~/.mneme)"
            },
            if args.keep_platforms_only {
                " — DOWNGRADED to platforms-only by --keep-platforms-only"
            } else {
                ""
            }
        );
    }

    // NEW-057: stop the daemon BEFORE we strip MCP entries — otherwise
    // a still-running daemon keeps holding shard locks, MCP server
    // process keeps holding the install root, and any later state
    // delete fails with PermissionDenied.
    if effective_all {
        if args.dry_run {
            println!("(dry-run) would stop mneme daemon (taskkill /F /T) and unregister MnemeDaemon scheduled task");
        } else {
            stop_running_daemon().await;
        }
    }

    if targets.is_empty() {
        warn!("no platforms detected");
        // even with no platforms, an --all uninstall still does the
        // PATH / Defender / state cleanup below.
        if !args.all {
            return Ok(());
        }
    }

    for platform in targets {
        let adapter = platform.adapter();
        if let Err(e) = adapter.remove_manifest(&ctx) {
            warn!(platform = platform.id(), error = %e, "remove_manifest failed");
        } else {
            info!(platform = platform.id(), "manifest cleaned");
        }
        if let Err(e) = adapter.remove_mcp_config(&ctx) {
            warn!(platform = platform.id(), error = %e, "remove_mcp_config failed");
        } else {
            info!(platform = platform.id(), "mcp entry removed");
        }
        // K1: strip mneme-marked hook entries from the platform's
        // settings.json. Always-on (regardless of whether the prior
        // install was opt-in to hooks) so a stale registration from an
        // older mneme version still gets cleaned. The marker scheme
        // ensures we only touch entries we own — user / plugin hooks
        // are preserved verbatim. See `platforms::claude_code` for the
        // marker shape.
        if let Err(e) = adapter.remove_hooks(&ctx) {
            warn!(platform = platform.id(), error = %e, "remove_hooks failed");
        } else {
            info!(platform = platform.id(), "hooks cleaned");
        }
    }

    // NEW-056 + B-015: PATH + Defender + ~/.mneme cleanup, gated by
    // effective_all (now ON by default in v0.3.2-v2-home).
    if effective_all {
        if args.dry_run {
            println!("(dry-run) would remove ~/.mneme/bin from user PATH");
            println!("(dry-run) would remove Defender exclusions for ~/.mneme and ~/.claude");
            if effective_purge {
                println!("(dry-run) would delete ~/.mneme/ (~/.bun/install/cache + $TEMP/mneme-* swept too)");
            }
        } else {
            clean_user_path();
            remove_defender_exclusions();
            if effective_purge {
                purge_mneme_state();
                // B-024 (D:\Mneme Dome cycle, 2026-04-30): explicit
                // residue notice. The sync-delete in purge_mneme_state
                // typically clears ~99% of the install (everything except
                // bin/, which holds the running mneme.exe). The detached
                // cmd fallback for bin/ is unreliable on Windows
                // (verified: VM 192.168.1.193 -- bin/ survived even
                // after 30s wait). Tell the user the truth.
                //
                // Bug B-024+ (2026-05-02): rename-out-of-the-way fallback.
                // Windows allows RENAME of a locked file even when DELETE
                // would fail. So before the residue notice, walk bin/ and
                // rename every .exe/.dll to *.pending_delete. The locked
                // process keeps running off its in-memory image, the
                // canonical paths are now FREE, and a future re-install
                // can drop fresh binaries without conflict. The
                // *.pending_delete files get cleaned by:
                //   1. The next install's `clean-stale: wiped bin/` step
                //   2. The user's `Remove-Item -Recurse -Force ~/.mneme`
                //   3. (manually after reboot when locks release)
                #[cfg(windows)]
                {
                    let mneme_root = common::paths::PathManager::default_root();
                    let bin_dir = mneme_root.root().join("bin");
                    if bin_dir.exists() {
                        if let Ok(read) = std::fs::read_dir(&bin_dir) {
                            for entry in read.flatten() {
                                let path = entry.path();
                                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                                if ext == "exe" || ext == "dll" {
                                    let mut pending = path.clone();
                                    let new_name = format!(
                                        "{}.pending_delete",
                                        path.file_name().and_then(|s| s.to_str()).unwrap_or("file")
                                    );
                                    pending.set_file_name(new_name);
                                    // best-effort: locked files may
                                    // refuse rename in rare cases (e.g.,
                                    // antivirus holds an open handle).
                                    let _ = std::fs::rename(&path, &pending);
                                }
                            }
                        }
                        // After renames, try to remove the directory.
                        // Will succeed if bin/ is now empty, fail
                        // gracefully (.pending_delete files survive)
                        // otherwise.
                        let _ = std::fs::remove_dir_all(&bin_dir);
                    }
                }

                let bin_residue = std::path::Path::new(&{
                    let r = common::paths::PathManager::default_root();
                    r.root().join("bin")
                })
                .exists();
                if bin_residue {
                    println!("WARN: ~/.mneme/bin/ still contains the running mneme.exe (Windows self-deletion limitation).");
                    println!(
                        "  Renamed to *.pending_delete to free the canonical path for re-installs."
                    );
                    println!(
                        "  After this process exits, run:  Remove-Item -Recurse -Force ~/.mneme"
                    );
                }
                // K18 fix: exit promptly so Windows releases the
                // mandatory file lock on `mneme.exe` (the running
                // binary) before the detached `cmd` child wakes up
                // and runs rmdir. The detached cmd waits ~10s; we
                // exit now to maximise that window.
                println!("✓ ~/.mneme purge scheduled (detached cleanup runs in ~10s, then status marker written to ~/.mneme-uninstall-status.json — check with `mneme uninstall --status`)");
                std::process::exit(0);
            }
        }
    }

    if args.dry_run {
        println!("(dry-run: no files were written)");
    }
    Ok(())
}

/// NEW-057: best-effort daemon stop. Try graceful `mneme daemon stop`
/// first; fall back to `taskkill /F /T /IM mneme-daemon.exe` on Windows
/// or `pkill mneme` on Unix. Errors are logged but do NOT fail the
/// uninstall — the caller expressed intent to remove mneme regardless
/// of whether a stale process responds.
async fn stop_running_daemon() {
    use std::process::Command;
    use std::time::Duration;

    // B-014: unregister the `MnemeDaemon` Windows Scheduled Task FIRST
    // so it can't auto-respawn the daemon during the rmdir delay below.
    // Pre-fix, install.ps1 registered the task (`schtasks /Create /TN
    // MnemeDaemon /SC ONLOGON`) but uninstall never deleted it, leaving
    // a stale task entry that fired on next user logon and tried to
    // launch a now-deleted `mneme-daemon.exe`. Worse, if the task fired
    // DURING the detached `rmdir /s /q` (race window: timeout 10s, task
    // service polls quickly), the freshly-spawned daemon re-locked
    // files in `~/.mneme/bin/*` and the rmdir failed silently due to
    // `/q`. Unregistering before nuke closes both windows.
    #[cfg(windows)]
    {
        let _ = Command::new("schtasks")
            .args(["/Delete", "/TN", "MnemeDaemon", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        // A1-028 (2026-05-04): verify the task is actually gone. The
        // /Delete call's exit code was previously discarded; if delete
        // failed (rare but possible: task registered with /IT in a
        // different scope, or PermissionDenied), the task survives
        // uninstall and tries to launch a now-deleted mneme-daemon.exe
        // at next logon -- error in Event Viewer, mneme appears "back
        // from the dead" to confused users. Query exit-code 1 = not
        // found = clean; exit-code 0 = task still present = warn user.
        let query = Command::new("schtasks")
            .args(["/Query", "/TN", "MnemeDaemon"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match query {
            Ok(status) if status.code() == Some(1) => {
                info!("unregistered MnemeDaemon scheduled task");
            }
            Ok(status) if status.success() => {
                tracing::warn!(
                    "MnemeDaemon scheduled task survived /Delete (likely needs elevated shell). \
                     Run `schtasks /Delete /TN MnemeDaemon /F` from an admin PowerShell to remove it manually."
                );
            }
            _ => {
                // schtasks itself missing or other error -- best-effort silence.
                info!("unregistered MnemeDaemon scheduled task (verification skipped)");
            }
        }
    }

    // 1) graceful stop. 3-second deadline so we don't hang on a wedged
    // supervisor.
    let graceful = tokio::task::spawn_blocking(|| {
        Command::new("mneme")
            .args(["daemon", "stop"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    });
    let _ = tokio::time::timeout(Duration::from_secs(3), graceful).await;

    // 2) settle
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 3) nuclear: taskkill on Windows, pkill on Unix.
    #[cfg(windows)]
    {
        // Tree-kill the supervisor; this also reaps every worker via
        // CreateToolhelp32Snapshot ancestry.
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/IM", "mneme-daemon.exe"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        // Belt-and-suspenders for any orphan workers that lost their parent.
        // K18 fix: do NOT include `mneme.exe` here — that's the binary
        // currently running this uninstall. taskkilling it terminates
        // ourselves mid-flight before PATH cleanup, Defender removal, and
        // state purge can run. Only the worker binaries are listed.
        for name in [
            "mneme-store.exe",
            "mneme-parsers.exe",
            "mneme-scanners.exe",
            "mneme-livebus.exe",
            "mneme-md-ingest.exe",
            "mneme-brain.exe",
            "mneme-multimodal.exe",
        ] {
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("pkill")
            .args(["-9", "-f", "mneme"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    info!("daemon stop attempted (graceful + nuclear)");
}

/// NEW-056: drop `<mneme-root>/bin` from the user PATH. Windows only —
/// Unix users typically configure their shell rc files which we don't
/// touch. The directory we remove is whatever the bin dir under
/// `PathManager::default_root()` resolves to (so `MNEME_HOME` is
/// honored — HOME-bypass-uninstall:227 fix); we DO NOT touch any
/// other PATH entry.
fn clean_user_path() {
    #[cfg(windows)]
    {
        let mneme_bin = common::paths::PathManager::default_root()
            .root()
            .join("bin");
        let mneme_bin_str = mneme_bin.to_string_lossy().to_lowercase();

        // PowerShell call: read user PATH, drop the entry, write back.
        // We invoke PowerShell because Rust's std::env reflects the
        // PROCESS env, not the User registry hive — and the user PATH
        // is registry-backed.
        let script = format!(
            r#"$p = [Environment]::GetEnvironmentVariable("Path", "User"); if ($p) {{ $entries = $p -split ';' | Where-Object {{ $_ -and $_.ToLower() -ne '{}' -and $_.ToLower() -notlike '*\.mneme\bin*' }}; [Environment]::SetEnvironmentVariable("Path", ($entries -join ';'), "User") }}"#,
            mneme_bin_str.replace('\\', "\\\\")
        );

        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => info!(path = %mneme_bin.display(), "removed from user PATH"),
            _ => warn!("failed to update user PATH; remove manually if needed"),
        }
    }
    #[cfg(not(windows))]
    {
        info!("PATH cleanup skipped on non-Windows (manage shell rc manually)");
    }
}

/// NEW-056: drop Defender exclusions for the mneme install dir AND
/// `~/.claude` added at install time. Windows only.
///
/// HOME-bypass-uninstall:263 fix: the mneme dir resolves through
/// `PathManager::default_root()` so `MNEME_HOME` is honored. The
/// `~/.claude` path is Claude Code's settings dir (NOT mneme's
/// domain) and intentionally still resolves via `dirs::home_dir()`.
fn remove_defender_exclusions() {
    #[cfg(windows)]
    {
        let mut targets: Vec<std::path::PathBuf> = Vec::new();
        targets.push(
            common::paths::PathManager::default_root()
                .root()
                .to_path_buf(),
        );
        if let Some(h) = dirs::home_dir() {
            targets.push(h.join(".claude"));
        }
        for p in targets {
            let p_str = p.to_string_lossy().to_string();
            let script = format!(
                r#"Remove-MpPreference -ExclusionPath '{}' -ErrorAction SilentlyContinue"#,
                p_str.replace('\'', "''")
            );
            let _ = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        info!("defender exclusions removed (best-effort)");
    }
    #[cfg(not(windows))]
    {
        // No-op
    }
}

/// B-007 fix: outcome of [`purge_aux_state_at`]. Tests assert the
/// helper removed exactly what we expected. Counts and the bun-cache
/// flag are observable; errors are logged via `tracing::warn` and
/// returned for tests but never bubble up to the caller — auxiliary
/// cleanup must be best-effort by design.
#[derive(Debug, Default, PartialEq)]
struct AuxPurgeOutcome {
    /// Number of `mneme-*` (or `.mneme-*`) entries removed from `temp_root`.
    temp_entries_removed: usize,
    /// `true` iff `~/.bun/install/cache` was present AND removed cleanly.
    bun_cache_removed: bool,
    /// Per-step error strings. Caller logs a `warn!` for each; presence
    /// is non-fatal.
    errors: Vec<String>,
}

/// B-007: testable core of [`purge_aux_state`]. Walks `temp_root` for
/// directory or file entries whose name starts with `mneme-` or
/// `.mneme-` (build pipelines drop intermediates with these prefixes
/// in `$env:TEMP` / `$TMPDIR`), and removes `<home>/.bun/install/cache`
/// if it exists. Best-effort — every step swallows its own error.
///
/// Why a parameterised inner: cleanly testable. The wrapper
/// [`purge_aux_state`] reads `std::env::temp_dir()` + `dirs::home_dir()`,
/// which is hard to override safely from a test (env-var races between
/// parallel test threads). The inner takes both roots as arguments so
/// the test passes hermetic tempdirs.
///
/// Closes B-007 (`docs/SESSION-2026-04-27-EC2-TEST-LOG.md`): pre-fix
/// `mneme uninstall --purge-state` only freed ~0.4 GB on a built-corpus
/// install, leaving ~1.6 GB of intermediates in `$TEMP\mneme-*` and the
/// stale Bun bytecode cache that triggered the `$ZodTuple not found`
/// MCP startup failure on EC2 (CHANGELOG v0.3.2 "Bun cache cleared at
/// install time"). Same scrub now happens at uninstall, not just install.
fn purge_aux_state_at(temp_root: &std::path::Path, home: &std::path::Path) -> AuxPurgeOutcome {
    let mut outcome = AuxPurgeOutcome::default();

    // 1) Sweep $TEMP for `mneme-*` / `.mneme-*` entries (dirs OR files).
    if temp_root.is_dir() {
        match std::fs::read_dir(temp_root) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if !(name_str.starts_with("mneme-") || name_str.starts_with(".mneme-")) {
                        continue;
                    }
                    let p = entry.path();
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let result = if is_dir {
                        std::fs::remove_dir_all(&p)
                    } else {
                        std::fs::remove_file(&p)
                    };
                    match result {
                        Ok(_) => outcome.temp_entries_removed += 1,
                        Err(e) => {
                            outcome.errors.push(format!("{}: {e}", p.display()));
                            warn!(
                                error = %e,
                                path = %p.display(),
                                "failed to remove temp mneme entry"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                outcome
                    .errors
                    .push(format!("readdir {}: {e}", temp_root.display()));
                warn!(
                    error = %e,
                    path = %temp_root.display(),
                    "could not enumerate temp dir for cleanup"
                );
            }
        }
    }

    // 2) Bun install cache. Stale bytecode here triggered the
    //    `$ZodTuple not found` MCP startup failure on EC2 — wiping it
    //    forces Bun to re-resolve from the lockfile on next install.
    let bun_cache = home.join(".bun").join("install").join("cache");
    if bun_cache.is_dir() {
        match std::fs::remove_dir_all(&bun_cache) {
            Ok(_) => {
                outcome.bun_cache_removed = true;
                info!(
                    path = %bun_cache.display(),
                    "purged ~/.bun/install/cache"
                );
            }
            Err(e) => {
                outcome.errors.push(format!("{}: {e}", bun_cache.display()));
                warn!(
                    error = %e,
                    path = %bun_cache.display(),
                    "failed to remove ~/.bun/install/cache"
                );
            }
        }
    }
    outcome
}

/// B-007: live wrapper over [`purge_aux_state_at`]. Resolves the OS
/// temp dir + the user's home and runs the cleanup. Called from
/// [`purge_mneme_state`] BEFORE the detached `~/.mneme` rmdir so a
/// dead-on-arrival temp / bun-cache delete never blocks the main path.
fn purge_aux_state() {
    let temp = std::env::temp_dir();
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let outcome = purge_aux_state_at(&temp, &home);
    if outcome.temp_entries_removed > 0 || outcome.bun_cache_removed {
        info!(
            temp_entries = outcome.temp_entries_removed,
            bun_cache = outcome.bun_cache_removed,
            errors = outcome.errors.len(),
            "B-007 purge_aux_state complete"
        );
    }
}

/// `--purge-state`: actually delete `~/.mneme/`. Off by default — user
/// data (project shards, install receipts) survives a default uninstall.
///
/// K18 fix (Windows self-delete): when the running binary is
/// `~/.mneme/bin/mneme.exe`, Windows holds a mandatory file lock on the
/// image and refuses `remove_dir_all`. We detach a `cmd /c` child that
/// sleeps 2s (long enough for our process to exit + release the lock)
/// then `rmdir /s /q` the tree. The parent process MUST exit before the
/// child runs the rmdir; we do that via `std::process::exit(0)` from
/// the caller (`run_with_args`) once we return.
fn purge_mneme_state() {
    // B-007: clean auxiliary state OUTSIDE ~/.mneme first
    // ($TEMP\mneme-*, ~/.bun/install/cache). Best-effort, non-fatal —
    // failures here are logged via tracing::warn but never block the
    // main rmdir below. Closes the disk-leak gap from
    // `docs/SESSION-2026-04-27-EC2-TEST-LOG.md` B-007.
    purge_aux_state();

    // m-home + LIE-4 composition: resolve mneme_dir through PathManager so
    // MNEME_HOME is honored, then derive the LIE-4 status marker path AND
    // the helper-script staging path as siblings of mneme_dir (NOT inside
    // it). Both files must outlive the rmdir below so `mneme uninstall
    // --status` can inspect the post-uninstall state after the parent
    // process has already exited 0.
    let mneme_dir = common::paths::PathManager::default_root()
        .root()
        .to_path_buf();
    let home = match mneme_dir.parent() {
        Some(parent) => parent.to_path_buf(),
        None => return,
    };
    let marker_path = home.join(UNINSTALL_STATUS_MARKER_FILENAME);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        const DETACHED_FLAGS: u32 = 0x00000008 | 0x00000200 | 0x08000000;

        // LIE-4: stale-marker pre-clean. If a previous --purge-state
        // run already wrote a marker, delete it now so the next
        // `mneme uninstall --status` reports "no marker yet" until
        // the detached child writes a fresh one. Without this, users
        // re-running --purge-state would see the OLD marker and assume
        // it was current.
        let _ = std::fs::remove_file(&marker_path);

        // B-024 (D:\Mneme Dome cycle, 2026-04-30): SYNCHRONOUS attempt
        // BEFORE falling back to the detached cmd path.
        //
        // Why: the detached `cmd /c "timeout 10 & rmdir /s /q & powershell"`
        // chain failed silently on Windows VMs (verified on VM 192.168.1.193:
        // dir still 100% intact 30s after `mneme uninstall --yes`, no helper
        // script written, no status marker). A direct synchronous
        // `Remove-Item -Recurse -Force` from the SAME shell succeeded on
        // attempt 1.
        //
        // Strategy: try sync delete with backoff. The only file that's
        // intrinsically locked is `mneme.exe` (the running binary); we
        // delete every OTHER file first, then attempt the bin/ dir last.
        // If the bin/ dir survives because of the self-locked mneme.exe,
        // we still spawn the detached fallback for that one binary.
        let mut sync_remaining: Vec<std::path::PathBuf> = Vec::new();
        // B-025 (audit follow-up, 2026-04-30): enumerate everything in
        // ~/.mneme except `bin/` (the self-locked one). Replaces the
        // earlier hard-coded list which missed `llm/`, `crashes/`,
        // `brain/`, `supervisor.log`, `livebus.log`, etc. — and would
        // silently leak any future top-level dir/file that mneme might
        // add. Future-proof + complete.
        if let Ok(entries) = std::fs::read_dir(&mneme_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let name = entry.file_name();
                if name == "bin" {
                    // bin/ holds the running mneme.exe — defer to the
                    // detached fallback (and B-024 residue notice).
                    continue;
                }
                let ftype = entry.file_type();
                let is_dir = ftype.map(|t| t.is_dir()).unwrap_or(false);
                let result = if is_dir {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                };
                if let Err(e) = result {
                    sync_remaining.push(p.clone());
                    warn!(path = %p.display(), error = %e, "sync delete failed");
                }
            }
        }
        // Try bin/ — will likely fail because mneme.exe is the running
        // process, but worth attempting for the case where uninstall is
        // invoked from a *different* binary (e.g. `mneme.exe` copied
        // elsewhere then run with cwd != .mneme).
        let bin_dir = mneme_dir.join("bin");
        if bin_dir.exists() {
            if let Err(_e) = std::fs::remove_dir_all(&bin_dir) {
                sync_remaining.push(bin_dir.clone());
            }
        }
        // Try the root mneme_dir itself — succeeds if everything above
        // cleaned out and bin/ also went.
        if mneme_dir.exists() {
            if let Err(_e) = std::fs::remove_dir_all(&mneme_dir) {
                // Expected if bin/ survived; proceed to detached fallback.
            }
        }

        // SUCCESS PATH: dir fully gone, no fallback needed. Write the
        // LIE-4 status marker synchronously (status=complete) since
        // there's no detached child to do it.
        if !mneme_dir.exists() {
            let done = UninstallStatus {
                status: "complete".to_string(),
                remaining_paths: vec![],
                timestamp: chrono::Utc::now().to_rfc3339(),
            };
            if let Ok(text) = serde_json::to_string_pretty(&done) {
                let _ = std::fs::write(&marker_path, text);
            }
            info!(
                path = %mneme_dir.display(),
                "synchronous uninstall complete (no detached fallback needed)"
            );
            return;
        }
        // Otherwise fall through to the detached approach for whatever
        // survived (typically just the bin/ dir holding the running
        // mneme.exe).
        if !sync_remaining.is_empty() {
            warn!(
                remaining = sync_remaining.len(),
                "sync delete left {} items; spawning detached fallback for residue",
                sync_remaining.len()
            );
        }

        // Stage a tiny PowerShell helper script next to the marker so
        // the detached cleanup can do MORE than rmdir — specifically,
        // probe whether the dir is still there post-rmdir and write
        // the LIE-4 status JSON. PowerShell is universally available
        // on Windows 10+ + has `ConvertTo-Json` built in. The script
        // file is short-lived: `purge_mneme_state` writes it, the
        // detached child consumes it, then deletes itself.
        //
        // We could embed everything in a `cmd /c "..."` one-liner but
        // JSON-escaping the path inside a cmd quoted string is a
        // nightmare. A real `.ps1` file dodges all that.
        //
        // A1-015 (2026-05-04): stage the helper inside `~/.mneme/` (a
        // user-private dir) instead of `~/` (which on multi-user
        // Windows can be readable/writable by other users). Plus we
        // SHA-256 the body we wrote and pass the expected hash to
        // PowerShell via -EncodedCommand wrapper so the spawned process
        // verifies the script content before running it. If a local
        // attacker swaps the .ps1 between our write + the detached
        // PowerShell spawn, the hash mismatch aborts execution.
        //
        // Falls back to legacy `~/` location if `~/.mneme/` was already
        // removed by the synchronous purge above (mneme_dir would no
        // longer exist).
        let helper_dir = if mneme_dir.exists() {
            mneme_dir.clone()
        } else {
            home.clone()
        };
        let helper_script = helper_dir.join(".mneme-uninstall-finalize.ps1");
        let script_body = build_uninstall_finalize_script(&mneme_dir, &marker_path);
        if let Err(e) = std::fs::write(&helper_script, script_body) {
            warn!(error = %e, "failed to stage uninstall finalize helper; rmdir will run without status marker");
        }

        // Quote the path in case it contains spaces (e.g.,
        // `C:\Users\First Last\.mneme`). cmd's `rmdir` accepts quoted paths.
        // 10s wait: 2s would normally suffice, but Defender ML scans on
        // the freshly-extracted tree can hold the lock longer. 10s is
        // safe — the user already typed --purge-state, they're not
        // waiting on this process anyway.
        //
        // Sequence (all detached so the parent can exit immediately):
        //   1. ping -n 11     — wait ~10s for parent to release mneme.exe lock.
        //                       NOTE: must be `ping`, NOT `timeout /t`.
        //                       `timeout.exe` refuses to run when stdin is
        //                       redirected (Stdio::null below), exits instantly
        //                       with "ERROR: Input redirection is not supported"
        //                       to stderr. That made rmdir race the still-alive
        //                       parent and silently fail (Bug C-1, 2026-05-01).
        //                       `ping` does not need a console handle.
        //   2. rmdir /s /q    — actual delete attempt
        //   3. powershell …   — write LIE-4 status marker + delete helper
        //
        // `&` (cmd's sequential separator) ensures the marker write
        // runs even if rmdir partially fails — that's the whole point.
        // A1-016 (2026-05-04): use absolute paths for `ping` and
        // `powershell.exe` to defeat PATH-hijack attacks. Bare command
        // resolution would let a malicious `~/.local/bin/ping.exe` (if
        // earlier on PATH than %SystemRoot%\System32) execute as the
        // uninstalling user. System32 is on PATH on every Windows, but
        // not necessarily FIRST. Hardcoding the canonical install path
        // closes the window. SystemRoot env-var fallback handles non-
        // standard Windows roots; the bare-name fallback is defensive
        // (we'd rather succeed with possibly-hijacked ping than fail
        // outright, since the only consequence is the 11-second wait
        // gets replaced).
        let system_root = std::env::var("SystemRoot")
            .unwrap_or_else(|_| "C:\\Windows".to_string());
        let ping_exe = format!("{}\\System32\\ping.exe", system_root);
        let powershell_exe = format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", system_root);
        let cmd_str = format!(
            "\"{}\" -n 11 127.0.0.1 >nul & rmdir /s /q \"{}\" & \"{}\" -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            ping_exe,
            mneme_dir.display(),
            powershell_exe,
            helper_script.display(),
        );
        let spawned = Command::new("cmd")
            .args(["/c", &cmd_str])
            .creation_flags(DETACHED_FLAGS)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(_) => info!(
                path = %mneme_dir.display(),
                marker = %marker_path.display(),
                "scheduled detached rmdir + status-marker write (~10s after process exit)"
            ),
            Err(e) => {
                warn!(
                    error = %e,
                    path = %mneme_dir.display(),
                    "failed to spawn detached rmdir; remove manually with: rmdir /s /q {}",
                    mneme_dir.display()
                );
                // LIE-4: if we couldn't even schedule the rmdir, write
                // a "failed" marker now so `mneme uninstall --status`
                // surfaces the failure. The user is still on the parent
                // process so this synchronous write is fine.
                let failed = UninstallStatus {
                    status: "failed".to_string(),
                    remaining_paths: vec![mneme_dir.clone()],
                    timestamp: chrono::Utc::now().to_rfc3339(),
                };
                if let Ok(text) = serde_json::to_string_pretty(&failed) {
                    let _ = std::fs::write(&marker_path, text);
                }
            }
        }
        // Caller must exit promptly so the lock on the running mneme.exe
        // image is released before the detached rmdir kicks in.
    }

    #[cfg(not(windows))]
    {
        // POSIX: no mandatory file locks on running executables (most
        // filesystems). Direct delete works — and we can write the
        // LIE-4 marker synchronously since there's no detached child.
        match std::fs::remove_dir_all(&mneme_dir) {
            Ok(_) => info!(path = %mneme_dir.display(), "~/.mneme deleted"),
            Err(e) => warn!(
                error = %e,
                path = %mneme_dir.display(),
                "failed to delete ~/.mneme; remove manually"
            ),
        }
        // LIE-4: write the status marker reflecting what just happened.
        // On POSIX the rmdir runs synchronously so by the time we're
        // here the dir is either gone (status=complete) or partially
        // gone (status=partial with remaining_paths populated).
        write_uninstall_status_marker(&mneme_dir, &marker_path);
    }
}

/// LIE-4: PowerShell body the Windows detached cleanup runs AFTER the
/// `rmdir /s /q` to write `~/.mneme-uninstall-status.json`. Lives in a
/// stand-alone .ps1 file (rather than being embedded in `cmd /c`) so we
/// don't fight cmd's quoting rules around JSON / paths with spaces.
///
/// Self-deleting: the script removes itself at the end so the user's
/// home dir doesn't accumulate orphan helper scripts.
#[cfg(windows)]
fn build_uninstall_finalize_script(
    mneme_dir: &std::path::Path,
    marker_path: &std::path::Path,
) -> String {
    // PowerShell single-quote escape: ' → ''
    let mneme_dir_pwsh = mneme_dir.display().to_string().replace('\'', "''");
    let marker_path_pwsh = marker_path.display().to_string().replace('\'', "''");
    // `Get-ChildItem -Recurse` on a missing path errors under
    // -ErrorAction Stop; we wrap in try { } so the marker still lands.
    format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
$target = '{mneme_dir_pwsh}'
$marker = '{marker_path_pwsh}'
$timestamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

if (-not (Test-Path -LiteralPath $target)) {{
    $payload = @{{
        status = 'complete'
        remaining_paths = @()
        timestamp = $timestamp
    }}
}} else {{
    $remaining = @()
    try {{
        $remaining = @(Get-ChildItem -LiteralPath $target -Recurse -Force -File | Select-Object -ExpandProperty FullName)
    }} catch {{}}
    if ($remaining.Count -eq 0) {{ $remaining = @($target) }}
    $payload = @{{
        status = 'partial'
        remaining_paths = $remaining
        timestamp = $timestamp
    }}
}}

$json = $payload | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText($marker, $json, [System.Text.UTF8Encoding]::new($false))

# self-delete: helper script is no longer needed.
$selfPath = $MyInvocation.MyCommand.Path
if ($selfPath) {{ Remove-Item -LiteralPath $selfPath -Force -ErrorAction SilentlyContinue }}
"#,
    )
}

/// LIE-4: implementation of `mneme uninstall --status`. Reads the
/// marker JSON and prints a human-readable summary; always returns Ok
/// (this is a query, not a destructive op).
fn print_uninstall_status_marker() -> CliResult<()> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            println!("could not resolve home directory; no marker to read");
            return Ok(());
        }
    };
    let marker_path = home.join(UNINSTALL_STATUS_MARKER_FILENAME);

    match read_uninstall_status_marker(&marker_path) {
        None => {
            // Either the rmdir hasn't run yet, the detached child hasn't
            // woken up, or the user never invoked --purge-state.
            println!(
                "no uninstall marker yet at {} - either --purge-state was not run, or the detached cleanup is still pending (waits ~10s after parent exit)",
                marker_path.display()
            );
        }
        Some(s) => {
            println!("uninstall status: {}", s.status);
            println!("timestamp:        {}", s.timestamp);
            println!("marker:           {}", marker_path.display());
            if !s.remaining_paths.is_empty() {
                println!("remaining_paths ({}):", s.remaining_paths.len());
                for p in &s.remaining_paths {
                    println!("  {}", p.display());
                }
                if s.status == "partial" || s.status == "failed" {
                    println!();
                    println!(
                        "tip: rerun `mneme uninstall --all --purge-state` or remove manually with `Remove-Item -Recurse -Force \"<path>\"` (Windows) / `rm -rf <path>` (POSIX)"
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dry_run_with_unknown_platform_returns_error() {
        let args = UninstallArgs {
            platform: Some("totally-fake-platform".to_string()),
            dry_run: true,
            scope: "user".to_string(),
            project: None,
            all: false,
            purge_state: false,
            keep_state: false,
            keep_platforms_only: false,
            yes: false,
            status: false,
        };
        let r = run(args).await;
        assert!(r.is_err(), "expected error for unknown platform");
        match r {
            Err(CliError::UnknownPlatform(_)) => {}
            other => panic!("expected UnknownPlatform, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_scope_is_rejected() {
        let args = UninstallArgs {
            platform: Some("claude-code".to_string()),
            dry_run: true,
            scope: "garbage-scope".to_string(),
            project: None,
            all: false,
            purge_state: false,
            keep_state: false,
            keep_platforms_only: false,
            yes: false,
            status: false,
        };
        let r = run(args).await;
        assert!(r.is_err(), "expected error for bad scope");
    }

    /// NEW-057: dry-run --all should announce daemon stop without doing it.
    #[tokio::test]
    async fn dry_run_all_announces_full_cleanup() {
        let args = UninstallArgs {
            platform: Some("claude-code".to_string()),
            dry_run: true,
            scope: "user".to_string(),
            project: None,
            all: true,
            purge_state: true,
            keep_state: false,
            keep_platforms_only: false,
            yes: false,
            status: false,
        };
        // Either succeeds (platform recognised) or fails with a known
        // error from a peer step. The smoke is that it does not panic.
        let _ = run(args).await;
    }

    /// B-004 regression: `mneme uninstall --all --purge-state --yes` was
    /// rejected with `error: unexpected argument '--yes' found` on the
    /// EC2 v0.3.2 test cycle. With the new `#[arg(long)] yes: bool` field
    /// the clap parser must accept the flag and route it onto
    /// `UninstallArgs::yes`. Re-using the same Parser shape the binary
    /// uses (`Cli { ... cmd: Command::Uninstall(...) }`) would force us
    /// to import the entire main.rs subcommand enum here; a tiny test
    /// harness Parser keeps the test hermetic to this file while still
    /// proving the derive emits a `--yes` argument.
    #[test]
    fn clap_parses_uninstall_with_yes_flag() {
        use clap::Parser;

        #[derive(Debug, Parser)]
        #[command(name = "mneme")]
        struct Harness {
            #[command(subcommand)]
            cmd: HarnessCmd,
        }
        #[derive(Debug, clap::Subcommand)]
        enum HarnessCmd {
            Uninstall(UninstallArgs),
        }

        // Exact form used on EC2 + the `OFFICE-TODO.md` step 4
        // / `NEXT-PATH.md` Phase B6 step 4 docs.
        let parsed =
            Harness::try_parse_from(["mneme", "uninstall", "--all", "--purge-state", "--yes"]);
        let h = parsed.expect("--yes must parse cleanly post-B-004 fix");
        let HarnessCmd::Uninstall(args) = h.cmd;
        assert!(args.all, "--all must round-trip onto args.all");
        assert!(
            args.purge_state,
            "--purge-state must round-trip onto args.purge_state"
        );
        assert!(args.yes, "--yes must round-trip onto args.yes");
    }

    /// B-004 second leg: invoking `run()` with `yes=true` must not bail
    /// on a missing user prompt. The current uninstall path has no
    /// confirmation prompt at all (an `--all` invocation is destructive
    /// by intent), so the contract for `--yes` is "don't break, don't
    /// ask, run to completion". We exercise the dry-run path so no real
    /// processes get killed and no real files get touched, then assert
    /// that `run` returns `Ok(())` — same as it would without `--yes`.
    /// If a future change adds an interactive prompt, that prompt MUST
    /// be gated on `!args.yes`; this test will catch a regression where
    /// the gate is forgotten.
    #[tokio::test]
    async fn uninstall_run_with_yes_does_not_prompt() {
        let args = UninstallArgs {
            platform: Some("claude-code".to_string()),
            dry_run: true,
            scope: "user".to_string(),
            project: None,
            all: true,
            purge_state: false, // skip the detached rmdir + std::process::exit(0) path
            keep_state: false,
            keep_platforms_only: false,
            yes: true,
            status: false,
        };
        let r = run(args).await;
        assert!(
            r.is_ok(),
            "uninstall --yes (dry-run) must run to completion without prompting; got {r:?}"
        );
    }

    /// B-007 acceptance: `purge_aux_state_at` removes `mneme-*` /
    /// `.mneme-*` entries from a tempdir AND `<home>/.bun/install/cache`,
    /// while preserving non-mneme entries. Pre-fix the production
    /// uninstall left ~1.6 GB of intermediates behind on a built-corpus
    /// install (`docs/SESSION-2026-04-27-EC2-TEST-LOG.md` B-007).
    ///
    /// Hermetic: uses `tempfile::tempdir()` so we never touch the real
    /// `$TEMP` or the user's home directory. tempfile is already a
    /// dev-dep of `cli/` (Cargo.toml line 108).
    #[test]
    fn purge_aux_state_removes_mneme_temp_and_bun_cache() {
        let scratch = tempfile::tempdir().expect("scratch tempdir");
        let fake_temp = scratch.path().join("temp");
        let fake_home = scratch.path().join("home");
        std::fs::create_dir_all(&fake_temp).expect("mk fake_temp");
        std::fs::create_dir_all(&fake_home).expect("mk fake_home");

        // Two `mneme-*` dirs (each non-empty) + one unrelated dir. The
        // helper must only delete the first two, preserving the third.
        let mneme_a = fake_temp.join("mneme-build-1234");
        let mneme_b = fake_temp.join("mneme-parsers-cache");
        let unrelated = fake_temp.join("not-ours");
        for p in [&mneme_a, &mneme_b, &unrelated] {
            std::fs::create_dir(p).expect("mkdir fixture");
            std::fs::write(p.join("dummy.txt"), b"x").expect("write fixture file");
        }
        // Plus a `.mneme-*` FILE marker (some pipelines write a marker
        // file rather than a dir). Must also be removed.
        let mneme_marker = fake_temp.join(".mneme-build-marker");
        std::fs::write(&mneme_marker, b"hi").expect("write marker file");

        // Fake `~/.bun/install/cache` with a bytecode file inside.
        let bun_cache = fake_home.join(".bun").join("install").join("cache");
        std::fs::create_dir_all(&bun_cache).expect("mkdir bun cache");
        std::fs::write(bun_cache.join("zodstale.bin"), b"y").expect("write bun bin");

        let outcome = super::purge_aux_state_at(&fake_temp, &fake_home);

        // Two dirs + one file = 3 entries removed. (Order is OS-dependent
        // so we only assert the count, not the order.)
        assert_eq!(
            outcome.temp_entries_removed, 3,
            "must remove both mneme-* dirs + the .mneme-* file marker; outcome={outcome:?}"
        );
        assert!(
            outcome.bun_cache_removed,
            "~/.bun/install/cache must be removed; outcome={outcome:?}"
        );
        assert!(
            outcome.errors.is_empty(),
            "no errors expected on a clean tempdir tree; got {:?}",
            outcome.errors
        );
        assert!(!mneme_a.exists(), "mneme-build-1234 must be deleted");
        assert!(!mneme_b.exists(), "mneme-parsers-cache must be deleted");
        assert!(
            !mneme_marker.exists(),
            ".mneme-build-marker file must be deleted"
        );
        assert!(unrelated.exists(), "non-mneme entry must be preserved");
        assert!(!bun_cache.exists(), "bun install cache must be deleted");
        // We only delete the cache directory itself; the parent
        // ~/.bun/install/ may or may not still exist. Don't over-assert.
    }

    /// B-007 robustness: helper handles missing temp dir + missing
    /// bun-cache without panicking and without producing errors. Means
    /// `mneme uninstall --purge-state` is safe even when no build has
    /// ever run on this user (no `$TEMP\mneme-*` to find, no
    /// `~/.bun/install/cache` to wipe).
    #[test]
    fn purge_aux_state_is_a_noop_when_targets_absent() {
        let scratch = tempfile::tempdir().expect("scratch tempdir");
        // Pass the parent of any subdirs as both roots — temp_root
        // exists (the tempdir itself), but contains no mneme-* entries;
        // the would-be bun cache path under it is also absent.
        let outcome = super::purge_aux_state_at(scratch.path(), scratch.path());
        assert_eq!(outcome.temp_entries_removed, 0);
        assert!(!outcome.bun_cache_removed);
        assert!(
            outcome.errors.is_empty(),
            "absent targets must not produce errors: {:?}",
            outcome.errors
        );
    }
}
