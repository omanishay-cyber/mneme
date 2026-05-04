//! `mneme daemon <op>` — start/stop/restart/status/logs subcommands for
//! the supervisor process.
//!
//! `start` and `restart` shell out to the supervisor binary. It ships as
//! `mneme-daemon` (see `supervisor/Cargo.toml`), with a legacy search
//! fallback to `mneme-supervisor` for older installs. The wrapper always
//! passes the `start` subcommand — the binary itself is a clap CLI that
//! requires a subcommand; invoking bare prints help and exits, which is
//! what F-004/F-005 in the v0.3.0 install-report were reporting.
//!
//! `stop`, `status`, and `logs` go over the IPC socket so we never have to
//! know the supervisor's PID.

use clap::Args;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::{info, warn};

use crate::commands::build::{handle_response, make_client};
use crate::error::{CliError, CliResult};
use crate::ipc::IpcRequest;

// Windows process-creation flags. Defined here (rather than imported from
// `winapi`/`windows-sys`) to keep the CLI lean — these constants are stable
// kernel32 ABI and won't change.
//
// - DETACHED_PROCESS (0x00000008): child has no console at all. We want this
//   so a `mneme daemon start` invoked from CMD/PowerShell does not pin the
//   parent terminal. Mutually exclusive with CREATE_NEW_CONSOLE.
// - CREATE_NEW_PROCESS_GROUP (0x00000200): child becomes the root of its
//   own group, so Ctrl+C in the spawning shell does not propagate.
// - CREATE_BREAKAWAY_FROM_JOB (0x01000000): child detaches from the parent's
//   job object. Modern shells (Windows Terminal, VS Code integrated term,
//   Claude Code itself) often run inside a Job that kills the whole tree on
//   shell exit. Without breakaway the daemon dies when the launching shell
//   closes — which is exactly the I-7 regression. Some Jobs forbid breakaway
//   (JOB_OBJECT_LIMIT_BREAKAWAY_OK not set); in that case spawn fails with
//   ERROR_ACCESS_DENIED and we retry without the flag, with a warning.
// - CREATE_NO_WINDOW (0x08000000): suppress the brief console flash that
//   the OS otherwise creates while DETACHED_PROCESS is being applied. M6
//   in DEEP-AUDIT-2026-04-29 — without this, hidden parents (Claude Code
//   hooks spawned windowless) can still see a transient cmd.exe window
//   on `mneme daemon start`.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// M6 (D-window): primary flag composition for `daemon::spawn_detached`.
///
/// Includes `CREATE_NO_WINDOW` (`0x0800_0000`) on top of the original
/// detach + new-group + breakaway-from-job set. Total: `0x0900_0208`.
///
/// Extracted as `pub(crate)` so the unit test can verify the bit
/// composition without a real spawn.
#[cfg(windows)]
pub(crate) fn windows_daemon_spawn_detached_primary_flags() -> u32 {
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW
}

/// M6 (D-window): fallback flag composition when the primary set fails
/// with ERROR_ACCESS_DENIED (job-object forbids breakaway).
///
/// Drops only `CREATE_BREAKAWAY_FROM_JOB`; keeps `CREATE_NO_WINDOW` so
/// the console-flash regression is covered on the fallback path too.
/// Total: `0x0800_0208`.
#[cfg(windows)]
pub(crate) fn windows_daemon_spawn_detached_fallback_flags() -> u32 {
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
}

/// CLI args for `mneme daemon`.
#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Sub-op: `start` | `stop` | `restart` | `status` | `logs`.
    pub op: String,

    /// Override path to the supervisor binary (used by `start` / `restart`).
    #[arg(long, env = "MNEME_SUPERVISOR_BIN")]
    pub bin: Option<PathBuf>,

    /// For `logs`: how many tail lines to fetch.
    #[arg(long, default_value_t = 200)]
    pub lines: usize,
}

/// Entry point used by `main.rs`.
pub async fn run(args: DaemonArgs, socket_override: Option<PathBuf>) -> CliResult<()> {
    match args.op.as_str() {
        "start" => start_daemon(args.bin),
        "stop" => stop_daemon(socket_override).await,
        "restart" => {
            // Best-effort stop, then start. Order matters; we don't want
            // two supervisors fighting over the IPC socket.
            let _ = stop_daemon(socket_override.clone()).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            start_daemon(args.bin)
        }
        "status" => status_daemon(socket_override).await,
        "logs" => logs_daemon(socket_override, args.lines).await,
        other => Err(CliError::Other(format!("unknown daemon op: {other}"))),
    }
}

fn start_daemon(bin: Option<PathBuf>) -> CliResult<()> {
    let path = bin.unwrap_or_else(default_supervisor_binary);
    info!(path = %path.display(), "spawning supervisor");
    if !path.exists() {
        return Err(CliError::Other(format!(
            "supervisor binary not found at {}; install mneme first or pass --bin",
            path.display()
        )));
    }
    // Always pass `start` — the supervisor binary is a clap CLI with
    // required subcommand. Detach stdio so the spawned daemon does not
    // hold the calling shell open (F-004 / F-005 regression gate).
    spawn_detached(&path)?;
    println!("supervisor started ({} start)", path.display());
    Ok(())
}

#[cfg(windows)]
fn spawn_detached(path: &std::path::Path) -> CliResult<()> {
    use std::os::windows::process::CommandExt;

    // First attempt: full detach including breakaway from any parent job
    // AND CREATE_NO_WINDOW to suppress the transient console flash (M6).
    // This is the only configuration that lets the daemon survive when the
    // launching shell (and its job object) goes away (I-7) without flashing
    // a console window when the parent is hidden.
    let full_flags = windows_daemon_spawn_detached_primary_flags();
    let mut cmd = Command::new(path);
    cmd.arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(full_flags);
    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(first_err) => {
            // ERROR_ACCESS_DENIED (5) is the symptom when the parent's
            // Job Object doesn't permit breakaway. The classic
            // "non-detached fallback" leaves the daemon married to the
            // parent's Job — it dies the moment the install shell closes
            // (postmortem 2026-04-29 §3.D + §12.5: "every taskkill batch
            // had completely different PIDs because the daemon was being
            // respawned by the next Claude tool call ...").
            //
            // Proper fix: register + run the daemon as a Windows
            // Scheduled Task. Task Scheduler runs tasks under svchost —
            // outside every Job Object in the shell hierarchy — so the
            // daemon survives parent termination AND auto-restarts at
            // every user logon (reboot survival).
            let raw = first_err.raw_os_error();
            warn!(
                error = %first_err,
                raw = ?raw,
                "spawn with CREATE_BREAKAWAY_FROM_JOB failed; falling over to Windows Scheduled Task"
            );

            match try_register_and_run_scheduled_task(path) {
                Ok(()) => {
                    info!(
                        "daemon registered + started via Windows Scheduled Task 'MnemeDaemon' \
                         (auto-restart at every user logon)"
                    );
                    return Ok(());
                }
                Err(sch_err) => {
                    warn!(
                        error = %sch_err,
                        "Scheduled Task registration failed; falling back to non-detached spawn"
                    );
                }
            }

            // Last-resort: non-detached spawn. The daemon will die with
            // the parent Job, but at least it runs briefly. Documented
            // failure mode for hosts where schtasks is also restricted.
            let fallback_flags = windows_daemon_spawn_detached_fallback_flags();
            let mut cmd = Command::new(path);
            cmd.arg("start")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(fallback_flags);
            cmd.spawn().map_err(|e| CliError::io(path, e))?;
            warn!(
                "supervisor spawned without job-object breakaway and without scheduled task; \
                 daemon will exit if the parent job is terminated"
            );
            Ok(())
        }
    }
}

/// Register `mneme-daemon.exe start` as a Windows Scheduled Task and run
/// it immediately. Used as the proper fallback when
/// `CREATE_BREAKAWAY_FROM_JOB` is denied by the parent's Job Object
/// (postmortem 2026-04-29 §3.D + §12.5).
///
/// Why Task Scheduler: tasks run under the Schedule service (svchost),
/// which has no parent Job Object. The daemon survives:
///   - the install shell closing
///   - SSH session terminations
///   - reboots (via /SC ONLOGON it auto-respawns at next user logon)
///
/// `/F` overwrites any existing task (idempotent reinstall path).
/// `/IT` keeps it interactive so it appears in Task Scheduler's per-user
/// view without admin (matches mneme's no-admin install policy).
/// `/SC ONLOGON` triggers at every user logon — the reboot-survival hook.
/// We `/Run` it explicitly afterward so the daemon is up for the current
/// session without waiting for a logoff/logon cycle.
#[cfg(windows)]
fn try_register_and_run_scheduled_task(
    daemon_path: &std::path::Path,
) -> Result<(), std::io::Error> {
    let task_name = "MnemeDaemon";

    // A1-027 (2026-05-04): validate daemon_path against shell-meta chars
    // before splicing into a schtasks /TR string. The /TR argument is
    // parsed by schtasks as a command line, so a path containing `"`,
    // `&`, `|`, `<`, `>`, `^`, `;`, or newline could inject extra
    // commands or corrupt task registration. NTFS technically permits
    // some of these characters in filenames; user-controlled MNEME_HOME
    // could put a malicious path into our process. Refuse loudly.
    let path_str = daemon_path.display().to_string();
    let bad_chars = ['"', '&', '|', '<', '>', '^', ';', '\n', '\r'];
    if path_str.chars().any(|c| bad_chars.contains(&c)) {
        return Err(std::io::Error::other(format!(
            "daemon path contains shell-meta characters refusing to register schtasks task: {:?}",
            path_str
        )));
    }
    let tr = format!("\"{}\" start", path_str);

    let create = Command::new("schtasks.exe")
        .args([
            "/Create",
            "/TN",
            task_name,
            "/TR",
            tr.as_str(),
            "/SC",
            "ONLOGON",
            "/F",
            "/IT",
        ])
        .output()?;
    if !create.status.success() {
        return Err(std::io::Error::other(format!(
            "schtasks /Create exit {}: {}",
            create.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&create.stderr).trim()
        )));
    }

    let run = Command::new("schtasks.exe")
        .args(["/Run", "/TN", task_name])
        .output()?;
    if !run.status.success() {
        return Err(std::io::Error::other(format!(
            "schtasks /Run exit {}: {}",
            run.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&run.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(path: &std::path::Path) -> CliResult<()> {
    // On Unix the supervisor handles its own daemonization via the
    // double-fork in `supervisor/src/service.rs::daemonize`. We just need
    // to disconnect stdio so the parent shell isn't held open.
    Command::new(path)
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| CliError::io(path, e))?;
    Ok(())
}

async fn stop_daemon(socket_override: Option<PathBuf>) -> CliResult<()> {
    let client = make_client(socket_override);
    match client.request(IpcRequest::Stop).await {
        Ok(_) => {
            println!("supervisor stop requested");
            Ok(())
        }
        Err(e) => {
            // If the socket's gone we treat that as "already stopped".
            warn!(error = %e, "stop request failed; supervisor may already be down");
            println!("supervisor not reachable (probably already stopped)");
            Ok(())
        }
    }
}

async fn status_daemon(socket_override: Option<PathBuf>) -> CliResult<()> {
    let client = make_client(socket_override);
    if !client.is_running().await {
        println!("supervisor: NOT RUNNING");
        return Ok(());
    }
    let resp = client.request(IpcRequest::Status { project: None }).await?;
    handle_response(resp)
}

async fn logs_daemon(socket_override: Option<PathBuf>, lines: usize) -> CliResult<()> {
    let client = make_client(socket_override);
    let resp = client
        .request(IpcRequest::Logs {
            child: None,
            n: lines,
        })
        .await?;
    handle_response(resp)
}

fn default_supervisor_binary() -> PathBuf {
    // The supervisor is shipped as `mneme-daemon` (see `supervisor/Cargo.toml`
    // `[[bin]] name = "mneme-daemon"`). Older 0.1.x / 0.2.x manual workarounds
    // expected `mneme-supervisor` — we keep that as a legacy fallback so users
    // with `$env:MNEME_SUPERVISOR_BIN` or older PATH entries are not broken.
    //
    // Search order, first match wins:
    //   1. `<dir-of-mneme>/mneme-daemon[.exe]`     — shipped binary
    //   2. `<dir-of-mneme>/mneme-supervisor[.exe]` — legacy symlink / rename
    //   3. `mneme-daemon[.exe]` on PATH
    //   4. `mneme-supervisor[.exe]` on PATH          (legacy)
    let candidates = ["mneme-daemon", "mneme-supervisor"];

    if let Ok(this) = std::env::current_exe() {
        if let Some(parent) = this.parent() {
            for name in candidates {
                let mut c = parent.join(name);
                if cfg!(windows) {
                    c.set_extension("exe");
                }
                if c.exists() {
                    return c;
                }
            }
        }
    }

    // PATH fallback — return the first name so the caller's error message
    // is informative if nothing resolves.
    let mut p = PathBuf::from(candidates[0]);
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helper surfaces above.
//
// WIRE-005 follow-up: `daemon.rs` previously had ZERO unit tests. Most of
// the file is process-spawn / IPC glue (untestable in isolation), but
// `default_supervisor_binary` is pure path-construction and is exercised
// here. Anything heavier is covered by the supervisor crate's own tests.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_supervisor_binary_returns_a_path() {
        // Function must always return a PathBuf — never panic, never
        // empty. Either it found a sibling daemon binary next to the
        // current exe, or it returned the PATH-fallback name.
        let p = default_supervisor_binary();
        let s = p.to_string_lossy();
        assert!(!s.is_empty(), "default_supervisor_binary returned empty");
    }

    #[test]
    fn default_supervisor_binary_has_exe_extension_on_windows() {
        // On Windows the helper unconditionally appends `.exe` (either to
        // a sibling-dir candidate or to the PATH-fallback `mneme-daemon`).
        // On POSIX no extension is added.
        let p = default_supervisor_binary();
        if cfg!(windows) {
            assert_eq!(
                p.extension().and_then(|e| e.to_str()),
                Some("exe"),
                "expected .exe extension on Windows, got {}",
                p.display()
            );
        } else {
            // POSIX leaves the candidate bare.
            assert!(p.extension().is_none());
        }
    }

    #[test]
    fn default_supervisor_binary_uses_known_daemon_name() {
        // Either `mneme-daemon` or the legacy `mneme-supervisor` name is
        // acceptable — both are documented in the search order above.
        let p = default_supervisor_binary();
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        assert!(
            stem == "mneme-daemon" || stem == "mneme-supervisor",
            "unexpected supervisor binary name: {stem}"
        );
    }

    /// M6 (D-window): both the primary and fallback flag composers in
    /// `daemon::spawn_detached` MUST set `CREATE_NO_WINDOW`
    /// (`0x0800_0000`) so a `mneme daemon start` invoked from a hidden
    /// parent never flashes a transient console window.
    ///
    /// Primary expected: `0x0900_0208`
    ///   = DETACHED_PROCESS (0x0000_0008)
    ///   | CREATE_NEW_PROCESS_GROUP (0x0000_0200)
    ///   | CREATE_BREAKAWAY_FROM_JOB (0x0100_0000)
    ///   | CREATE_NO_WINDOW (0x0800_0000)
    ///
    /// Fallback expected: `0x0800_0208`
    ///   = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    /// (drops only CREATE_BREAKAWAY_FROM_JOB; keeps CREATE_NO_WINDOW).
    #[cfg(windows)]
    #[test]
    fn windows_daemon_spawn_detached_flags() {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

        // Primary: must include all four flags.
        let primary = super::windows_daemon_spawn_detached_primary_flags();
        assert!(
            primary & CREATE_NO_WINDOW == CREATE_NO_WINDOW,
            "M6 primary: must include CREATE_NO_WINDOW; got 0x{primary:08x}"
        );
        assert!(primary & DETACHED_PROCESS == DETACHED_PROCESS);
        assert!(primary & CREATE_NEW_PROCESS_GROUP == CREATE_NEW_PROCESS_GROUP);
        assert!(primary & CREATE_BREAKAWAY_FROM_JOB == CREATE_BREAKAWAY_FROM_JOB);

        // Fallback: must keep CREATE_NO_WINDOW; must drop only breakaway.
        let fallback = super::windows_daemon_spawn_detached_fallback_flags();
        assert!(
            fallback & CREATE_NO_WINDOW == CREATE_NO_WINDOW,
            "M6 fallback: must include CREATE_NO_WINDOW; got 0x{fallback:08x}"
        );
        assert!(fallback & DETACHED_PROCESS == DETACHED_PROCESS);
        assert!(fallback & CREATE_NEW_PROCESS_GROUP == CREATE_NEW_PROCESS_GROUP);
        assert!(
            fallback & CREATE_BREAKAWAY_FROM_JOB == 0,
            "M6 fallback: must NOT set CREATE_BREAKAWAY_FROM_JOB \
             (that's the only flag dropped); got 0x{fallback:08x}"
        );
    }
}
