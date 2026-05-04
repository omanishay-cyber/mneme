//! `mneme-supervisor` binary entry point.
//!
//! Subcommands:
//!   - `start`   — boot the supervisor in the foreground.
//!   - `service-run` — used by the Windows service control manager; do not
//!     invoke directly.
//!   - `install` / `uninstall` — manage the Windows service registration.
//!   - `stop`    — send a `Stop` over IPC.
//!   - `restart` — send a `RestartAll` (or `Restart {child}`) over IPC.
//!   - `status`  — print the current child snapshot.
//!   - `logs`    — tail recent log entries.

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use mneme_daemon::config::SupervisorConfig;
use mneme_daemon::error::SupervisorError;
use mneme_daemon::ipc::{self, ControlCommand, ControlResponse};
use mneme_daemon::service::{self, ServiceAction};
use mneme_daemon::watcher::{self, WatcherStatsHandle, DEFAULT_DEBOUNCE};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::error;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[derive(Debug, Parser)]
#[command(name = "mneme-supervisor", version, about = "Mneme process supervisor", long_about = None)]
struct Cli {
    /// Path to the supervisor TOML config.
    #[arg(long, env = "MNEME_CONFIG")]
    config: Option<PathBuf>,

    /// Override the IPC socket / pipe path for client subcommands.
    #[arg(long, env = "MNEME_IPC")]
    ipc: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start the supervisor in the foreground.
    Start {
        /// K10 chaos-test-only: panic the worker selected to handle the
        /// Nth dispatched job (1-indexed). Counts down from N; when it
        /// hits zero the supervisor invokes `panic!()` inside the
        /// dispatcher, which is captured by the per-child monitor as
        /// "exit code -1" and the restart loop respawns the worker.
        ///
        /// Only available when the binary is built with
        /// `--features test-hooks`. Production users see no `--inject-crash`
        /// in `--help`. Acts as a no-op (`0`) when omitted.
        #[cfg(feature = "test-hooks")]
        #[arg(long, default_value_t = 0)]
        inject_crash: u64,
    },
    /// Hand off to the Windows service control manager.
    ServiceRun,
    /// Install as a Windows service (no-op on Unix).
    Install,
    /// Uninstall the Windows service (no-op on Unix).
    Uninstall,
    /// Send a graceful Stop over IPC.
    Stop,
    /// Restart all children (or a single named child).
    Restart {
        /// Optional child name (omit to restart all).
        #[arg(long)]
        child: Option<String>,
    },
    /// Print supervisor + child status as JSON.
    Status,
    /// Tail recent log entries.
    Logs {
        /// Limit to a single child.
        #[arg(long)]
        child: Option<String>,
        /// How many entries to print.
        #[arg(long, default_value_t = 100)]
        n: usize,
    },
    /// Watch a project directory and incrementally re-index on save.
    /// Blocks until Ctrl-C. Writes `file_reindexed` events to livebus if
    /// the socket path is reachable.
    Watch {
        /// Project root to watch (defaults to CWD).
        #[arg(long)]
        project: Option<PathBuf>,
        /// Optional livebus IPC socket path to emit events on.
        #[arg(long, env = "MNEME_LIVEBUS")]
        livebus: Option<PathBuf>,
        /// Debounce window in milliseconds.
        #[arg(long, default_value_t = 250)]
        debounce_ms: u64,
    },
}

fn main() -> std::process::ExitCode {
    // The non-blocking file-appender guard MUST live for the entire run
    // of `main`; dropping it flushes & shuts down the writer thread, so
    // we bind it explicitly even though it looks unused. B-005 fix.
    let _file_log_guard = init_tracing();

    let cli = Cli::parse();
    // I-4 / I-5 / NEW-008: cap the tokio runtime. The supervisor is a
    // long-lived control-plane process — the worker_threads/blocking
    // pools should NOT scale with `num_cpus` on a 32-core box and stay
    // pinned even when nothing is running. min(4) covers typical IPC
    // burst (status/metrics/logs/dispatch) while keeping baseline RSS
    // predictable. max_blocking_threads(8) keeps stdin/stdout forwarder
    // tasks from accreting on a flapping worker pool.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(supervisor_worker_threads(num_cpus::get()))
        .max_blocking_threads(8)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "failed to build tokio runtime");
            return std::process::ExitCode::FAILURE;
        }
    };

    let result = rt.block_on(run_cli(cli));
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "command failed");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Wire up tracing for the supervisor process.
///
/// Two layers compose into a single subscriber:
///
/// * **stdout layer** — JSON, `WARN+` only. Live operator visibility for
///   anyone running the supervisor in the foreground (`mneme daemon
///   start` falls through to this codepath after detaching). Workers
///   that emit color codes are stripped at the [`crate::log_ring`]
///   boundary, but the daemon's own log lines never colorise.
/// * **file layer** — JSON, `DEBUG+`. Rolling daily appender at
///   `<MNEME_HOME>/logs/supervisor.log`, rotated daily, keeping the most
///   recent 7 days. This is the canonical durable log surface that
///   `mneme daemon logs` tails when the in-memory ring is empty (B-005).
///
/// `MNEME_LOG` overrides both layers' filters at once (`info` keeps the
/// pre-fix behaviour). On a read-only filesystem the file layer is
/// disabled and a single warning is printed to stdout — the supervisor
/// still boots.
///
/// Returns the [`WorkerGuard`] for the non-blocking file writer; the
/// caller must keep it alive for the lifetime of the program. Drop ⇒
/// flush + writer-thread shutdown.
fn init_tracing() -> Option<WorkerGuard> {
    let stdout_filter = EnvFilter::try_from_env("MNEME_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,mneme_supervisor=warn"));

    let stdout_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(std::io::stdout)
        .with_filter(stdout_filter);

    // Build the file appender. If the dir can't be created (read-only
    // FS, permission error in a sandbox) we fall back to stdout-only and
    // surface a warning — supervisors without persistent logs still
    // boot.
    let file_layer_with_guard: Option<(_, WorkerGuard)> = match mneme_daemon::ensure_logs_dir() {
        Ok(dir) => {
            match RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix("supervisor.log")
                .max_log_files(7)
                .build(&dir)
            {
                Ok(appender) => {
                    let (nb, guard) = tracing_appender::non_blocking(appender);
                    let file_filter = EnvFilter::try_from_env("MNEME_LOG")
                        .unwrap_or_else(|_| EnvFilter::new("debug,mneme_supervisor=debug"));
                    let layer = tracing_subscriber::fmt::layer()
                        .json()
                        .with_current_span(false)
                        .with_span_list(false)
                        .with_ansi(false)
                        .with_writer(nb)
                        .with_filter(file_filter);
                    Some((layer, guard))
                }
                Err(e) => {
                    eprintln!(
                        "warning: could not build supervisor log appender ({e}); \
                             file logging disabled, stdout only"
                    );
                    None
                }
            }
        }
        Err(e) => {
            eprintln!(
                "warning: could not create supervisor logs dir ({e}); \
                     file logging disabled, stdout only"
            );
            None
        }
    };

    match file_layer_with_guard {
        Some((file_layer, guard)) => {
            let _ = tracing_subscriber::registry()
                .with(stdout_layer)
                .with(file_layer)
                .try_init();
            Some(guard)
        }
        None => {
            let _ = tracing_subscriber::registry().with(stdout_layer).try_init();
            None
        }
    }
}

async fn run_cli(cli: Cli) -> Result<(), SupervisorError> {
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);

    match cli.command {
        #[cfg(feature = "test-hooks")]
        Cmd::Start { inject_crash } => {
            let config = SupervisorConfig::load(&config_path)?;
            // K10 chaos-test hook: store the configured countdown in a
            // process-wide atomic that `ChildManager::dispatch_to_pool`
            // reads on every dispatch. `0` (the default) disables the
            // hook entirely so the production-feature build is a no-op
            // even if the user somehow passes `--inject-crash 0`.
            if inject_crash > 0 {
                mneme_daemon::test_hooks::set_inject_crash(inject_crash);
                tracing::warn!(
                    n = inject_crash,
                    "K10 test hook armed: worker dispatch will panic on job N",
                );
            }
            service::execute(ServiceAction::RunForeground, config).await
        }
        #[cfg(not(feature = "test-hooks"))]
        Cmd::Start {} => {
            let config = SupervisorConfig::load(&config_path)?;
            service::execute(ServiceAction::RunForeground, config).await
        }
        Cmd::ServiceRun => {
            // Under SCM, this code path runs INSIDE the service worker
            // process. SCM gives us only ~30s after start before flagging
            // "service did not respond in time" (NEW-013). A bad config
            // file would otherwise propagate as an error here BEFORE we
            // ever hand control to the dispatcher, leaving SCM hung.
            // Fall back to the default layout so the dispatcher always
            // gets called; the dispatcher's own service_main then signals
            // RUNNING immediately and re-loads config with the same
            // fallback so the service still boots even on misconfigured
            // installs.
            let config = SupervisorConfig::load(&config_path).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "service-run: config load failed; using default_layout");
                SupervisorConfig::default_layout()
            });
            service::execute(ServiceAction::RunAsService, config).await
        }
        Cmd::Install => {
            let config = SupervisorConfig::load(&config_path)?;
            service::execute(ServiceAction::Install, config).await
        }
        Cmd::Uninstall => {
            let config = SupervisorConfig::load(&config_path)?;
            service::execute(ServiceAction::Uninstall, config).await
        }
        Cmd::Stop => {
            let socket = cli.ipc.unwrap_or_else(default_ipc_path);
            let resp = round_trip(&socket, &ControlCommand::Stop).await?;
            print_response(&resp);
            Ok(())
        }
        Cmd::Restart { child } => {
            let socket = cli.ipc.unwrap_or_else(default_ipc_path);
            let cmd = match child {
                Some(c) => ControlCommand::Restart { child: c },
                None => ControlCommand::RestartAll,
            };
            let resp = round_trip(&socket, &cmd).await?;
            print_response(&resp);
            Ok(())
        }
        Cmd::Status => {
            let socket = cli.ipc.unwrap_or_else(default_ipc_path);
            let resp = round_trip(&socket, &ControlCommand::Status).await?;
            print_response(&resp);
            Ok(())
        }
        Cmd::Logs { child, n } => {
            let socket = cli.ipc.unwrap_or_else(default_ipc_path);
            let resp = round_trip(&socket, &ControlCommand::Logs { child, n }).await?;
            print_response(&resp);
            Ok(())
        }
        Cmd::Watch {
            project,
            livebus,
            debounce_ms,
        } => {
            let root = project
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let stats = WatcherStatsHandle::new();
            let debounce = if debounce_ms == 0 {
                DEFAULT_DEBOUNCE
            } else {
                std::time::Duration::from_millis(debounce_ms)
            };
            tracing::info!(
                root = %root.display(),
                debounce_ms = debounce.as_millis() as u64,
                "starting watcher"
            );
            let watch_fut = watcher::run_watcher(root, livebus, stats, debounce);
            tokio::select! {
                result = watch_fut => {
                    if let Err(e) = result {
                        return Err(SupervisorError::Other(format!("watcher exited: {e}")));
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("ctrl-c received, watcher shutting down");
                }
            }
            Ok(())
        }
    }
}

fn print_response(resp: &ControlResponse) {
    match serde_json::to_string_pretty(resp) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to render response: {e}"),
    }
}

/// Per-read deadline applied to the IPC client. The server side already
/// honours `IPC_READ_TIMEOUT` (`ipc.rs`) but the client previously
/// awaited reads forever, which let a wedged daemon (e.g. dispatcher
/// starved by BUG-A4-001) hang `mneme daemon status` indefinitely. Set
/// to 30 s -- long enough for legitimate slow responses (status
/// snapshot of 10+ workers) but short enough that the user knows the
/// daemon is unresponsive without learning to hit Ctrl+C.
const CLIENT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn round_trip(
    socket: &Path,
    cmd: &ControlCommand,
) -> Result<ControlResponse, SupervisorError> {
    let mut stream = ipc::connect_client(socket).await?;

    let body = serde_json::to_vec(cmd)?;
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;

    // BUG-A4-007 fix (2026-05-04): wrap response reads in a timeout so
    // a wedged daemon does not hang the CLI forever. The previous code
    // awaited `read_exact` with no upper bound; if the daemon accepted
    // the connection but its dispatcher was starved (e.g. by sync
    // SQLite work in api_graph BUG-A4-001) the client would block
    // indefinitely -- only Ctrl+C broke out.
    let mut len_buf = [0u8; 4];
    match tokio::time::timeout(CLIENT_READ_TIMEOUT, stream.read_exact(&mut len_buf)).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(SupervisorError::Ipc(format!(
                "daemon not responding (no reply within {:?}); is the supervisor wedged?",
                CLIENT_READ_TIMEOUT
            )));
        }
    };
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_body = vec![0u8; resp_len];
    match tokio::time::timeout(CLIENT_READ_TIMEOUT, stream.read_exact(&mut resp_body)).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(SupervisorError::Ipc(format!(
                "daemon not responding mid-read (timeout after {:?})",
                CLIENT_READ_TIMEOUT
            )));
        }
    };

    let resp: ControlResponse = serde_json::from_slice(&resp_body)?;
    Ok(resp)
}

fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MNEME_CONFIG") {
        return PathBuf::from(p);
    }
    let mut base = home_dir();
    base.push(".mneme");
    base.push("supervisor.toml");
    base
}

/// Resolve the IPC socket / pipe path for client subcommands.
///
/// Discovery order (first hit wins):
///   1. The `MNEME_IPC` env var or `--ipc` CLI flag (handled upstream).
///   2. `~/.mneme/supervisor.pipe` — written by the supervisor on boot in
///      [`crate::run`] (`lib.rs:56-62`). On Windows this resolves the
///      PID-scoped pipe name a running supervisor actually advertises;
///      without it, raw `mneme-daemon status` would always miss because
///      the unscoped legacy name is never bound (I-8).
///   3. Platform-specific fallback (legacy unscoped pipe on Windows, the
///      default `~/.mneme/supervisor.sock` on Unix). Kept so brand-new
///      installs without a discovery file still produce a coherent
///      "supervisor not running" error rather than panicking.
fn default_ipc_path() -> PathBuf {
    // K10 test hook: when `MNEME_TEST_SOCKET_NAME` is set, the test
    // suite is driving both the daemon and its client; use the same
    // override here so client subcommands (`status`, `logs`, etc.)
    // talk to the test-scoped pipe instead of the system-wide one.
    if let Ok(custom) = std::env::var("MNEME_TEST_SOCKET_NAME") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            #[cfg(windows)]
            {
                return PathBuf::from(format!(r"\\.\pipe\{}", trimmed));
            }
            #[cfg(unix)]
            {
                let mut base = home_dir();
                base.push(".mneme");
                base.push(trimmed);
                return base;
            }
        }
    }
    let mut disco = home_dir();
    disco.push(".mneme");
    disco.push("supervisor.pipe");
    if let Ok(contents) = std::fs::read_to_string(&disco) {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            // BUG-A4-012 fix (2026-05-04): validate the discovery file
            // against `~/.mneme/run/daemon.pid` before returning the
            // value. On a supervisor crash / SIGKILL / OOM-kill the
            // best-effort cleanup in lib.rs:run() never runs, so
            // `supervisor.pipe` retains the dead PID-scoped pipe name
            // and every subsequent `mneme daemon status` failed with
            // an opaque "connection refused" error. Now: if the pid
            // file is missing, or the pid is not alive, we drop the
            // stale discovery file and fall through to the fallback so
            // the user gets the canonical "supervisor not running"
            // message instead of a confusing pipe-busy error.
            if discovery_pid_alive(&home_dir()) {
                return PathBuf::from(trimmed);
            }
            tracing::debug!(
                "supervisor.pipe discovery file references a dead daemon; \
                 ignoring and falling back to default ipc path"
            );
            // Best-effort cleanup so the next CLI invocation doesn't
            // hit the same stale read.
            let _ = std::fs::remove_file(&disco);
        }
    }
    default_ipc_fallback()
}

/// Return true iff `~/.mneme/run/daemon.pid` exists, parses, and the
/// PID it points at is currently alive on the system. Used by
/// `default_ipc_path` (BUG-A4-012) to validate the discovery file
/// before trusting the pipe name written there.
fn discovery_pid_alive(home: &Path) -> bool {
    let pid_path = home.join(".mneme").join("run").join("daemon.pid");
    let raw = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let pid: u32 = match raw.trim().parse() {
        Ok(p) => p,
        Err(_) => return false,
    };
    pid_is_alive(pid)
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    // OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION on a dead pid
    // returns ERROR_INVALID_PARAMETER. We avoid taking a winapi
    // dependency just for this by shelling out to `tasklist` -- not
    // pretty but cheap (only on CLI startup) and avoids a new crate.
    use std::process::Command;
    let out = match Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // tasklist prints "INFO: No tasks are running..." on miss; otherwise
    // a row containing the PID. Cheap substring match is enough.
    stdout.contains(&format!(" {pid} ")) || stdout.contains(&format!("\t{pid}\t"))
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // kill(pid, 0) is the standard probe -- returns 0 if the process
    // exists and we have permission, ESRCH if not.
    let pid_i = pid as i32;
    // SAFETY: libc::kill with sig=0 is a pure liveness probe; no signal
    // is sent. We don't take a libc dependency just for this either --
    // /proc/<pid> existence is the cross-distro probe.
    std::path::Path::new(&format!("/proc/{pid_i}")).exists()
}

#[cfg(windows)]
fn default_ipc_fallback() -> PathBuf {
    PathBuf::from(r"\\.\pipe\mneme-supervisor")
}

#[cfg(unix)]
fn default_ipc_fallback() -> PathBuf {
    let mut base = home_dir();
    base.push(".mneme");
    base.push("supervisor.sock");
    base
}

fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("MNEME_HOME") {
        return PathBuf::from(h);
    }
    #[cfg(windows)]
    {
        if let Some(h) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(h);
        }
    }
    #[cfg(unix)]
    {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h);
        }
    }
    PathBuf::from(".")
}

/// A10-010 (2026-05-04): pure tokio-runtime worker-thread sizing
/// extracted from `main()`. The supervisor is a long-lived control-
/// plane process - worker_threads should NOT scale with num_cpus on
/// a 32-core box. clamp(1, 4) keeps baseline RSS predictable while
/// covering typical IPC burst.
fn supervisor_worker_threads(cpus: usize) -> usize {
    cpus.clamp(1, 4)
}

#[cfg(test)]
mod supervisor_pool_size_tests {
    use super::supervisor_worker_threads;

    #[test]
    fn supervisor_worker_threads_floors_at_1() {
        assert_eq!(supervisor_worker_threads(0), 1);
        assert_eq!(supervisor_worker_threads(1), 1);
    }

    #[test]
    fn supervisor_worker_threads_caps_at_4_on_high_core_hosts() {
        assert_eq!(supervisor_worker_threads(4), 4);
        assert_eq!(supervisor_worker_threads(8), 4);
        assert_eq!(supervisor_worker_threads(16), 4);
        assert_eq!(supervisor_worker_threads(32), 4);
        assert_eq!(supervisor_worker_threads(64), 4);
    }

    #[test]
    fn supervisor_worker_threads_passes_through_2_and_3() {
        assert_eq!(supervisor_worker_threads(2), 2);
        assert_eq!(supervisor_worker_threads(3), 3);
    }
}
