//! Mneme Supervisor library.
//!
//! Re-exports every module so the binary (`main.rs`) and external integration
//! tests can use a stable surface. Nothing here performs side effects — see
//! [`run`] for the entry point that actually spawns workers.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod api_graph;
pub mod child;
pub mod config;
pub mod error;
pub mod health;
pub mod ipc;
pub mod job_queue;
pub mod job_queue_db;
pub mod log_ring;
pub mod manager;
pub mod service;
pub mod watchdog;
pub mod watcher;
pub mod ws;

/// K10 chaos-test-only fault-injection hooks. Compiled out of release
/// binaries by default — the entire module is gated behind `#[cfg]` so
/// the panic call site doesn't even exist in user-facing builds.
///
/// Public-but-feature-gated so the daemon binary's `Start` arm can
/// arm the counter, and the manager's dispatcher can decrement it.
///
/// Gated on `feature = "test-hooks"` only (not `cfg(test)`) because the
/// daemon bin in this same package consumes this lib as a dependency —
/// and `cfg(test)` does not propagate from the lib's tests to the bin's
/// view of the lib. Use `cargo test --features test-hooks` to enable.
#[cfg(feature = "test-hooks")]
pub mod test_hooks;

#[cfg(test)]
mod tests;

pub use child::{ChildHandle, ChildSpec, ChildStatus, RestartStrategy};
pub use config::{RestartPolicy, SupervisorConfig};
pub use error::SupervisorError;
pub use health::{HealthServer, SlaSnapshot};
pub use ipc::{ControlCommand, ControlResponse, IpcServer};
pub use job_queue::{JobQueue, JobQueueSnapshot};
pub use job_queue_db::{DurableJobQueue, RecoveredJob};
pub use log_ring::{LogEntry, LogLevel, LogRing};
pub use manager::ChildManager;
pub use watchdog::Watchdog;
pub use watcher::{run_watcher, WatcherStats, WatcherStatsHandle, DEFAULT_DEBOUNCE};

use std::sync::Arc;
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// Top-level supervisor result alias.
pub type Result<T> = std::result::Result<T, SupervisorError>;

/// Resolve the path to the durable job queue database.
///
/// Default: `<mneme-root>/run/jobs.db`. Override with the
/// `MNEME_JOBS_DB` env var (used by the supervisor's integration
/// tests so a test run never collides with the real install).
///
/// HOME-bypass-supervisor-jobs fix: the default routes through
/// `PathManager::default_root()` so `MNEME_HOME` is honored. This
/// matches the existing `logs_dir()` pattern just below.
fn jobs_db_path() -> std::path::PathBuf {
    if let Ok(v) = std::env::var("MNEME_JOBS_DB") {
        return std::path::PathBuf::from(v);
    }
    common::paths::PathManager::default_root()
        .root()
        .join("run")
        .join("jobs.db")
}

/// Resolve the directory that holds rotated supervisor log files.
///
/// Default: `<MNEME_HOME>/logs/`, falling back through the same chain
/// `mneme_common::PathManager::default_root()` uses (`MNEME_HOME` env,
/// then `~/.mneme`, then OS default). Tests redirect via
/// `MNEME_HOME=<tempdir>` so a real install on the host machine is
/// untouched. B-005 fix.
pub fn logs_dir() -> std::path::PathBuf {
    common::paths::PathManager::default_root()
        .root()
        .join("logs")
}

/// Canonical absolute path of the active supervisor log file.
///
/// `tracing_appender::rolling` writes one file per day with a date
/// suffix (e.g. `supervisor.log.2026-04-27`) PLUS a stable suffix-less
/// "current" file in the same directory; both forms are tailed by
/// [`tail_supervisor_log`]. This helper returns the suffix-less canonical
/// name so consumers (`/health`, `mneme daemon logs`, integration tests,
/// docs) can refer to a single path.
pub fn supervisor_log_path() -> std::path::PathBuf {
    logs_dir().join("supervisor.log")
}

/// Create the supervisor logs directory if it doesn't already exist.
/// Idempotent -- calling on an existing dir is a no-op. Returns the
/// resolved directory on success so callers (e.g. `init_tracing` in the
/// daemon binary) don't have to recompute it.
///
/// B-005 fix: the install / first-run path was producing supervisors that
/// only logged to stdout, so post-install probes for `~/.mneme/logs/`
/// always came up empty. The supervisor now creates this dir on every
/// boot.
///
/// BUG-A4-016 fix (2026-05-04): also reap files older than 7 days at
/// boot. `tracing_appender::rolling::DAILY`'s `max_log_files(7)` only
/// reaps files written by *this* appender instance -- a supervisor
/// restart starts a new appender that has no memory of prior days'
/// files, so on a daily-restart machine `~/.mneme/logs/` accreted
/// `supervisor.log.YYYY-MM-DD` files indefinitely. We now make the
/// retention horizon authoritative by sweeping it ourselves on every
/// boot.
pub fn ensure_logs_dir() -> std::io::Result<std::path::PathBuf> {
    let dir = logs_dir();
    std::fs::create_dir_all(&dir)?;
    reap_old_log_files(&dir, std::time::Duration::from_secs(7 * 24 * 60 * 60));
    Ok(dir)
}

/// BUG-A4-016 helper: delete `supervisor.log.*` files in `dir` whose
/// last-modified mtime is older than `max_age`. Best-effort: errors
/// reading the dir or stat-ing entries are logged at debug level and
/// otherwise ignored. We never delete the suffix-less `supervisor.log`
/// (the live appender's "current" handle) regardless of age.
fn reap_old_log_files(dir: &std::path::Path, max_age: std::time::Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %e, "log reap: read_dir failed");
            return;
        }
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        // Only reap rotated supervisor.log.* files. Leave the live
        // file and any unrelated artifacts alone.
        if !name.starts_with("supervisor.log.") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = match now.duration_since(modified) {
            Ok(d) => d,
            Err(_) => continue, // mtime in the future -- skip
        };
        if age > max_age {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::debug!(error = %e, file = %path.display(), "log reap: remove failed");
            } else {
                tracing::debug!(file = %path.display(), "log reap: removed stale log");
            }
        }
    }
}

/// Tail the last `n` lines from `~/.mneme/logs/supervisor.log` plus any
/// rotated daily files. Returns oldest-first so the caller can render
/// chronologically. If no log files exist (fresh install with no daemon
/// boot yet), returns an empty vector — never errors.
///
/// `tracing_appender::rolling::daily` rotates by appending the date to
/// the file stem (`supervisor.log.YYYY-MM-DD`); the suffix-less file is
/// the symlink-style "current" handle. We read both shapes and then take
/// the last `n` lines from the merged output. Used by:
///   * `mneme daemon logs` when the in-memory ring has rolled over
///   * Integration tests that exercise the file appender
pub fn tail_supervisor_log(n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let dir = logs_dir();
    if !dir.exists() {
        return Vec::new();
    }

    // Collect every supervisor.log* file, sort by mtime (oldest first), then
    // read in order. Keeps memory bounded by streaming line-by-line.
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            // Match both `supervisor.log` and `supervisor.log.YYYY-MM-DD`.
            if name == "supervisor.log" || name.starts_with("supervisor.log.") {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                files.push((path, mtime));
            }
        }
    }
    files.sort_by_key(|(_, t)| *t);

    let mut all_lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for (path, _) in files {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            for line in contents.lines() {
                all_lines.push_back(line.to_string());
                if all_lines.len() > n {
                    all_lines.pop_front();
                }
            }
        }
    }
    all_lines.into_iter().collect()
}

/// Boot the supervisor. Spawns the [`ChildManager`], [`Watchdog`],
/// [`HealthServer`], and [`IpcServer`], then awaits a shutdown signal
/// (Ctrl+C, SIGTERM, or an IPC `Stop` command).
pub async fn run(config: SupervisorConfig) -> Result<()> {
    // B-005 fix: ensure `~/.mneme/logs/` exists BEFORE any logging
    // statement that would otherwise be silently dropped because the
    // tracing-appender file writer hasn't been able to create the file.
    // `init_tracing` (in main.rs) already calls this — we call it again
    // here as a belt-and-suspenders so anyone embedding `run` directly
    // (integration tests, future daemon harnesses) gets the same
    // guarantee. Idempotent.
    if let Err(e) = ensure_logs_dir() {
        // Non-fatal: a read-only filesystem still lets the supervisor
        // boot, just without persistent logs. Surface a warning.
        tracing::warn!(error = %e, dir = %logs_dir().display(), "failed to ensure logs dir");
    }

    info!(
        version = env!("CARGO_PKG_VERSION"),
        children = config.children.len(),
        ipc = %config.ipc_socket_path.display(),
        log_file = %supervisor_log_path().display(),
        "supervisor starting"
    );

    // Advertise the PID-scoped IPC pipe path so CLI clients can discover it
    // (Windows named pipes are PID-unique to avoid "Access denied" zombies).
    // Also drop a true PID file at `~/.mneme/run/daemon.pid` so external
    // tooling (raw `mneme-daemon status`, init scripts, doctor checks) can
    // tell whether a supervisor is alive without having to round-trip IPC.
    // Bug G-2 (2026-05-01): surface failed PID/pipe-discovery writes.
    // The CLI relies on `~/.mneme/supervisor.pipe` for daemon discovery
    // and on `~/.mneme/run/daemon.pid` for raw process-status checks.
    // Previously these `let _ =` calls swallowed every failure mode
    // (missing dir, permission denied, full disk) so the daemon would
    // come up "successfully" but the CLI would hang at the IPC connect
    // forever with no diagnostic. Now we log every failure at warn level
    // so the supervisor.log makes the root cause visible.
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        let mneme_root = std::path::Path::new(&home).join(".mneme");

        let disco = mneme_root.join("supervisor.pipe");
        if let Some(parent) = disco.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(error = %e, parent = %parent.display(), "failed to create discovery dir; CLI may not find daemon");
            }
        }
        if let Err(e) = std::fs::write(&disco, config.ipc_socket_path.to_string_lossy().as_bytes())
        {
            warn!(
                error = %e,
                discovery = %disco.display(),
                "failed to write supervisor.pipe discovery file; CLI auto-spawn or fresh CLI commands may fail to find this daemon"
            );
        }

        let run_dir = mneme_root.join("run");
        if let Err(e) = std::fs::create_dir_all(&run_dir) {
            warn!(error = %e, run_dir = %run_dir.display(), "failed to create run dir; daemon.pid file write will fail next");
        }
        let pid_file = run_dir.join("daemon.pid");
        if let Err(e) = std::fs::write(&pid_file, std::process::id().to_string().as_bytes()) {
            warn!(
                error = %e,
                pid_file = %pid_file.display(),
                "failed to write daemon.pid; external tooling (raw status checks, init scripts) won't see this daemon"
            );
        } else {
            info!(
                pid = std::process::id(),
                pid_file = %pid_file.display(),
                "wrote supervisor PID file"
            );
        }
    } else {
        warn!("could not resolve home dir (USERPROFILE/HOME unset); skipping discovery + PID file writes — CLI commands will not find this daemon");
    }

    let log_ring = Arc::new(LogRing::new(10_000));
    let manager = Arc::new(ChildManager::new(config.clone(), log_ring.clone()));
    let watchdog = Arc::new(Watchdog::new(manager.clone(), config.health_check_interval));
    let shutdown = Arc::new(Notify::new());

    // 0a. Attach the job queue BEFORE any child spawns, so that a
    // worker that dies during startup can still have its in-flight
    // (empty) queue snapshot recorded without panicking.
    //
    // L5 (v0.3.0): durable queue at `~/.mneme/run/jobs.db`. Every
    // state transition is mirrored to disk so a supervisor crash
    // doesn't lose queued or in-flight work. On boot we
    // `recover_from_disk` — see `JobQueue::recover_from_disk`.
    let jobs_db_path = jobs_db_path();
    let job_queue = match DurableJobQueue::open(&jobs_db_path) {
        Ok(durable) => match JobQueue::with_durable(16 * 1024, Arc::new(durable)) {
            Ok(q) => Arc::new(q),
            Err(e) => {
                error!(
                    error = %e,
                    "failed to bind durable job queue; falling back to in-memory"
                );
                Arc::new(JobQueue::new(16 * 1024))
            }
        },
        Err(e) => {
            error!(
                path = %jobs_db_path.display(),
                error = %e,
                "could not open durable job queue; running in-memory only"
            );
            Arc::new(JobQueue::new(16 * 1024))
        }
    };
    match job_queue.recover_from_disk() {
        Ok(0) => {}
        Ok(n) => info!(recovered = n, "job queue: re-loaded jobs from disk"),
        Err(e) => tracing::warn!(error = %e, "job queue: recovery failed; new submits only"),
    }
    manager.attach_job_queue(job_queue.clone()).await;

    // 0. Start the restart-request processor BEFORE any child is spawned
    //    so a child that crashes during spawn_all() is still eligible for
    //    auto-restart. The receiver is taken exactly once.
    let restart_handle = if let Some(rx) = manager.take_restart_rx().await {
        let mgr = manager.clone();
        Some(tokio::spawn(async move { mgr.run_restart_loop(rx).await }))
    } else {
        None
    };

    // 0b. Bug I defensive fix: probe each unique worker exe for its
    // `--version` output and fail fast on a mismatch. Catches the
    // v0.3.0/v0.3.2 mixed-binary scenario from postmortem §17 (and any
    // future packaging regression of the same shape) before workers
    // crash-loop with an opaque `STATUS_CONTROL_C_EXIT` (-1073741510 on
    // Windows). The probe is best-effort: workers that don't yet
    // support `--version` are skipped with a `warn!` line. See
    // `manager::probe_worker_versions` for the full contract.
    manager::probe_worker_versions(&config.children, env!("CARGO_PKG_VERSION"))?;

    // 1. Spawn every configured child.
    manager.spawn_all().await?;

    // 2. Start the watchdog loop.
    let wd_handle = {
        let wd = watchdog.clone();
        let sd = shutdown.clone();
        tokio::spawn(async move { wd.run(sd).await })
    };

    // 3. Start the SLA dashboard HTTP server (localhost:7777/health).
    //
    // 4.2 livebus wiring: construct an in-process `EventBus` and the
    // matching `SubscriberManager` here so the production daemon's
    // `/ws` upgrade handler attaches to a real bus instead of always
    // returning the polite 503 error frame. Workers (the watcher,
    // future drift detector, etc.) push events to `bus.publish(...)`
    // and connected WebSocket clients receive them through `mgr`.
    let livebus_bus = livebus::EventBus::new();
    let livebus_mgr = livebus::SubscriberManager::new(livebus_bus.clone());
    info!(
        capacity = livebus_bus.published_count(),
        "livebus online (in-process EventBus + SubscriberManager)"
    );
    // C2: thread the job queue into HealthServer so /health.queue_depth
    // reports a real value instead of always 0. The bus is also threaded
    // in so the /ws upgrade route attaches to a live subscriber manager.
    let health_server = HealthServer::new(manager.clone(), config.health_port)
        .with_livebus(livebus_mgr.clone())
        .with_job_queue(job_queue.clone());
    let health_handle = {
        let sd = shutdown.clone();
        tokio::spawn(async move { health_server.serve(sd).await })
    };

    // 3a. Bus-fan-out task: bridges raw bus events into the
    // SubscriberManager so connected `/ws` clients receive events
    // emitted by the watcher / job-completion paths. Without this
    // bridge `bus.publish(ev)` would only reach raw `subscribe_raw`
    // consumers — `/ws` clients register through the manager.
    let bus_bridge_handle = {
        let mgr = livebus_mgr.clone();
        let mut rx = livebus_bus.subscribe_raw();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            info!("livebus bus->manager bridge online");
            loop {
                tokio::select! {
                    _ = sd.notified() => {
                        info!("livebus bridge shutting down");
                        break;
                    }
                    maybe_ev = rx.recv() => {
                        match maybe_ev {
                            Ok(ev) => mgr.dispatch(&ev),
                            Err(_) => {
                                // Channel closed — bus dropped. Quit.
                                tracing::debug!("livebus bridge channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        })
    };

    // 3a-pre. Phase-A C1: per-worker RSS refresher. sysinfo's process
    // refresh is OS-syscall-heavy (PEB walk on Windows, /proc on Linux)
    // so we run it on a 5-second cadence inside `spawn_blocking` from
    // `ChildManager::refresh_rss_samples`. /health reports the most
    // recent sample; before the first refresh fires, `rss_mb` is `0`,
    // matching the prior schema.
    let rss_handle = {
        let mgr = manager.clone();
        let sd = shutdown.clone();
        tokio::spawn(async move { run_rss_refresher(mgr, sd).await })
    };

    // 3a-pre-2. Bug I defensive fix: per-worker crash-loop recovery
    // logger. Same 5s cadence as the RSS refresher because the cost is
    // identical (a single read-lock walk over the live handle map plus
    // a per-handle write lock only on children that actually meet the
    // recovery threshold). One-shot per worker per recovery cycle —
    // see `ChildManager::check_recovery_logs` for the full contract.
    let recovery_handle = {
        let mgr = manager.clone();
        let sd = shutdown.clone();
        tokio::spawn(async move { run_recovery_logger(mgr, sd).await })
    };

    // 3a. Router task: drains the job queue and dispatches each job to
    // the matching worker pool. Runs in its own task so the IPC server
    // never blocks on stdin writes and so router panics cannot take
    // down the control plane.
    let router_handle = {
        let mgr = manager.clone();
        let queue = job_queue.clone();
        let sd = shutdown.clone();
        tokio::spawn(async move { run_router(mgr, queue, sd).await })
    };

    // 4. Start the IPC control plane (Unix socket / Windows named pipe).
    let ipc = IpcServer::new(manager.clone(), config.ipc_socket_path.clone());
    let ipc_handle = {
        let sd = shutdown.clone();
        tokio::spawn(async move { ipc.serve(sd).await })
    };

    // 5. Wait for OS signal OR an IPC-triggered shutdown.
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(e) = result {
                error!(error = %e, "ctrl_c handler failed");
            }
            info!("ctrl-c received, initiating graceful shutdown");
        }
        _ = shutdown.notified() => {
            info!("shutdown notified by control plane");
        }
    }

    shutdown.notify_waiters();

    // 6. Stop children.
    manager.shutdown_all().await?;

    // 7. Join background tasks. Errors are logged, never panicked.
    if let Err(e) = wd_handle.await {
        error!(error = %e, "watchdog task join error");
    }
    if let Err(e) = health_handle.await {
        error!(error = %e, "health task join error");
    }
    if let Err(e) = ipc_handle.await {
        error!(error = %e, "ipc task join error");
    }
    // Bug G-11 (2026-05-01): on shutdown, the previous code did
    //     handle.abort();
    //     let _ = handle.await;
    // for each background task. The `let _` swallowed every join
    // error including any panic that happened in the task before
    // shutdown. We now log JoinError + panic-payload so a task that
    // crashed mid-flight is visible in supervisor.log instead of
    // disappearing into "supervisor stopped cleanly".
    // Use std::result::Result explicitly because this crate's prelude
    // defines `Result<T> = std::result::Result<T, SupervisorError>` —
    // a single-arg alias that doesn't fit a tokio JoinError second arg.
    fn log_join(name: &str, res: std::result::Result<(), tokio::task::JoinError>) {
        match res {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {
                // Expected — we just `abort()`ed it.
            }
            Err(e) => {
                error!(task = %name, error = %e, "background task join error on shutdown (panic before abort?)");
            }
        }
    }
    router_handle.abort();
    log_join("router", router_handle.await);
    rss_handle.abort();
    log_join("rss_refresher", rss_handle.await);
    recovery_handle.abort();
    log_join("crash_recovery_logger", recovery_handle.await);
    bus_bridge_handle.abort();
    log_join("livebus_bridge", bus_bridge_handle.await);
    if let Some(h) = restart_handle {
        h.abort();
        log_join("restart_loop", h.await);
    }

    // Best-effort cleanup of the discovery + PID files. Stale entries are
    // harmless on the next boot (the PID-scoped pipe name is regenerated)
    // but we remove them so external tooling sees an honest "down" state.
    // Bug G-11 (2026-05-01): cleanup of stale discovery + PID file.
    // Failures here are TRULY best-effort (the next boot will
    // overwrite them anyway), but we still log them at debug so a
    // sysadmin troubleshooting "stale supervisor.pipe survives
    // restarts" has a paper trail.
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        let mneme_root = std::path::Path::new(&home).join(".mneme");
        let pipe_path = mneme_root.join("supervisor.pipe");
        if let Err(e) = std::fs::remove_file(&pipe_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(error = %e, path = %pipe_path.display(), "failed to remove stale supervisor.pipe on shutdown (next boot will overwrite)");
            }
        }
        let pid_path = mneme_root.join("run").join("daemon.pid");
        if let Err(e) = std::fs::remove_file(&pid_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(error = %e, path = %pid_path.display(), "failed to remove stale daemon.pid on shutdown (next boot will overwrite)");
            }
        }
    }

    info!("supervisor stopped cleanly");
    Ok(())
}

/// Refresh per-worker RSS samples on a 5-second cadence. Phase-A C1.
///
/// The actual sysinfo refresh runs inside `spawn_blocking` (see
/// [`ChildManager::refresh_rss_samples`]), so this loop is just a
/// timer + shutdown plumbing. We deliberately pick 5 seconds: too
/// frequent and Windows' PEB walk shows up in supervisor CPU
/// microbenchmarks; too slow and a worker that allocates a burst gets
/// missed by an /health caller polling every second. 5 s lines up with
/// `health::SNAPSHOT_TTL` (1 s) × 5 — every fifth burst-poll is
/// guaranteed to see fresh memory data.
async fn run_rss_refresher(manager: Arc<ChildManager>, shutdown: Arc<Notify>) {
    info!("rss refresher online");
    let interval = std::time::Duration::from_secs(5);
    // Kick once immediately so /health doesn't show 0 for the entire
    // first 5 seconds after boot.
    manager.refresh_rss_samples().await;
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                info!("rss refresher shutting down");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                manager.refresh_rss_samples().await;
            }
        }
    }
}

/// Periodic crash-loop recovery logger. Bug I defensive fix.
///
/// Polls [`ChildManager::check_recovery_logs`] every 5 seconds; that
/// method emits one `info!` line per worker that has crash-looped
/// (`restart_count >= 3`) and then stabilised (`current_uptime >= 60s`).
/// The per-handle one-shot flag inside `ChildHandle` ensures the line
/// fires exactly once per recovery cycle — `record_restart` clears
/// the flag the next time the worker crashes.
///
/// Cadence reuses the RSS refresher's 5s tick because the cost profile
/// is identical (read-lock walk over the handle map + cheap per-handle
/// uptime read), and aligning the two cadences keeps supervisor wakeup
/// patterns predictable for power-management observers.
async fn run_recovery_logger(manager: Arc<ChildManager>, shutdown: Arc<Notify>) {
    info!("crash-loop recovery logger online");
    let interval = std::time::Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                info!("crash-loop recovery logger shutting down");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                let _emitted = manager.check_recovery_logs().await;
            }
        }
    }
}

/// Drain the shared [`JobQueue`] forever, dispatching each job to the
/// worker pool identified by its `pool_prefix()`. The router runs in
/// its own tokio task. Design notes:
///
/// * Pulls at most one pending job per iteration. Small by design —
///   `dispatch_to_pool` has to grab a write lock on the target worker's
///   stdin handle and we want to give other workers a fair chance.
/// * Uses the queue's `Notify` to wake on submits without busy-polling.
/// * On dispatch failure (e.g. no worker in the pool is running yet),
///   puts the job back on the front of the queue and sleeps 100 ms so
///   the supervisor can (re)spawn the missing pool.
/// * Honours the shared `shutdown` notify so that Ctrl-C leaves the
///   queue quiescent.
async fn run_router(manager: Arc<ChildManager>, queue: Arc<JobQueue>, shutdown: Arc<Notify>) {
    info!("router task online");
    let waker = queue.router_waker();
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                info!("router shutting down");
                break;
            }
            _ = waker.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                // Periodic wake-up covers the edge case where a pool
                // came online AFTER a job was queued and returned to
                // pending with no notification.
            }
        }

        // Drain everything we can in a burst. Keeps per-iteration
        // overhead low when the CLI submits a Build of 10k files.
        while let Some((id, job)) = queue.next_pending() {
            let prefix = job.pool_prefix();
            // Translate our Job enum into the worker-native wire format.
            // Each worker's stdin reader predates v0.3 and expects a
            // flat per-worker JSON shape — we keep it that way so
            // routing is additive (no worker behaviour change). If the
            // translation fails (e.g. a file we can't read), we fail
            // the job rather than crash the router.
            let line = match encode_for_worker(id, &job) {
                Ok(s) => s,
                Err(e) => {
                    error!(%id, kind = job.kind_label(), error = %e, "router: encode failed");
                    queue.complete(
                        id,
                        common::jobs::JobOutcome::Err {
                            message: format!("router encode: {e}"),
                            duration_ms: 0,
                            stats: serde_json::Value::Null,
                        },
                    );
                    continue;
                }
            };
            match manager.dispatch_to_pool(prefix, &line).await {
                Ok(worker) => {
                    queue.mark_assigned(id, worker.clone());
                    // Phase-A C5: bump the dispatched-job counter so
                    // /health surfaces forward progress even if the
                    // worker never reports `WorkerCompleteJob` back.
                    manager.record_job_dispatch(&worker).await;
                    tracing::debug!(
                        %id,
                        kind = job.kind_label(),
                        worker = %worker,
                        "router: dispatched job"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        %id,
                        kind = job.kind_label(),
                        prefix,
                        error = %e,
                        "router: no worker available; re-queuing"
                    );
                    queue.return_pending(id);
                    // Pool isn't ready yet (worker not spawned, or all
                    // stdins closed). Back off briefly so we don't spin.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    break;
                }
            }
        }
    }
    info!("router task offline");
}

/// Translate a v0.3 [`Job`] into the worker-native JSON line shape.
///
/// Each worker predates the `Job` enum; they deserialize their own
/// historical wire formats. Rather than break those, the router emits
/// per-worker JSON so the handoff is fully additive. On the worker side
/// the only thing we need to add is an "emit result back to supervisor"
/// path — tracked as a v0.4 follow-up (for now the parse-worker still
/// writes to stdout, which the supervisor's `monitor_child` captures as
/// log lines).
fn encode_for_worker(
    id: common::jobs::JobId,
    job: &common::jobs::Job,
) -> std::result::Result<String, String> {
    use common::jobs::Job;
    match job {
        Job::Parse { file_path, .. } => {
            // parse-worker's JobWire expects {file_path, language, content,
            // prev_tree_id?, job_id}. Read the file synchronously here —
            // the router task is dedicated and a blocking read is cheap
            // compared to the downstream tree-sitter parse.
            let content = std::fs::read_to_string(file_path)
                .map_err(|e| format!("read {}: {e}", file_path.display()))?;
            let language = infer_language_tag(file_path)
                .ok_or_else(|| format!("no language for {}", file_path.display()))?;
            Ok(serde_json::json!({
                "job_id": id.0,
                "file_path": file_path,
                "language": language,
                "content": content,
            })
            .to_string())
        }
        Job::Scan {
            file_path,
            ast_id,
            shard_root,
        } => {
            let content = std::fs::read_to_string(file_path)
                .map_err(|e| format!("read {}: {e}", file_path.display()))?;
            // B11.7 (v0.3.2): pass shard_root through so the scanner
            // worker can persist findings DIRECTLY to the per-project
            // findings.db (B12 streaming guarantee). Without it the
            // worker would only emit findings via the batched stdout
            // pipe, which the supervisor's `monitor_child` doesn't
            // consume — every finding would be lost.
            Ok(serde_json::json!({
                "job_id": id.0,
                "file_path": file_path,
                "content": content,
                "ast_id": ast_id,
                "scanner_filter": [],
                "shard_root": shard_root,
            })
            .to_string())
        }
        Job::Embed {
            node_qualified,
            text,
            ..
        } => Ok(serde_json::json!({
            "job_id": id.0,
            "node_qualified": node_qualified,
            "text": text,
        })
        .to_string()),
        Job::Ingest { md_file, .. } => Ok(serde_json::json!({
            "job_id": id.0,
            "md_file": md_file,
        })
        .to_string()),
    }
}

/// Best-effort language tag for the parse-worker's JobWire.
///
/// Kept as a tiny hardcoded map so the supervisor doesn't depend on the
/// parsers crate (which pulls in tree-sitter). Extensions we don't know
/// map to `None` and the job fails at encode time; the CLI's walker
/// already filters most of them out before submit.
fn infer_language_tag(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|s| s.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "hpp" | "hh" | "cxx" => Some("cpp"),
        "md" | "markdown" => Some("markdown"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        _ => None,
    }
}
