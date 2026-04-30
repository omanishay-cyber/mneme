//! `mneme doctor` — health check / self-test.
//!
//! v0.3.1: human-readable summary first (closes F-006 from the
//! install-report — prior output was an unbounded raw-JSON dump),
//! optional `--json` for machine output. Diagnostics run in-process
//! (version, runtime/state dir writable, Windows MSVC build toolchain)
//! plus a live supervisor ping.
//! v0.3.1+: per-MCP-tool probe — spawns a fresh `mneme mcp stdio`
//! child, runs a JSON-RPC `initialize` + `tools/list` handshake, and
//! reports a ✓ for every tool the MCP server actually exposes.
//! v0.3.1++: Windows MSVC probe expanded to four signals (link.exe,
//! cl.exe, vswhere with VC.Tools.x86.x64 component, Windows SDK
//! kernel32.lib) plus a one-line PASS/FAIL summary row. Closes I-16.

use clap::Args;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};
use tracing::warn;

use crate::commands::build::make_client;
use crate::error::CliResult;
use crate::ipc::{IpcRequest, IpcResponse};

/// Single source of truth for the copyright line printed in the
/// banner. Anish confirmed canonical names 2026-04-25 → restoring full line.
/// Closes I-14.
const COPYRIGHT: &str = "© 2026 Anish Trivedi & Kruti Trivedi";

/// Bug M10 (D-window class): canonical Windows process-creation flags
/// for the `mneme mcp stdio` probe spawned by `probe_mcp_tools`. Sets
/// `CREATE_NO_WINDOW` (`0x08000000`) so no console window flashes
/// when `mneme doctor` runs from a hook context (or as part of
/// `mneme audit --self-check`). The constant is exposed
/// unconditionally so pure-Rust unit tests can pin the contract on
/// every host platform — the `cmd.creation_flags(...)` call site is
/// `#[cfg(windows)]` only.
pub(crate) fn windows_doctor_mcp_probe_flags() -> u32 {
    /// CREATE_NO_WINDOW from `windows-sys`: suppresses console window
    /// allocation for the child process. Canonical Win32 doc:
    /// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    CREATE_NO_WINDOW
}

/// Inside-width of the banner box (chars between the two `║`).
const BANNER_WIDTH: usize = 62;

/// JSON-RPC `clientInfo.name` we identify as when probing the MCP
/// server during `mneme doctor`. Intentionally fixed and distinct from
/// real clients (Claude Code, Cursor, …) so server-side telemetry can
/// recognise that the request came from the doctor and short-circuit
/// any expensive lazy initialisation. Closes NEW-027.
pub const MCP_CLIENT_NAME: &str = "mneme-doctor";

/// CLI args for `mneme doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Skip the live IPC probe (in-process diagnostics only).
    #[arg(long)]
    pub offline: bool,

    /// Dump the raw supervisor status JSON (default is the friendly
    /// summary only).
    #[arg(long)]
    pub json: bool,

    /// Skip the per-MCP-tool health probe (spawns a fresh
    /// `mneme mcp stdio` child to enumerate the live tool set). The
    /// probe is usually <2s on POS2 but can be skipped for a faster
    /// run in CI / automated scripts.
    #[arg(long)]
    pub skip_mcp_probe: bool,

    /// G11: pre-flight verification mode. Runs the full toolchain
    /// probe (Rust / Bun / Node / Tauri CLI / Git / Python / SQLite /
    /// Java / Tesseract / ImageMagick), verifies every binary in
    /// `~/.mneme/bin/` launches cleanly with `--version`, and exits
    /// non-zero if any HIGH-severity tool is missing. Use after a
    /// fresh install to confirm the environment is fully wired up.
    #[arg(long)]
    pub strict: bool,
}

/// One row in a doctor section. `value` is rendered after a `:` and
/// padded to the box width.
#[derive(Debug, Clone)]
pub struct DoctorRow {
    pub label: String,
    pub value: String,
}

impl DoctorRow {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Entry point used by `main.rs`.
pub async fn run(args: DoctorArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    // G11: --strict short-circuits the regular run with a focused
    // pre-flight verifier (toolchain probes + binary self-test). Exits
    // non-zero if any HIGH-severity tool is missing.
    if args.strict {
        let code = run_strict();
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }

    print_banner();
    println!();
    println!("  {:<16}{}", "timestamp:", utc_now_readable());
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ mneme doctor · health check                             │");
    println!("├─────────────────────────────────────────────────────────┤");

    let runtime = crate::runtime_dir();
    let state = crate::state_dir();
    line("runtime dir", &runtime.display().to_string());
    line("state   dir", &state.display().to_string());
    let rt_ok = is_writable(&runtime);
    let st_ok = is_writable(&state);
    line("runtime writable", if rt_ok { "yes ✓" } else { "NO ✗" });
    line("state   writable", if st_ok { "yes ✓" } else { "NO ✗" });

    if args.offline {
        println!("└─────────────────────────────────────────────────────────┘");
        print_build_toolchain_section();
        // K1: hooks-registered check is filesystem-only — works offline.
        render_hooks_registered_box();
        // Bug C: filesystem-only too — works offline.
        render_models_box();
        return Ok(());
    }

    let client = make_client(socket_override);
    let is_up = client.is_running().await;
    line(
        "supervisor",
        if is_up { "running ✓" } else { "NOT RUNNING ✗" },
    );
    // Per-tool path indicator: which source `recall`/`blast`/`godnodes`
    // will hit right now. Added in v0.3.1 alongside supervisor IPC for
    // those three commands — an up daemon serves them from its pooled
    // read connections; otherwise the CLI falls back to a direct
    // `graph.db` read. Both are correct; this row just tells operators
    // which one they're getting.
    line(
        "query path",
        if is_up {
            "supervisor ✓"
        } else {
            "direct-db (supervisor down)"
        },
    );
    if !is_up {
        println!("└─────────────────────────────────────────────────────────┘");
        print_build_toolchain_section();
        println!();
        // Even without the supervisor, the MCP bridge + per-tool probe
        // are useful — the Bun MCP server spawns independently of the
        // Rust supervisor for `tools/list`.
        render_mcp_bridge_box();
        // K1: settings.json check — independent of supervisor.
        render_hooks_registered_box();
        // Bug C: filesystem-only — works without the daemon.
        render_models_box();
        if !args.skip_mcp_probe {
            render_mcp_tool_probe_box();
        }
        println!();
        println!("start the daemon with:  mneme daemon start");
        return Ok(());
    }

    // Request per-child snapshot for the summary.
    let resp = client.request(IpcRequest::Status { project: None }).await?;
    if let IpcResponse::Status { ref children } = resp {
        let total = children.len();
        let mut running = 0usize;
        let mut pending = 0usize;
        let mut failed = 0usize;
        let mut restarts = 0u64;
        for child in children {
            let status = child
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            match status {
                "running" | "healthy" => running += 1,
                "pending" | "starting" => pending += 1,
                "failed" | "crashed" => failed += 1,
                _ => {}
            }
            if let Some(r) = child.get("restart_count").and_then(|v| v.as_u64()) {
                restarts += r;
            }
        }
        line(
            "workers",
            &format!("{total} total  ({running} up, {pending} pending, {failed} failed)"),
        );
        line("restarts", &restarts.to_string());
        println!("└─────────────────────────────────────────────────────────┘");
        println!();

        // Per-worker breakdown — one row per worker with status + pid +
        // uptime. Humans can tell which worker is failing at a glance.
        println!("┌─────────────────────────────────────────────────────────┐");
        println!("│ per-worker health                                       │");
        println!("├─────────────────────────────────────────────────────────┤");
        for child in children {
            let name = child
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = child
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let pid = child
                .get("pid")
                .and_then(|v| v.as_u64())
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            let uptime_ms = child
                .get("current_uptime_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let restarts = child
                .get("restart_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            // Bug L: surface dropped-restart count next to restarts so
            // a non-zero value is visible in `mneme doctor` output.
            let dropped = child
                .get("restart_dropped_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mark = match status {
                "running" | "healthy" => "✓",
                "pending" | "starting" => "…",
                "failed" | "crashed" => "✗",
                _ => "?",
            };
            let uptime_str = if uptime_ms > 0 {
                format!("{}s", uptime_ms / 1000)
            } else {
                "-".to_string()
            };
            line(
                &format!("{mark} {name}"),
                &format!(
                    "status={status:<9}  pid={pid:<6}  uptime={uptime_str:<6}  restarts={restarts}  dropped={dropped}"
                ),
            );
        }
        println!("└─────────────────────────────────────────────────────────┘");

        // Per-binary health — does every expected mneme-* binary live
        // on disk next to `mneme(.exe)`? Linux / macOS builds ship
        // without the `.exe` suffix; `expected_binary_names()` picks
        // the right platform matrix.
        println!();
        println!("┌─────────────────────────────────────────────────────────┐");
        println!("│ binaries on disk                                        │");
        println!("├─────────────────────────────────────────────────────────┤");
        let bin_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));
        if let Some(dir) = bin_dir {
            for b in expected_binary_names() {
                let p = dir.join(b);
                let ok = p.exists();
                let size = if ok {
                    std::fs::metadata(&p)
                        .map(|m| format!("{:.1} MB", m.len() as f64 / 1_048_576.0))
                        .unwrap_or_else(|_| "?".to_string())
                } else {
                    "MISSING".to_string()
                };
                let mark = if ok { "✓" } else { "✗" };
                line(&format!("{mark} {b}"), &size);
            }
        }
        println!("└─────────────────────────────────────────────────────────┘");

        // MCP bridge health — does `~/.mneme/mcp/src/index.ts` exist?
        // Is `bun` on PATH?
        render_mcp_bridge_box();

        // K1: hooks-registered check — does mneme have its 8 entries
        // in `~/.claude/settings.json`? Reports green/red. Always emit
        // so users see the persistent-memory-layer status front and
        // centre.
        render_hooks_registered_box();

        // Bug C: local models inventory — surfaces the BGE pair plus
        // bundled GGUFs per kind, so users see the full bundle, not
        // just BGE.
        render_models_box();

        // Per-MCP-tool probe — spawn a fresh `mneme mcp stdio` child,
        // run the JSON-RPC handshake, and list every tool the server
        // actually exposes. Gated behind --skip-mcp-probe so CI / very
        // slow disks can opt out.
        if !args.skip_mcp_probe {
            render_mcp_tool_probe_box();
        }

        print_build_toolchain_section();
        if args.json {
            println!();
            println!("raw status:");
            println!("{}", serde_json::to_string_pretty(&children)?);
        }
    } else {
        println!("└─────────────────────────────────────────────────────────┘");
        print_build_toolchain_section();
        warn!(?resp, "supervisor returned non-status response");
    }
    Ok(())
}

/// Render the "MCP bridge" box (entry path + bun runtime). Split out
/// so we can emit it on both the supervisor-up and supervisor-down
/// paths without duplicating the box-drawing.
fn render_mcp_bridge_box() {
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ MCP bridge                                              │");
    println!("├─────────────────────────────────────────────────────────┤");
    let mcp_entry = mcp_entry_path();
    let mcp_exists = mcp_entry
        .as_ref()
        .map(|p| p.exists())
        .unwrap_or(false);
    line(
        if mcp_exists { "✓ MCP entry" } else { "✗ MCP entry" },
        mcp_entry
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".into())
            .as_str(),
    );
    let bun_on_path = which_on_path("bun");
    let bun_str = bun_on_path
        .as_ref()
        .map(|p| p.display().to_string());
    line(
        if bun_on_path.is_some() {
            "✓ bun runtime"
        } else {
            "✗ bun runtime"
        },
        bun_str.as_deref().unwrap_or("not on PATH"),
    );
    println!("└─────────────────────────────────────────────────────────┘");
}

/// Render the right-hand "value" string for the `hooks_registered`
/// row in the doctor's "Claude Code hooks" box.
///
/// **M2 (audit DEEP-AUDIT-2026-04-29.md §M2):** After the K1 fix in
/// v0.3.2, `mneme install` defaults hooks ON and the legacy
/// `--enable-hooks` flag is a deprecated no-op
/// (`cli/src/commands/install.rs:91` + `CHANGELOG.md:91`). The
/// remediation copy therefore says `re-run \`mneme install\`` — not
/// the deprecated `--enable-hooks` form, which would silently do
/// nothing and trap the user in a loop. `--force` is retained for the
/// partial-registration case so users can rewrite a hand-edited
/// settings file.
///
/// Pure (no I/O) so it can be unit-tested without setting up a fake
/// `~/.claude/settings.json` tree.
fn hooks_remediation_message(count: usize, expected: usize) -> String {
    if count == expected {
        format!("{count}/{expected} entries registered")
    } else if count == 0 {
        format!("{count}/{expected} — re-run `mneme install` to register")
    } else {
        format!(
            "{count}/{expected} — partial registration; re-run `mneme install --force`"
        )
    }
}

/// B-AGENT-C-2 (v0.3.2): compose the full doctor message for the
/// hooks row, taking Claude Code's running-state into account.
///
/// Anish's reproduction:
///   1. `mneme install` runs successfully, writing 8 hooks into
///      `~/.claude/settings.json::hooks` with the `_mneme.managed=true`
///      marker.
///   2. Claude Code is still open — it has its own in-memory copy of
///      settings.json that pre-dates the install and lacks mneme's
///      hooks.
///   3. Claude later auto-saves settings.json (UI interaction, slash
///      command, tab focus, exit) — overwriting mneme's entries with
///      its own stale view.
///   4. `mneme doctor` reads the now-stripped file and reports `0/8`.
///   5. User panics, closes Claude, re-runs install, hooks return.
///
/// We can't prevent step 3 — Claude owns the file — but doctor can
/// stop blaming the install and tell the truth. Four message branches
/// per the truth table below; `parse_error` is an orthogonal overlay
/// surfacing the Layer-1 fix (errors are no longer silent zeroes).
///
/// |     count == expected | claude_running   | message
/// |-----------------------|------------------|----------------------------
/// | yes                   | None             | "8/8 entries registered"
/// | yes                   | Some(pid)        | "+ note: restart Claude"
/// | no, count == 0        | None             | "0/N — re-run `mneme install`"
/// | no, count == 0        | Some(pid)        | "0/N + claude is running"
/// | no, partial           | None             | "M/N — partial; re-run --force"
/// | no, partial           | Some(pid)        | "M/N + claude is running"
///
/// Pure (no I/O); Claude state and parse-error are passed in by the
/// caller so this can be unit-tested deterministically.
pub(crate) fn compose_hooks_message(
    count: usize,
    expected: usize,
    claude_pid: Option<u32>,
    parse_error: Option<String>,
) -> String {
    // Layer 1 overlay: a concrete read / parse error trumps every other
    // signal — surface it so the user knows the file is broken, not
    // empty.
    if let Some(err) = parse_error {
        if let Some(pid) = claude_pid {
            return format!(
                "{count}/{expected} — could not read settings.json cleanly ({err}). \
                 Claude Code is RUNNING (PID {pid}); it may be holding the file. \
                 Close Claude entirely and re-run `mneme doctor` to verify."
            );
        }
        return format!(
            "{count}/{expected} — could not read settings.json cleanly ({err}). \
             Open the file and check its JSON, then re-run `mneme install`."
        );
    }

    // Truth table on (count == expected, claude_pid).
    let all_present = count == expected;
    let none_present = count == 0;

    match (all_present, claude_pid) {
        // 8/8 + Claude not running — the existing happy-path line.
        (true, None) => hooks_remediation_message(count, expected),

        // 8/8 + Claude running — install worked; remind the user that
        // already-open sessions won't pick up the new hooks until
        // Claude is restarted.
        (true, Some(pid)) => format!(
            "{count}/{expected} entries registered. Note: Claude Code is running \
             (PID {pid}); new hooks won't fire in already-open sessions — \
             restart Claude to pick them up."
        ),

        // 0/N + Claude running — THE bug Anish hit. Claude likely
        // overwrote the file with its in-memory copy. Don't blame the
        // install.
        (false, Some(pid)) if none_present => format!(
            "{count}/{expected} detected, but Claude Code is RUNNING (PID {pid}). \
             Claude may be holding settings.json with an in-memory copy that does \
             not include mneme hooks. Close Claude entirely and re-run \
             `mneme doctor` to verify. If still missing, run `mneme install` \
             to re-register."
        ),

        // 0/N + Claude closed — true negative. Install genuinely didn't
        // take. Existing copy is correct here.
        (false, None) if none_present => hooks_remediation_message(count, expected),

        // Partial + Claude running — Claude probably stripped some,
        // not all. Tell the user to close + reinstall.
        (false, Some(pid)) => format!(
            "{count}/{expected} — partial registration; Claude Code is RUNNING \
             (PID {pid}) and may have rewritten settings.json. Close Claude \
             entirely and re-run `mneme install --force`."
        ),

        // Partial + Claude closed — hand-edited. Existing partial copy
        // is correct.
        (false, None) => hooks_remediation_message(count, expected),
    }
}

/// B-AGENT-C-2 (v0.3.2): is Claude Code currently running on this host?
///
/// Returns the PID of the first matching process, or None if no Claude
/// process is found. Cross-platform via the workspace `sysinfo` dep
/// already used by `cli/src/commands/abort.rs` and `cli/src/build_lock.rs`.
///
/// Recognition heuristics (any one match):
///   - process name (case-insensitive) == "claude.exe" / "claude"
///   - process command line contains "claude-code" (covers
///     `node claude-code` invocations on systems where the CLI is
///     run as a node script)
///
/// Best-effort: a sysinfo refresh failure returns None — the doctor's
/// hook-state diagnosis still works, just without the Claude-running
/// overlay. Never panics.
pub(crate) fn is_claude_code_running() -> Option<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    // Refresh only the process surface — RAM / CPU / disk are not
    // needed and `refresh_processes_specifics` with `Everything` is
    // overkill. `ProcessRefreshKind::new()` is the cheapest variant
    // that still populates `name()` and `cmd()`.
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new(),
    );
    for (pid, proc_) in sys.processes() {
        let name = proc_.name().to_string_lossy().to_lowercase();
        // Direct executable name match. We accept both `claude.exe`
        // (Windows) and `claude` (POSIX). We deliberately do NOT match
        // bare "node" here — only when its cmdline contains
        // "claude-code" (see below).
        if name == "claude.exe" || name == "claude" {
            return Some(pid.as_u32());
        }
        // Command-line match for `node claude-code` style invocations.
        // sysinfo's `cmd()` returns the argv vector; we lowercase and
        // join so we don't have to care about which slot the
        // claude-code script lives in.
        let cmd_joined: String = proc_
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        if cmd_joined.contains("claude-code") || cmd_joined.contains("claude_code") {
            return Some(pid.as_u32());
        }
    }
    None
}

/// K1 / Phase A §K1: render the "Claude Code hooks" box.
/// Reports whether mneme's 8 hook entries are registered in
/// `~/.claude/settings.json`. Green when all 8 are present, red
/// otherwise. Split out so we can emit it on both supervisor-up and
/// supervisor-down paths.
///
/// **B-AGENT-C-2 (v0.3.2):** the simple "hooks present? yes/no" line
/// turned out to be wrong when Claude Code was running. Claude owns
/// `~/.claude/settings.json` while it is open; if the user installed
/// mneme with Claude already running, Claude's next auto-save can
/// strip mneme's entries. Doctor now:
///
///   1. uses `count_registered_mneme_hooks_detailed` so a real read /
///      parse error surfaces instead of silently degrading to `0/8`
///      (Layer 1 fix);
///   2. probes the live process table for Claude Code via
///      `is_claude_code_running()` so the message can call out the
///      likely cause when hooks appear missing while Claude is up
///      (Layer 2 fix).
fn render_hooks_registered_box() {
    use crate::platforms::claude_code::{
        count_registered_mneme_hooks_detailed, HookFileReadState, HOOK_SPECS,
    };

    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ Claude Code hooks (~/.claude/settings.json)             │");
    println!("├─────────────────────────────────────────────────────────┤");

    let settings_path = dirs::home_dir().map(|h| h.join(".claude").join("settings.json"));
    let path_str = settings_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "?".into());

    let claude_pid = is_claude_code_running();

    match settings_path.as_ref() {
        Some(p) => {
            let r = count_registered_mneme_hooks_detailed(p);
            match &r.read_state {
                HookFileReadState::Missing => {
                    // True negative — file does not exist.
                    let value = compose_hooks_message(0, r.expected, claude_pid, None);
                    line("✗ hooks_registered", &value);
                    line("settings.json", &p.display().to_string());
                    line(
                        "  status",
                        "settings.json does not exist (mneme install has not run)",
                    );
                }
                HookFileReadState::UnreadableIo(io_msg) => {
                    // File present but unreadable — surface concrete reason.
                    let value = compose_hooks_message(
                        0,
                        r.expected,
                        claude_pid,
                        Some(format!("io error: {io_msg}")),
                    );
                    line("✗ hooks_registered", &value);
                    line("settings.json", &path_str);
                }
                HookFileReadState::Read => {
                    let mark = if r.count == r.expected { "✓" } else { "✗" };
                    let value = compose_hooks_message(
                        r.count,
                        r.expected,
                        claude_pid,
                        r.parse_error.clone(),
                    );
                    line(&format!("{mark} hooks_registered"), &value);
                    line("settings.json", &path_str);
                    // Per-event breakdown so users can see which event is
                    // missing without opening the JSON. Helps debug a partial
                    // registration (e.g. user hand-edited Stop out of the file).
                    // Only render when parse succeeded — otherwise the body is
                    // not trustworthy.
                    if r.count != r.expected && r.parse_error.is_none() {
                        let body = std::fs::read_to_string(p).unwrap_or_default();
                        let parsed: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
                        let hooks_obj = parsed
                            .get("hooks")
                            .and_then(|v| v.as_object())
                            .cloned()
                            .unwrap_or_default();
                        for spec in HOOK_SPECS {
                            let present = hooks_obj
                                .get(spec.event)
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter().any(|e| {
                                        e.get("_mneme")
                                            .and_then(|m| m.get("managed"))
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false)
                                    })
                                })
                                .unwrap_or(false);
                            let m = if present { "✓" } else { "✗" };
                            line(
                                &format!("  {m} {}", spec.event),
                                if present { "yes" } else { "no" },
                            );
                        }
                    }
                }
            }
        }
        None => {
            line("✗ hooks_registered", "could not resolve home dir");
        }
    }
    println!("└─────────────────────────────────────────────────────────┘");
}

/// Render the "local models" box. Bug C — surface every registered
/// model file (BGE ONNX, BGE tokenizer, GGUFs) per kind so the user
/// sees the full bundle inventory at a glance, not just BGE. Reads
/// `~/.mneme/models/manifest.json`. Empty manifest renders a single
/// "no models registered" line + the install hint.
fn render_models_box() {
    use crate::commands::models::{public_model_root, read_manifest_or_empty, ModelKind};

    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ local models (~/.mneme/models)                          │");
    println!("├─────────────────────────────────────────────────────────┤");

    let root = public_model_root();
    line("model root", &root.display().to_string());
    let manifest = read_manifest_or_empty(&root);
    if manifest.entries.is_empty() {
        line(
            "✗ models",
            "0 registered — run `mneme models install --from-path <bundle/models>`",
        );
        println!("└─────────────────────────────────────────────────────────┘");
        return;
    }

    // Group counts per kind so the summary row makes sense at a glance.
    let mut embedding_models = 0usize;
    let mut tokenizers = 0usize;
    let mut llms = 0usize;
    let mut embedding_llms = 0usize;
    let mut total_bytes: u64 = 0;
    for entry in &manifest.entries {
        total_bytes = total_bytes.saturating_add(entry.size);
        match entry.kind {
            ModelKind::EmbeddingModel => embedding_models += 1,
            ModelKind::EmbeddingTokenizer => tokenizers += 1,
            ModelKind::Llm => llms += 1,
            ModelKind::EmbeddingLlm => embedding_llms += 1,
        }
    }
    let total_mb = total_bytes / 1_048_576;
    line(
        "✓ registered",
        &format!(
            "{} files · {} MB  ({} embedding, {} tokenizer, {} llm, {} embed-llm)",
            manifest.entries.len(),
            total_mb,
            embedding_models,
            tokenizers,
            llms,
            embedding_llms,
        ),
    );

    // Per-file rows for full inventory visibility.
    for entry in &manifest.entries {
        let mb = entry.size / 1_048_576;
        line(
            &format!("  · {}", entry.name),
            &format!("{:<19}  {} MB", entry.kind.label(), mb),
        );
    }
    println!("└─────────────────────────────────────────────────────────┘");
}

/// Render the "per-MCP-tool health" box — spawn a fresh mneme child,
/// enumerate tools via JSON-RPC, show one ✓ per live tool. Split out
/// so we can emit it on both the supervisor-up and supervisor-down
/// paths.
fn render_mcp_tool_probe_box() {
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ per-MCP-tool health                                     │");
    println!("├─────────────────────────────────────────────────────────┤");
    match probe_mcp_tools(Duration::from_secs(10)) {
        Ok(tools) => {
            for t in &tools {
                line(&format!("✓ {t}"), "live");
            }
            let count = tools.len();
            let summary = if count >= 40 {
                format!("{count} tools exposed (expected >= 40) ✓")
            } else {
                format!("{count} tools exposed (expected >= 40) ✗")
            };
            line("total", &summary);
        }
        Err(reason) => {
            line("✗ probe", &format!("could not probe MCP server — {reason}"));
        }
    }
    println!("└─────────────────────────────────────────────────────────┘");
}

/// Return the list of mneme binary filenames the doctor expects to
/// find on disk next to `mneme(.exe)`. On Windows the names carry a
/// `.exe` suffix; Linux + macOS binaries have no extension. Order is
/// stable so the dashboard rows do not reshuffle between runs.
pub fn expected_binary_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &[
            "mneme.exe",
            "mneme-daemon.exe",
            "mneme-brain.exe",
            "mneme-parsers.exe",
            "mneme-scanners.exe",
            "mneme-livebus.exe",
            "mneme-md-ingest.exe",
            "mneme-store.exe",
            "mneme-multimodal.exe",
        ]
    }
    #[cfg(not(windows))]
    {
        &[
            "mneme",
            "mneme-daemon",
            "mneme-brain",
            "mneme-parsers",
            "mneme-scanners",
            "mneme-livebus",
            "mneme-md-ingest",
            "mneme-store",
            "mneme-multimodal",
        ]
    }
}

/// Path to the MCP entry `index.ts` inside the user's mneme install
/// (`<PathManager::default_root()>/mcp/src/index.ts`).
///
/// Routes through `PathManager::default_root()` so `MNEME_HOME`
/// is honored (HOME-bypass-doctor:560 fix).
/// `PathManager::default_root()` itself falls back through:
///   1. `MNEME_HOME` env override.
///   2. `dirs::home_dir().join(".mneme")` (historical default).
///   3. OS default (`%PROGRAMDATA%\mneme` / `/var/lib/mneme`).
///
/// Returns `Option` for source compatibility with prior callers; the
/// new resolver is infallible in practice — every supported OS yields
/// at least one of the three fallbacks above.
pub fn mcp_entry_path() -> Option<std::path::PathBuf> {
    Some(
        common::paths::PathManager::default_root()
            .root()
            .join("mcp")
            .join("src")
            .join("index.ts"),
    )
}

/// Print the boxed banner. Version line + copyright line use dynamic
/// padding so longer pre-release versions (e.g. "0.4.0-rc.5+build.7")
/// don't overflow the right border. Closes NEW-026 + I-14.
fn print_banner() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                                                              ║");
    println!("║   ███╗   ███╗███╗   ██╗███████╗███╗   ███╗███████╗           ║");
    println!("║   ████╗ ████║████╗  ██║██╔════╝████╗ ████║██╔════╝           ║");
    println!("║   ██╔████╔██║██╔██╗ ██║█████╗  ██╔████╔██║█████╗             ║");
    println!("║   ██║╚██╔╝██║██║╚██╗██║██╔══╝  ██║╚██╔╝██║██╔══╝             ║");
    println!("║   ██║ ╚═╝ ██║██║ ╚████║███████╗██║ ╚═╝ ██║███████╗           ║");
    println!("║   ╚═╝     ╚═╝╚═╝  ╚═══╝╚══════╝╚═╝     ╚═╝╚══════╝           ║");
    println!("║                                                              ║");
    println!(
        "║   persistent memory · code graph · drift detector · 47 tools ║"
    );
    print_banner_line(&format!(
        "   v{} · 100% local · Apache-2.0",
        env!("CARGO_PKG_VERSION")
    ));
    println!("║                                                              ║");
    print_banner_line(&format!("   {COPYRIGHT}"));
    println!("║                                                              ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
}

/// Render one inside-the-box line, padding (or truncating) to the
/// banner width so the right border always lands in the same column.
fn print_banner_line(content: &str) {
    let visible = content.chars().count();
    if visible >= BANNER_WIDTH {
        // Truncate to width-1 and append a marker so the box still
        // closes cleanly. Better than overflowing the right border.
        let mut out = String::new();
        for (i, ch) in content.chars().enumerate() {
            if i + 1 >= BANNER_WIDTH {
                break;
            }
            out.push(ch);
        }
        out.push('…');
        println!("║{out}║");
    } else {
        let pad = " ".repeat(BANNER_WIDTH - visible);
        println!("║{content}{pad}║");
    }
}


fn line(label: &str, value: &str) {
    let padded_label = format!("{label:<17}");
    let content = format!("│ {padded_label}: {value}");
    // Pad to width 59 (inside borders), then add right border.
    let visible_len = content.chars().count();
    let target = 59;
    let pad = if visible_len < target {
        " ".repeat(target - visible_len)
    } else {
        String::new()
    };
    println!("{content}{pad}│");
}

/// Spawn a fresh `mneme mcp stdio` child, drive the MCP JSON-RPC
/// handshake, and return the list of tool names the server publishes
/// via `tools/list`.
///
/// Fails fast (and cleanly — never hangs the main doctor command) if:
///   - the current exe path cannot be resolved
///   - spawning the child fails
///   - stdin/stdout pipes can't be captured
///   - the child doesn't respond within `deadline`
///   - the `tools/list` response is malformed
///
/// Always kills the child before returning so no zombie Bun processes
/// linger.
fn probe_mcp_tools(deadline: Duration) -> Result<Vec<String>, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe unavailable: {e}"))?;

    let mut cmd = StdCommand::new(&exe);
    cmd.arg("mcp")
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Silence the MCP server's own stderr banner / diagnostic
        // logs so nothing contaminates the probe; stderr is piped to
        // null anyway, but belt-and-braces for any SDK that reads env.
        .env("MNEME_LOG", "error")
        .env("NO_COLOR", "1");
    // Bug M10 (D-window class): suppress console-window allocation
    // when this probe runs from a windowless parent (hooks,
    // `mneme audit --self-check`). See `windows_doctor_mcp_probe_flags`.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_doctor_mcp_probe_flags());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    let start = Instant::now();

    // Take ownership of stdin/stdout handles. If either is missing the
    // child is unusable — kill it and bail.
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("no stdin pipe".into());
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("no stdout pipe".into());
        }
    };

    // Run the actual JSON-RPC handshake on a worker thread so we can
    // enforce the deadline from this thread without blocking forever
    // on a stuck `read_line`.
    let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<String>, String>>();

    // Handshake thread — owns stdout reader, writes to stdin via the
    // handle it captures, posts result to the channel.
    std::thread::spawn(move || {
        let res = handshake_and_list(&mut stdin, stdout);
        let _ = tx.send(res);
    });

    // Wait for the worker to finish, bounded by `deadline`.
    let remaining = deadline.saturating_sub(start.elapsed());
    let result = match rx.recv_timeout(remaining) {
        Ok(res) => res,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(format!("timed out after {}s", deadline.as_secs()))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("handshake thread died".into())
        }
    };

    // Always kill the child and reap it before returning.
    let _ = child.kill();
    let _ = child.wait();

    result
}

/// Drive the MCP JSON-RPC handshake: initialize → initialized →
/// tools/list. Returns the tool names in the order the server
/// returned them.
fn handshake_and_list(
    stdin: &mut std::process::ChildStdin,
    stdout: std::process::ChildStdout,
) -> Result<Vec<String>, String> {
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": MCP_CLIENT_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    });
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    });
    let tools_list = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {},
    });

    // Write initialize, then wait for its response.
    write_frame(stdin, &initialize)?;

    let mut reader = BufReader::new(stdout);

    // Consume the initialize response (match id == 1). The server may
    // interleave log lines on stdout in some transports, but the MCP
    // SDK uses pure JSON-RPC framing on stdio — one JSON object per
    // line — so we just read until we see id == 1.
    let _init_resp = read_response_with_id(&mut reader, 1)?;

    // Tell the server initialization is complete.
    write_frame(stdin, &initialized)?;

    // Ask for the tool list.
    write_frame(stdin, &tools_list)?;

    // Read until we find the response with id == 2.
    let resp = read_response_with_id(&mut reader, 2)?;

    let tools = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .ok_or_else(|| "tools/list response missing result.tools[]".to_string())?;

    let mut names = Vec::with_capacity(tools.len());
    for t in tools {
        if let Some(n) = t.get("name").and_then(|v| v.as_str()) {
            names.push(n.to_string());
        }
    }
    Ok(names)
}

/// Write one JSON-RPC frame (`{json}\n`) to the child's stdin.
fn write_frame<W: Write>(w: &mut W, value: &serde_json::Value) -> Result<(), String> {
    let s = serde_json::to_string(value)
        .map_err(|e| format!("encode failed: {e}"))?;
    w.write_all(s.as_bytes())
        .map_err(|e| format!("stdin write failed: {e}"))?;
    w.write_all(b"\n")
        .map_err(|e| format!("stdin newline failed: {e}"))?;
    w.flush()
        .map_err(|e| format!("stdin flush failed: {e}"))?;
    Ok(())
}

/// Read lines from the child's stdout until we find a JSON object
/// with `id == want_id`. Intermediate lines (other responses,
/// notifications, blank lines) are skipped.
fn read_response_with_id<R: BufRead>(
    reader: &mut R,
    want_id: u64,
) -> Result<serde_json::Value, String> {
    loop {
        let mut buf = String::new();
        let n = reader
            .read_line(&mut buf)
            .map_err(|e| format!("stdout read failed: {e}"))?;
        if n == 0 {
            return Err("child closed stdout before response arrived".into());
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // Skip non-JSON lines defensively.
        };
        if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
            if id == want_id {
                return Ok(value);
            }
        }
    }
}

fn is_writable(path: &std::path::Path) -> bool {
    std::fs::create_dir_all(path)
        .and_then(|_| {
            let probe = path.join(".mneme-probe");
            std::fs::write(&probe, b"")?;
            std::fs::remove_file(&probe)
        })
        .is_ok()
}

/// Search PATH for `name` (with platform-appropriate extensions) and
/// return the first hit, or `None` if not present.
///
/// Pure-stdlib so we don't have to pull `which` as a dep just for the
/// doctor probe.
pub fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .ok()
            .map(|s| {
                s.split(';')
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![".EXE".into(), ".CMD".into(), ".BAT".into(), ".COM".into()])
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = if ext.is_empty() {
                dir.join(name)
            } else {
                // Skip if the name already has an extension and we're on Windows.
                if cfg!(windows) && Path::new(name).extension().is_some() {
                    dir.join(name)
                } else {
                    dir.join(format!("{name}{ext}"))
                }
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Suggested install hint shown when the doctor cannot find any MSVC
/// compiler / linker on this machine. Used by both the live probe and
/// the unit-test that pins the message text. Closes I-16.
const MSVC_INSTALL_HINT: &str =
    "MSVC Build Tools missing — install via `winget install Microsoft.VisualStudio.2022.BuildTools` or VS Installer";

// ============================================================================
// G1-G10: developer toolchain probes. Closes Phase A §G.
// ============================================================================
//
// `mneme install` (and the install.ps1 / install.sh shell scripts) only lay
// down the mneme runtime + MCP wiring. The dev-toolchain (rust, bun, node,
// tauri-cli, git, python, sqlite, java, tesseract, imagemagick) is detected
// here and surfaced via `mneme doctor --strict` and the `mneme install`
// capability summary so users know exactly what's missing and how to fix
// it. We deliberately do NOT auto-install: Windows + auto-install is too
// fragile and easily kicks off elevation prompts the user wasn't expecting.
// Detect, report, advise. Keep the user in control.

/// Severity tier for a missing toolchain entry.
///
/// `--strict` returns non-zero when ANY High-severity tool is missing.
/// Medium / Low surface as warnings only — install proceeds, capability
/// is reduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSeverity {
    High,
    Medium,
    Low,
}

impl ToolSeverity {
    pub const fn label(self) -> &'static str {
        match self {
            ToolSeverity::High => "HIGH",
            ToolSeverity::Medium => "MED ",
            ToolSeverity::Low => "LOW ",
        }
    }
}

/// Canonical entry for one developer-toolchain dependency. The single
/// source of truth shared by `mneme doctor --strict` and the capability
/// summary printed at the end of `mneme install`. The PowerShell + sh
/// install scripts probe the same set of tool names; see
/// `scripts/install.ps1` and `scripts/install.sh` G-section comments.
#[derive(Debug, Clone, Copy)]
pub struct ToolchainEntry {
    /// Display name shown in the doctor row + capability summary.
    pub display: &'static str,
    /// Comma-separated list of binary names to probe on PATH (in order;
    /// the first hit wins). Most tools have one name; Python is split
    /// across `python` and `python3` so we list both.
    pub probes: &'static [&'static str],
    /// Optional secondary probe via `cargo <subcommand> --version` (for
    /// Tauri CLI, which is normally installed as a cargo subcommand).
    /// `None` for tools probed by binary alone.
    pub cargo_subcommand: Option<&'static str>,
    /// Severity tier — High blocks `--strict` exit, Medium / Low warn.
    pub severity: ToolSeverity,
    /// Phase A issue id (G1, G2, …) so doctor output is traceable back
    /// to phase-a-issues.md.
    pub issue_id: &'static str,
    /// One-line description of what mneme uses this tool for.
    pub purpose: &'static str,
    /// Install hint for Windows (winget / official installer one-liner).
    pub hint_windows: &'static str,
    /// Install hint for macOS / Linux. Single string covers both — the
    /// scripts/install.sh script already breaks this down per package
    /// manager (brew / apt / dnf / pacman / apk).
    pub hint_unix: &'static str,
}

/// Canonical list of every dev-toolchain dependency mneme cares about.
/// Order = display order in `mneme doctor --strict` and the install
/// capability summary. Closes G1-G10 from `phase-a-issues.md` §G.
///
/// IMPORTANT: this list is the single source of truth. Both
/// `scripts/install.ps1` and `scripts/install.sh` mirror these entries
/// in their probe blocks — keep all three in sync if you add / remove a
/// tool here.
pub const KNOWN_TOOLCHAIN: &[ToolchainEntry] = &[
    ToolchainEntry {
        display: "Rust toolchain (rustc + cargo)",
        probes: &["rustc", "cargo"],
        cargo_subcommand: None,
        severity: ToolSeverity::High,
        issue_id: "G1",
        purpose: "vision/tauri/ build, future Rust-port work, workspace builds",
        hint_windows: "winget install Rustlang.Rustup",
        hint_unix: "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh",
    },
    ToolchainEntry {
        display: "Bun",
        probes: &["bun"],
        cargo_subcommand: None,
        severity: ToolSeverity::High,
        issue_id: "G2",
        purpose: "vision app runtime + MCP server (mneme mcp stdio)",
        hint_windows: "irm bun.sh/install.ps1 | iex",
        hint_unix: "curl -fsSL https://bun.sh/install | bash",
    },
    ToolchainEntry {
        display: "Node.js",
        probes: &["node"],
        cargo_subcommand: None,
        severity: ToolSeverity::High,
        issue_id: "G3",
        purpose: "Claude Code CLI install, JS-tooling fallbacks, npm-based installers",
        hint_windows: "winget install OpenJS.NodeJS.LTS",
        hint_unix: "use nvm: curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.0/install.sh | bash",
    },
    ToolchainEntry {
        display: "Tauri CLI",
        probes: &["tauri"],
        cargo_subcommand: Some("tauri"),
        severity: ToolSeverity::Medium,
        issue_id: "G4",
        purpose: "ergonomic Tauri builds (tauri build, tauri dev) for vision/",
        hint_windows: "cargo install tauri-cli --version \"^2.0\"",
        hint_unix: "cargo install tauri-cli --version \"^2.0\"",
    },
    ToolchainEntry {
        display: "Git",
        probes: &["git"],
        cargo_subcommand: None,
        severity: ToolSeverity::High,
        issue_id: "G5",
        purpose: "git.db shard population (commits / blame / history), Why-Chain trace",
        hint_windows: "winget install Git.Git",
        hint_unix: "brew install git (macOS) | sudo apt install git (Debian/Ubuntu)",
    },
    ToolchainEntry {
        display: "Python",
        probes: &["python", "python3"],
        cargo_subcommand: None,
        severity: ToolSeverity::Medium,
        issue_id: "G6",
        purpose: "PNG->ICO icon conversion (PIL), multimodal sidecar, scanner fallbacks",
        hint_windows: "winget install Python.Python.3.11",
        hint_unix: "brew install python@3.11 (macOS) | sudo apt install python3 (Debian/Ubuntu)",
    },
    ToolchainEntry {
        display: "SQLite CLI",
        probes: &["sqlite3"],
        cargo_subcommand: None,
        severity: ToolSeverity::Low,
        issue_id: "G7",
        purpose: "manual shard inspection (sqlite3 graph.db .schema). Drivers are bundled.",
        hint_windows: "winget install SQLite.SQLite",
        hint_unix: "brew install sqlite (macOS) | sudo apt install sqlite3 (Debian/Ubuntu)",
    },
    ToolchainEntry {
        display: "Java JDK",
        probes: &["java"],
        cargo_subcommand: None,
        severity: ToolSeverity::Low,
        issue_id: "G8",
        purpose: "optional — only if a future feature needs the JVM (currently unused)",
        hint_windows: "winget install EclipseAdoptium.Temurin.21.JDK",
        hint_unix: "brew install openjdk@21 (macOS) | sudo apt install openjdk-21-jdk (Debian/Ubuntu)",
    },
    ToolchainEntry {
        display: "Tesseract OCR",
        probes: &["tesseract"],
        cargo_subcommand: None,
        severity: ToolSeverity::Medium,
        issue_id: "G9",
        purpose: "image OCR via multimodal sidecar (binary feature-gated; rebuild with --features tesseract to enable)",
        hint_windows: "winget install UB-Mannheim.TesseractOCR",
        hint_unix: "brew install tesseract (macOS) | sudo apt install tesseract-ocr (Debian/Ubuntu)",
    },
    ToolchainEntry {
        display: "ImageMagick",
        probes: &["magick"],
        cargo_subcommand: None,
        severity: ToolSeverity::Low,
        issue_id: "G10",
        purpose: "PNG->ICO conversion fallback when Python+PIL unavailable",
        hint_windows: "winget install ImageMagick.ImageMagick",
        hint_unix: "brew install imagemagick (macOS) | sudo apt install imagemagick (Debian/Ubuntu)",
    },
];

/// Outcome of probing one toolchain entry on this host.
#[derive(Debug, Clone)]
pub struct ToolProbe {
    pub entry: ToolchainEntry,
    /// Path of the first matching binary, or None if no probe hit.
    pub found_at: Option<PathBuf>,
    /// `--version` output (first line, trimmed) or None if probe failed.
    pub version: Option<String>,
}

impl ToolProbe {
    pub fn is_present(&self) -> bool {
        self.found_at.is_some()
    }
}

/// Probe one toolchain entry: try each binary in `entry.probes` on
/// PATH, fall back to `cargo <subcommand>` if present. First hit wins.
pub fn probe_tool(entry: &ToolchainEntry) -> ToolProbe {
    // Direct binary probes first.
    for bin in entry.probes {
        if let Some(path) = which_on_path(bin) {
            let version = run_version_probe(&path);
            return ToolProbe {
                entry: *entry,
                found_at: Some(path),
                version,
            };
        }
    }

    // Cargo subcommand fallback (e.g. `cargo tauri --version`).
    if let Some(sub) = entry.cargo_subcommand {
        if let Some(cargo) = which_on_path("cargo") {
            let out = StdCommand::new(&cargo)
                .args([sub, "--version"])
                .output()
                .ok();
            if let Some(o) = out {
                if o.status.success() {
                    let v = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    return ToolProbe {
                        entry: *entry,
                        found_at: Some(cargo),
                        version: if v.is_empty() { None } else { Some(v) },
                    };
                }
            }
        }
    }

    ToolProbe {
        entry: *entry,
        found_at: None,
        version: None,
    }
}

/// Run `<bin> --version` and return the first non-empty line, trimmed.
/// Returns None on any error so the caller can render a "version
/// unknown" row without crashing the doctor.
fn run_version_probe(bin: &Path) -> Option<String> {
    let out = StdCommand::new(bin).arg("--version").output().ok()?;
    if !out.status.success() {
        // Some tools (notably older `java`) print --version on stderr.
        let s = String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        return if s.is_empty() { None } else { Some(s) };
    }
    let s = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if s.is_empty() {
        // Fallback: check stderr (java, some others).
        let s2 = String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if s2.is_empty() {
            None
        } else {
            Some(s2)
        }
    } else {
        Some(s)
    }
}

/// Probe every entry in [`KNOWN_TOOLCHAIN`] and return the results in
/// canonical order.
pub fn probe_all_toolchain() -> Vec<ToolProbe> {
    KNOWN_TOOLCHAIN.iter().map(probe_tool).collect()
}

/// Choose the platform-appropriate install hint for a tool.
pub fn install_hint_for(entry: &ToolchainEntry) -> &'static str {
    if cfg!(windows) {
        entry.hint_windows
    } else {
        entry.hint_unix
    }
}

/// Render the developer-toolchain section. Used by both the regular
/// `mneme doctor` output and `--strict`. Returns `true` if every
/// HIGH-severity tool was found (so `--strict` knows whether to exit
/// non-zero).
pub fn render_toolchain_box(probes: &[ToolProbe]) -> bool {
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ developer toolchain (G1-G10)                            │");
    println!("├─────────────────────────────────────────────────────────┤");
    let mut all_high_present = true;
    for probe in probes {
        let mark = if probe.is_present() { "✓" } else { "✗" };
        let label = format!("{mark} [{}] {}", probe.entry.severity.label(), probe.entry.display);
        let value = match (&probe.found_at, &probe.version) {
            (Some(_), Some(v)) => v.clone(),
            (Some(p), None) => format!("present at {}", p.display()),
            (None, _) => format!("MISSING — {}", probe.entry.issue_id),
        };
        line(&label, &value);
        if probe.entry.severity == ToolSeverity::High && !probe.is_present() {
            all_high_present = false;
        }
    }
    println!("└─────────────────────────────────────────────────────────┘");

    // Per-tool fix hints for everything missing — printed below the box
    // so the table stays readable.
    let missing: Vec<&ToolProbe> = probes.iter().filter(|p| !p.is_present()).collect();
    if !missing.is_empty() {
        println!();
        println!("install hints for missing tools:");
        for probe in missing {
            println!(
                "  [{}] {} ({}): {}",
                probe.entry.severity.label().trim(),
                probe.entry.display,
                probe.entry.issue_id,
                install_hint_for(&probe.entry),
            );
        }
    }

    all_high_present
}

/// G11 strict-mode entry point. Runs all G1-G10 probes, verifies every
/// binary in `~/.mneme/bin/` launches with `--version` cleanly, probes
/// the optional vision app, and returns a non-zero exit code if any
/// HIGH-severity check failed.
///
/// Output format mirrors the regular doctor box-drawing so the strict
/// run still reads like a health check, just with a final PASS / FAIL
/// summary.
pub fn run_strict() -> i32 {
    print_banner();
    println!();
    println!("  {:<16}{}", "timestamp:", utc_now_readable());
    println!("  {:<16}{}", "mode:", "strict (G11 pre-flight verification)");
    println!();

    let mut all_ok = true;

    // G1-G10: developer toolchain.
    let probes = probe_all_toolchain();
    let toolchain_ok = render_toolchain_box(&probes);
    if !toolchain_ok {
        all_ok = false;
    }

    // Binary self-test — every mneme-* binary in the install dir must
    // launch cleanly with `--version`. Catches the partial-extract case
    // (locked binaries on Windows, truncated tar on Unix) that the
    // post-extract check in install.ps1 / install.sh sometimes misses.
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ binary self-test (~/.mneme/bin/* --version)             │");
    println!("├─────────────────────────────────────────────────────────┤");
    let bin_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    let mut binary_failures = 0usize;
    if let Some(dir) = bin_dir {
        for b in expected_binary_names() {
            let p = dir.join(b);
            if !p.exists() {
                line(&format!("✗ {b}"), "MISSING on disk");
                binary_failures += 1;
                continue;
            }
            // Skip the supervisor / worker binaries — they don't expose
            // `--version` (they expect IPC). Only `mneme(.exe)` itself
            // is a CLI; the others get an existence check above.
            let is_cli = b.starts_with("mneme.") || *b == "mneme";
            if !is_cli {
                line(&format!("✓ {b}"), "present (no --version probe — IPC binary)");
                continue;
            }
            match StdCommand::new(&p).arg("--version").output() {
                Ok(out) if out.status.success() => {
                    let v = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    line(&format!("✓ {b}"), &v);
                }
                Ok(out) => {
                    binary_failures += 1;
                    line(
                        &format!("✗ {b}"),
                        &format!("--version exited {}", out.status.code().unwrap_or(-1)),
                    );
                }
                Err(e) => {
                    binary_failures += 1;
                    line(&format!("✗ {b}"), &format!("spawn failed: {e}"));
                }
            }
        }
    } else {
        line("✗ bin dir", "could not resolve current_exe parent");
        binary_failures += 1;
    }
    println!("└─────────────────────────────────────────────────────────┘");
    if binary_failures > 0 {
        all_ok = false;
    }

    // Optional vision app probe — only fails strict if the user passed
    // --with-vision earlier (recorded as a marker file in ~/.mneme/).
    // Today there's no such marker; we just probe and report.
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ optional: vision app (mneme-vision)                     │");
    println!("├─────────────────────────────────────────────────────────┤");
    let vision_bin = if cfg!(windows) { "mneme-vision.exe" } else { "mneme-vision" };
    let vision_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .map(|d| d.join(vision_bin));
    match vision_path.as_ref().filter(|p| p.exists()) {
        Some(p) => line(&format!("✓ {vision_bin}"), &p.display().to_string()),
        None => line(
            &format!("- {vision_bin}"),
            "not installed (vision app is optional — install with `cargo build --release -p mneme-vision`)",
        ),
    }
    println!("└─────────────────────────────────────────────────────────┘");

    // MSVC build toolchain (Windows) — required to build mneme from
    // source. Not a strict-mode failure for binary users (they ran the
    // installer, so they don't need MSVC), but we still surface the
    // probe results so devs hacking on the codebase see them.
    print_build_toolchain_section();

    println!();
    if all_ok {
        println!("strict pre-flight: PASS — all HIGH-severity toolchain present, binaries healthy");
        0
    } else {
        println!("strict pre-flight: FAIL — see install hints above + run individual fix commands");
        1
    }
}

/// Probe the Windows MSVC build toolchain. Closes I-16. Probes:
///   * `link.exe` on PATH
///   * `cl.exe`   on PATH
///   * `vswhere.exe` (PATH or fixed VS Installer path) →
///     `installationPath` filtered to installs that actually ship the
///     VC.Tools.x86.x64 component
///   * Windows SDK (`kernel32.lib` under `Program Files (x86)\Windows
///     Kits\10\Lib\<sdk-version>\um\x64\`)
///
/// PASS = at least one of `link.exe` or `cl.exe` is present *and*
/// either vswhere reports a VC-Tools install or the Windows SDK
/// `kernel32.lib` is on disk. The summary row makes the verdict
/// explicit so users do not have to triangulate four sub-rows. On
/// non-Windows this returns an empty Vec so the box is skipped entirely.
#[cfg(windows)]
pub fn check_build_toolchain() -> Vec<DoctorRow> {
    let mut rows = Vec::new();

    // First, do PATH-only probe for link.exe / cl.exe.
    let mut link_ok = which_on_path("link").is_some();
    let mut cl_ok = which_on_path("cl").is_some();

    // vswhere.exe ships at a fixed location with VS Installer; also
    // sometimes on PATH. Probe both.
    let vswhere_path = which_on_path("vswhere").or_else(|| {
        let pf = std::env::var_os("ProgramFiles(x86)")
            .or_else(|| std::env::var_os("ProgramFiles"))?;
        let candidate = std::path::PathBuf::from(pf)
            .join("Microsoft Visual Studio")
            .join("Installer")
            .join("vswhere.exe");
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    });

    let mut vc_tools_install: Option<String> = None;
    let mut vc_tools_compiler_dir: Option<std::path::PathBuf> = None;
    let vswhere_row_value: String;
    match vswhere_path {
        Some(p) => {
            // Filter to installs that have the VC.Tools.x86.x64 component
            // — this is what `cargo build` actually needs.
            let install = std::process::Command::new(&p)
                .args([
                    "-latest",
                    "-products",
                    "*",
                    "-requires",
                    "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                    "-property",
                    "installationPath",
                ])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    } else {
                        None
                    }
                });
            match install {
                Some(install_path) => {
                    vswhere_row_value = format!("ok ({install_path})");
                    // Resolve the concrete VC compiler bin directory:
                    //   <install>\VC\Tools\MSVC\<version>\bin\Hostx64\x64
                    // Reading <install>\VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt
                    // gives the version string. Inside that bin/ are link.exe + cl.exe.
                    if let Some(bin_dir) = locate_vc_compiler_bin(&install_path) {
                        if !link_ok && bin_dir.join("link.exe").is_file() {
                            link_ok = true;
                        }
                        if !cl_ok && bin_dir.join("cl.exe").is_file() {
                            cl_ok = true;
                        }
                        vc_tools_compiler_dir = Some(bin_dir);
                    }
                    vc_tools_install = Some(install_path);
                }
                None => {
                    vswhere_row_value =
                        "MISSING — VS Installer present but no VC.Tools.x86.x64 component".to_string();
                }
            }
        }
        None => {
            vswhere_row_value = "MISSING — install Visual Studio Installer".to_string();
        }
    }

    // Push link.exe / cl.exe rows AFTER vswhere has had a chance to
    // upgrade them to ok via the VC Tools install — closes "FAIL even
    // when MSVC is installed but not on PATH" (postmortem 2026-04-29 §6).
    rows.push(DoctorRow::new(
        "link.exe",
        if link_ok {
            match &vc_tools_compiler_dir {
                Some(d) if !which_on_path("link").is_some() => {
                    format!("ok (via vswhere: {})", d.display())
                }
                _ => "ok".to_string(),
            }
        } else {
            "MISSING — not on PATH and not in any VS install".to_string()
        },
    ));
    rows.push(DoctorRow::new(
        "cl.exe",
        if cl_ok {
            match &vc_tools_compiler_dir {
                Some(d) if !which_on_path("cl").is_some() => {
                    format!("ok (via vswhere: {})", d.display())
                }
                _ => "ok".to_string(),
            }
        } else {
            "MISSING — not on PATH and not in any VS install".to_string()
        },
    ));

    // Push the VC Tools row in its original position.
    if vc_tools_install.is_some() {
        rows.push(DoctorRow::new("VC Tools", vswhere_row_value.clone()));
    } else {
        // vswhere either missing or VS Installer found no VC component.
        // Use the row label "vswhere.exe" only when vswhere itself is
        // missing; otherwise label "VC Tools".
        let label = if vswhere_row_value.starts_with("MISSING — install Visual Studio Installer") {
            "vswhere.exe"
        } else {
            "VC Tools"
        };
        rows.push(DoctorRow::new(label, vswhere_row_value));
    }

    // Windows SDK probe — look for `kernel32.lib` under the standard
    // Windows Kits 10 layout. We pick the highest-numbered SDK
    // directory that contains the lib, mirroring the search msvc-rs
    // and link.exe perform at build time.
    let sdk_lib = locate_windows_sdk_kernel32_lib();
    match &sdk_lib {
        Some(path) => rows.push(DoctorRow::new("Windows SDK", format!("ok ({path})"))),
        None => rows.push(DoctorRow::new(
            "Windows SDK",
            "MISSING — install Windows 10/11 SDK (e.g. via VS Installer)",
        )),
    }

    // Roll the four signals into one summary verdict so users see PASS /
    // FAIL at a glance. After the vswhere upgrade above, link_ok / cl_ok
    // reflect filesystem reality — not just PATH visibility — so this
    // verdict matches what `cargo build` would actually be able to do.
    let any_compiler = link_ok || cl_ok;
    let toolchain_ok = any_compiler && (vc_tools_install.is_some() || sdk_lib.is_some());
    rows.push(DoctorRow::new(
        "summary",
        if toolchain_ok {
            "PASS — MSVC build toolchain available".to_string()
        } else {
            format!("FAIL — {MSVC_INSTALL_HINT}")
        },
    ));

    rows
}

/// Resolve the concrete MSVC compiler bin directory for a Visual Studio
/// install. Reads
/// `<install>\VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt`
/// to get the active VC Tools version, then returns
/// `<install>\VC\Tools\MSVC\<version>\bin\Hostx64\x64` if it exists.
/// Returns `None` if anything along the chain is missing.
#[cfg(windows)]
fn locate_vc_compiler_bin(install_path: &str) -> Option<std::path::PathBuf> {
    let install = std::path::Path::new(install_path);
    let ver_file = install
        .join("VC")
        .join("Auxiliary")
        .join("Build")
        .join("Microsoft.VCToolsVersion.default.txt");
    let ver = std::fs::read_to_string(&ver_file).ok()?.trim().to_string();
    if ver.is_empty() {
        return None;
    }
    let bin = install
        .join("VC")
        .join("Tools")
        .join("MSVC")
        .join(&ver)
        .join("bin")
        .join("Hostx64")
        .join("x64");
    if bin.is_dir() { Some(bin) } else { None }
}

/// Walk `%ProgramFiles(x86)%\Windows Kits\10\Lib\*\um\x64\kernel32.lib`
/// and return the highest-numbered SDK directory that actually contains
/// the lib. Returns `None` if no Windows SDK is installed. Pure stdlib.
#[cfg(windows)]
fn locate_windows_sdk_kernel32_lib() -> Option<String> {
    let pf = std::env::var_os("ProgramFiles(x86)")
        .or_else(|| std::env::var_os("ProgramFiles"))?;
    let lib_root = std::path::PathBuf::from(pf)
        .join("Windows Kits")
        .join("10")
        .join("Lib");
    let read = std::fs::read_dir(&lib_root).ok()?;
    let mut versions: Vec<std::path::PathBuf> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // Sort descending so the highest-numbered SDK wins.
    versions.sort();
    versions.reverse();
    for v in versions {
        let kernel32 = v.join("um").join("x64").join("kernel32.lib");
        if kernel32.is_file() {
            return Some(kernel32.display().to_string());
        }
    }
    None
}

#[cfg(not(windows))]
pub fn check_build_toolchain() -> Vec<DoctorRow> {
    Vec::new()
}

/// Render the build-toolchain section after the supervisor box.
/// No-ops on non-Windows.
fn print_build_toolchain_section() {
    let rows = check_build_toolchain();
    if rows.is_empty() {
        return;
    }
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ build toolchain (Windows MSVC)                          │");
    println!("├─────────────────────────────────────────────────────────┤");
    for row in rows {
        line(&row.label, &row.value);
    }
    println!("└─────────────────────────────────────────────────────────┘");
}

/// `YYYY-MM-DD HH:MM:SS UTC` without pulling chrono.
fn utc_now_readable() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let s = secs % 86_400;
    let hh = s / 3600;
    let mm = (s % 3600) / 60;
    let ss = s % 60;
    let (y, m, d) = ymd(days);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}
fn ymd(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { (mp + 3) as u32 } else { (mp - 9) as u32 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_on_path_finds_known_tool() {
        // `cargo` must be on PATH because `cargo test` is what's
        // running this test. If it isn't, the test environment is
        // broken — fail loudly.
        assert!(
            which_on_path("cargo").is_some(),
            "cargo must be on PATH when running cargo test"
        );
    }

    #[test]
    fn which_on_path_missing_tool_returns_none() {
        // A name that's exceedingly unlikely to exist on PATH.
        let needle = "this-binary-should-not-exist-on-any-developer-machine-12345";
        assert!(which_on_path(needle).is_none());
    }

    #[test]
    fn doctor_row_constructs_with_string_slice() {
        let row = DoctorRow::new("label", "value");
        assert_eq!(row.label, "label");
        assert_eq!(row.value, "value");
    }

    #[cfg(not(windows))]
    #[test]
    fn check_build_toolchain_empty_on_non_windows() {
        assert!(check_build_toolchain().is_empty());
    }

    #[test]
    fn copyright_constant_carries_both_names() {
        // I-14 closed: Anish picked Trivedi for both. Banner now says
        // "© 2026 Anish Trivedi & Kruti Trivedi".
        assert!(COPYRIGHT.contains("Anish Trivedi"));
        assert!(COPYRIGHT.contains("Kruti Trivedi"));
    }

    #[test]
    fn mcp_client_name_is_doctor_marker() {
        // NEW-027 closed: the doctor identifies itself with a fixed
        // clientInfo.name so server-side telemetry can recognise the
        // probe and short-circuit lazy initialisation. Value is
        // intentionally distinct from real clients.
        assert_eq!(MCP_CLIENT_NAME, "mneme-doctor");
    }

    #[test]
    fn known_toolchain_covers_g1_through_g10() {
        // G1-G10 from phase-a-issues.md §G. Pin the canonical set so
        // future edits don't accidentally drop a tool.
        let ids: Vec<&str> = KNOWN_TOOLCHAIN.iter().map(|t| t.issue_id).collect();
        for expected in &["G1", "G2", "G3", "G4", "G5", "G6", "G7", "G8", "G9", "G10"] {
            assert!(
                ids.contains(expected),
                "KNOWN_TOOLCHAIN missing entry for {expected}"
            );
        }
    }

    #[test]
    fn known_toolchain_severities_match_phase_a_priorities() {
        // High = blocks `--strict` exit. Map per phase-a-issues.md:
        //   G1 Rust         HIGH
        //   G2 Bun          HIGH
        //   G3 Node         HIGH
        //   G4 Tauri CLI    MEDIUM
        //   G5 Git          HIGH
        //   G6 Python       MEDIUM
        //   G7 SQLite       LOW
        //   G8 Java         LOW
        //   G9 Tesseract    MEDIUM
        //   G10 ImageMagick LOW
        let by_id = |id: &str| {
            KNOWN_TOOLCHAIN
                .iter()
                .find(|t| t.issue_id == id)
                .unwrap()
                .severity
        };
        assert_eq!(by_id("G1"), ToolSeverity::High);
        assert_eq!(by_id("G2"), ToolSeverity::High);
        assert_eq!(by_id("G3"), ToolSeverity::High);
        assert_eq!(by_id("G4"), ToolSeverity::Medium);
        assert_eq!(by_id("G5"), ToolSeverity::High);
        assert_eq!(by_id("G6"), ToolSeverity::Medium);
        assert_eq!(by_id("G7"), ToolSeverity::Low);
        assert_eq!(by_id("G8"), ToolSeverity::Low);
        assert_eq!(by_id("G9"), ToolSeverity::Medium);
        assert_eq!(by_id("G10"), ToolSeverity::Low);
    }

    #[test]
    fn known_toolchain_install_hints_are_actionable() {
        // Anish runs Windows; every entry must have a winget-shaped
        // hint or a clear official one-liner. None should be empty.
        for entry in KNOWN_TOOLCHAIN {
            assert!(
                !entry.hint_windows.is_empty(),
                "windows hint missing for {}",
                entry.issue_id
            );
            assert!(
                !entry.hint_unix.is_empty(),
                "unix hint missing for {}",
                entry.issue_id
            );
        }
    }

    #[test]
    fn probe_tool_returns_present_for_cargo_during_cargo_test() {
        // cargo must be on PATH while `cargo test` runs this — same
        // contract as the existing which_on_path test.
        let rust_entry = KNOWN_TOOLCHAIN
            .iter()
            .find(|t| t.issue_id == "G1")
            .expect("G1 entry");
        let probe = probe_tool(rust_entry);
        assert!(
            probe.is_present(),
            "rust toolchain probe failed during cargo test — env is broken"
        );
    }

    #[test]
    fn probe_tool_marks_known_missing_tool_absent() {
        // Synthetic entry pointing at a binary that cannot exist.
        let bogus = ToolchainEntry {
            display: "Bogus",
            probes: &["this-binary-is-not-installed-anywhere-12345"],
            cargo_subcommand: None,
            severity: ToolSeverity::Low,
            issue_id: "G99",
            purpose: "test fixture",
            hint_windows: "n/a",
            hint_unix: "n/a",
        };
        let probe = probe_tool(&bogus);
        assert!(!probe.is_present());
        assert!(probe.version.is_none());
    }

    #[test]
    fn install_hint_for_picks_platform_string() {
        let entry = &KNOWN_TOOLCHAIN[0]; // Rust
        let hint = install_hint_for(entry);
        if cfg!(windows) {
            assert_eq!(hint, entry.hint_windows);
        } else {
            assert_eq!(hint, entry.hint_unix);
        }
    }

    #[test]
    fn msvc_install_hint_mentions_winget_and_buildtools() {
        // I-16 closed: when the build toolchain probe fails, the user
        // gets one actionable line. Pin the wording so future edits do
        // not silently regress the install hint.
        assert!(MSVC_INSTALL_HINT.contains("winget install"));
        assert!(MSVC_INSTALL_HINT.contains("Microsoft.VisualStudio.2022.BuildTools"));
        assert!(MSVC_INSTALL_HINT.contains("VS Installer"));
    }

    /// M2 (audit DEEP-AUDIT-2026-04-29.md §M2): after the K1 fix in
    /// v0.3.2, `mneme install` defaults hooks ON and `--enable-hooks`
    /// is a deprecated no-op (`cli/src/commands/install.rs:91` +
    /// `CHANGELOG.md:91`). The doctor's remediation copy must NOT
    /// instruct the user to re-run a deprecated flag — it must point
    /// them at plain `mneme install` (and `--force` for the partial
    /// case).
    #[test]
    fn hooks_remediation_message_zero_drops_enable_hooks_flag() {
        let msg = hooks_remediation_message(0, 8);
        assert!(
            msg.contains("mneme install"),
            "remediation must mention `mneme install`: {msg}"
        );
        assert!(
            !msg.contains("--enable-hooks"),
            "remediation must NOT contain the deprecated `--enable-hooks` flag: {msg}"
        );
    }

    #[test]
    fn hooks_remediation_message_partial_drops_enable_hooks_flag() {
        let msg = hooks_remediation_message(3, 8);
        assert!(
            msg.contains("mneme install"),
            "remediation must mention `mneme install`: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "partial-registration remediation must keep `--force`: {msg}"
        );
        assert!(
            !msg.contains("--enable-hooks"),
            "remediation must NOT contain the deprecated `--enable-hooks` flag: {msg}"
        );
    }

    #[test]
    fn hooks_remediation_message_full_does_not_remediate() {
        // When all hooks registered, doctor must emit a positive
        // status, not a remediation line.
        let msg = hooks_remediation_message(8, 8);
        assert_eq!(msg, "8/8 entries registered");
        assert!(!msg.contains("re-run"));
        assert!(!msg.contains("--enable-hooks"));
    }

    /// Bug M10 (D-window class): the `mneme mcp stdio` child spawned
    /// from `probe_mcp_tools` must include the Windows
    /// `CREATE_NO_WINDOW` flag (`0x08000000`). When `mneme doctor`
    /// runs from a hook context (or `mneme audit --self-check`), a
    /// missing flag flashes a console window for the duration of the
    /// JSON-RPC handshake. The fix exposes a pure-Rust
    /// `windows_doctor_mcp_probe_flags()` helper that returns the
    /// canonical flag bitfield; this test pins the contract so future
    /// edits cannot silently regress it.
    #[test]
    fn windows_doctor_mcp_probe_flags() {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let flags = super::windows_doctor_mcp_probe_flags();
        assert_eq!(
            flags & CREATE_NO_WINDOW,
            CREATE_NO_WINDOW,
            "doctor mcp-probe spawn must set CREATE_NO_WINDOW (0x08000000); got {flags:#010x}"
        );
    }

    // -----------------------------------------------------------------
    // B-AGENT-C-2 (v0.3.2): doctor reports `0/8 hooks` when Claude
    // Code is running and overwrote settings.json with its in-memory
    // copy that lacks mneme's entries. The fix is a four-branch
    // truth-table on (claude_running, hooks_present) — see
    // `compose_hooks_message` for the contract. These tests pin every
    // branch independently so future edits don't silently collapse
    // them.
    //
    // Anish's exact quote: "install was good, just when i made mneme
    // doctor it showed all hooks missing and claude was on, i had to
    // close all and do mneme install and it came back but still not
    // plesent". The "still not plesent" is what Layer 2 fixes — even
    // when the underlying race is timing-dependent, the user gets a
    // crisp explanation instead of a scary `0/8`.
    // -----------------------------------------------------------------

    #[test]
    fn is_claude_code_running_never_panics() {
        // Either we get Some(pid) (Claude is open) or None (closed).
        // Either is correct — the contract is just totality.
        let _ = super::is_claude_code_running();
    }

    #[test]
    fn message_when_claude_running_and_hooks_missing_calls_out_pid() {
        // The headline-bug message — Anish's repro: install OK, Claude
        // up, doctor reports 0/8, user panics. New copy must say the
        // hooks are not detected AND that Claude is up AND give the
        // PID so the user can map it back to a window.
        let msg = super::compose_hooks_message(0, 8, Some(98765), None);
        assert!(msg.contains("0/8"), "must show count: {msg}");
        assert!(
            msg.to_lowercase().contains("claude"),
            "must name Claude Code: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("running")
                || msg.to_lowercase().contains("open"),
            "must indicate Claude is alive: {msg}"
        );
        assert!(msg.contains("98765"), "must include the PID: {msg}");
        assert!(
            msg.to_lowercase().contains("close"),
            "must tell the user to close Claude: {msg}"
        );
    }

    #[test]
    fn message_when_claude_not_running_and_hooks_missing_keeps_install_remediation() {
        // True negative: install genuinely didn't take. Old copy is
        // correct here — point the user at `mneme install`.
        let msg = super::compose_hooks_message(0, 8, None, None);
        assert!(msg.contains("0/8"), "must show count: {msg}");
        assert!(
            msg.contains("mneme install"),
            "true-negative must point to `mneme install`: {msg}"
        );
        // Critically: must NOT pretend Claude is the cause.
        assert!(
            !msg.to_lowercase().contains("running"),
            "must not blame Claude when it isn't open: {msg}"
        );
    }

    #[test]
    fn message_when_claude_running_and_hooks_present_emits_restart_reminder() {
        // Hooks are wired correctly (8/8) but Claude was already open
        // when the user installed. New hooks won't fire in
        // already-open sessions until Claude is restarted.
        let msg = super::compose_hooks_message(8, 8, Some(12345), None);
        assert!(msg.contains("8/8"), "must show count: {msg}");
        assert!(
            msg.to_lowercase().contains("restart")
                || msg.to_lowercase().contains("won't fire")
                || msg.to_lowercase().contains("won't pick"),
            "running-Claude-with-hooks-present must remind to restart: {msg}"
        );
    }

    #[test]
    fn message_when_claude_not_running_and_hooks_present_is_clean() {
        // Happy path: 8/8 + Claude closed.
        let msg = super::compose_hooks_message(8, 8, None, None);
        assert!(msg.contains("8/8"), "must show count: {msg}");
        assert!(
            msg.contains("entries registered"),
            "happy path must show the existing 'entries registered' line: {msg}"
        );
        // Must NOT scare the user with "running" or "restart".
        assert!(
            !msg.to_lowercase().contains("restart"),
            "happy path must not emit a restart reminder: {msg}"
        );
    }

    #[test]
    fn message_when_read_error_surfaces_diagnostic() {
        // Layer 1 wired-through-Layer-2: when the detailed counter
        // reports a parse_error we must surface it instead of silently
        // returning "0/8 — re-run mneme install" which would mislead
        // the user.
        let parse_err = "settings.json failed to parse as JSON: trailing comma at line 12";
        let msg = super::compose_hooks_message(0, 8, None, Some(parse_err.to_string()));
        assert!(
            msg.contains("settings.json"),
            "diagnostic must mention the file: {msg}"
        );
        assert!(
            msg.contains("parse") || msg.contains("trailing comma"),
            "diagnostic must surface the concrete reason: {msg}"
        );
    }

    #[test]
    fn message_with_claude_running_and_read_error_combines_both_signals() {
        // If Claude is up AND parse failed (e.g. Claude's mid-write
        // produced a partial JSON read), the message must mention BOTH
        // — Claude-is-open is the likely cause, parse-error is the
        // concrete symptom.
        let parse_err = "unexpected end of JSON input";
        let msg = super::compose_hooks_message(
            0,
            8,
            Some(54321),
            Some(parse_err.to_string()),
        );
        assert!(msg.contains("54321"), "must include PID: {msg}");
        assert!(
            msg.contains("parse") || msg.contains("unexpected end"),
            "must surface parse error: {msg}"
        );
    }
}
