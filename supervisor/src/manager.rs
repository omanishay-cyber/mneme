//! Child process manager.
//!
//! Owns every [`ChildHandle`], spawns workers via tokio's [`tokio::process`]
//! API, watches each one for exit, applies the exponential back-off restart
//! policy, and pipes stdout/stderr into the shared [`LogRing`].

use crate::child::{ChildHandle, ChildSpec, ChildStatus, RestartStrategy};
use crate::config::SupervisorConfig;
use crate::error::SupervisorError;
use crate::job_queue::JobQueue;
use crate::log_ring::LogRing;
use chrono::Utc;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

// Bug J (postmortem §12.1): the restart-request channel is now
// unbounded. The previous bounded design (`mpsc::channel(cap)` with
// `try_send`) silently dropped requests on `Full` — observed in
// production at 11 dropped restarts in 5s on the AWS install test. The
// CHANGELOG v0.2.0 (line 709) had already promised
// `mpsc::UnboundedChannel<RestartRequest>` — this commit honours that
// contract. Memory pressure is bounded externally by the
// max_restarts_per_window budget enforced inside `respawn_one`, so the
// channel itself never needs back-pressure. The `restart_channel_cap`
// helper has been removed accordingly.

/// Windows process-creation flags used when the supervisor spawns a
/// worker via [`ChildManager::spawn_os_process`]. Bug D (postmortem
/// §3.D + §12.5): the v0.3.0/v0.3.2 install storm flashed 22 console
/// windows on every supervisor boot because the worker spawn path was
/// missing `CREATE_NO_WINDOW`. The composition mirrors the uninstall
/// self-delete shim (`cli/src/commands/uninstall.rs:448-449`) and adds
/// `CREATE_BREAKAWAY_FROM_JOB` so a Job-owned daemon does not pull
/// every worker into the same Job object.
///
/// - `DETACHED_PROCESS` (0x00000008): no console handle inheritance.
/// - `CREATE_NEW_PROCESS_GROUP` (0x00000200): own Ctrl-Break group.
/// - `CREATE_BREAKAWAY_FROM_JOB` (0x01000000): escape parent Job.
/// - `CREATE_NO_WINDOW` (0x08000000): no console window ever.
///
/// Exposed at `pub(crate)` so the supervisor's own test module can
/// assert the exact bit composition without re-deriving the magic
/// numbers.
#[cfg(windows)]
pub(crate) fn windows_worker_spawn_flags() -> u32 {
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW
}

/// Fallback flag composition without `CREATE_BREAKAWAY_FROM_JOB` for
/// restricted environments (CI sandboxes, test runners) where the
/// parent Job object denies breakaway with `ERROR_ACCESS_DENIED`. The
/// load-bearing console-window suppression bits (DETACHED_PROCESS +
/// CREATE_NEW_PROCESS_GROUP + CREATE_NO_WINDOW) are preserved — only
/// the Job-escape flag is dropped.
#[cfg(windows)]
fn windows_worker_spawn_flags_no_breakaway() -> u32 {
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
}

/// Cached snapshot returned from [`ChildManager::snapshot`] for up to
/// [`SNAPSHOT_TTL`]. NEW-015: amortises the per-child Mutex lock storm
/// when callers (CLI status, /metrics, /health) poll faster than the
/// underlying state actually changes.
const SNAPSHOT_TTL: Duration = Duration::from_secs(1);

/// Reason a monitor task queued a restart.
#[derive(Debug, Clone)]
pub(crate) struct RestartRequest {
    /// Child name.
    pub name: String,
    /// Exit code observed by the monitor.
    pub exit_code: i32,
    /// Time the exit was observed — drives the slow-restart telemetry
    /// the restart loop logs when the channel is back-pressured.
    /// REG-020: actively read in `respawn_one` to compute queue latency.
    pub queued_at: Instant,
}

/// Owns all running children and their restart state.
pub struct ChildManager {
    config: SupervisorConfig,
    log_ring: Arc<LogRing>,
    handles: RwLock<HashMap<String, Arc<Mutex<ChildHandle>>>>,
    monitors: Mutex<HashMap<String, JoinHandle<()>>>,
    shutdown_flag: Mutex<bool>,
    /// Sender used by each monitor task to queue a restart request. The
    /// receive end lives inside a separate task started by
    /// [`Self::start_restart_loop`] — this indirection is what breaks the
    /// `tokio::process::Child` Send-recursion cycle that forced v0.1 to
    /// ship with auto-restart disabled.
    ///
    /// Bug J: unbounded so the monitor task never silently drops a
    /// crash signal under load. `send` on an `UnboundedSender` returns
    /// `Err` only when the receiver has been dropped (channel closed),
    /// which means the supervisor itself is shutting down — the dropped
    /// path is observed via [`crate::child::ChildState::restart_dropped_count`]
    /// and surfaced through `mneme doctor`.
    restart_tx: mpsc::UnboundedSender<RestartRequest>,
    /// Receiver for [`RestartRequest`]s. Wrapped in a `Mutex<Option<…>>`
    /// to enforce a one-shot transfer — only one restart loop is allowed
    /// per manager. `take_restart_rx` returns `Some` exactly once; every
    /// subsequent caller gets `None` and is expected to treat that as a
    /// programming error (the supervisor only ever calls it once during
    /// boot).
    restart_rx: Mutex<Option<mpsc::UnboundedReceiver<RestartRequest>>>,
    /// Shared job queue (set via [`Self::attach_job_queue`]). The queue
    /// tracks CLI-submitted work items (`Job::Parse`, `Job::Scan`, …)
    /// that the router task drains by pushing JSON lines to worker
    /// stdin via [`Self::dispatch_to_pool`].
    job_queue: RwLock<Option<Arc<JobQueue>>>,
    /// Cached snapshot of every child handle. Refreshed at most once
    /// per [`SNAPSHOT_TTL`]. The `Mutex` serialises both the cache read
    /// and the underlying per-handle lock storm so a /metrics scrape +
    /// /health scrape + CLI status burst hitting in the same second
    /// produces ONE pass over the handle map, not three.
    snapshot_cache: Mutex<Option<(Instant, Vec<ChildSnapshot>)>>,
    /// BUG-A4-013 fix (2026-05-04): per-manager round-robin index for
    /// `dispatch_to_pool`. The doc comment claimed round-robin
    /// dispatch but the implementation always tried `parser-worker-0`
    /// first (alphabetical sort) -- so a wedged head-of-pool worker
    /// absorbed every dispatch attempt for STDIN_WRITE_TIMEOUT
    /// (10 s) before the router moved on. With 1100 in-flight files
    /// this was 11000 s of lost time per build. AtomicUsize so the
    /// router can mutate without taking a lock.
    round_robin_idx: std::sync::atomic::AtomicUsize,
}

impl ChildManager {
    /// Construct a manager from a fully-validated config.
    pub fn new(config: SupervisorConfig, log_ring: Arc<LogRing>) -> Self {
        // Bug J: unbounded channel — see module-level comment.
        let (restart_tx, restart_rx) = mpsc::unbounded_channel::<RestartRequest>();
        Self {
            config,
            log_ring,
            handles: RwLock::new(HashMap::new()),
            monitors: Mutex::new(HashMap::new()),
            shutdown_flag: Mutex::new(false),
            restart_tx,
            restart_rx: Mutex::new(Some(restart_rx)),
            job_queue: RwLock::new(None),
            snapshot_cache: Mutex::new(None),
            round_robin_idx: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Attach a shared [`JobQueue`]. Must be called once during
    /// supervisor boot BEFORE the first worker can crash, so requeue
    /// logic never misses an exit.
    pub async fn attach_job_queue(&self, queue: Arc<JobQueue>) {
        let mut g = self.job_queue.write().await;
        *g = Some(queue);
    }

    /// Borrow the attached job queue, if any.
    pub async fn job_queue(&self) -> Option<Arc<JobQueue>> {
        self.job_queue.read().await.clone()
    }

    /// Take ownership of the restart-request receiver.
    ///
    /// **One-shot contract**: the supervisor spawns exactly one restart
    /// loop per manager during boot. Calling this a second time returns
    /// `None` — callers MUST treat that as a programming error and
    /// surface it (e.g. by panicking or by failing supervisor boot).
    /// Silently ignoring a second `None` would leave the channel
    /// unconsumed and the restart pipeline dead. (NEW-012.)
    #[must_use = "the receiver must be passed to run_restart_loop or restarts will silently stop"]
    pub(crate) async fn take_restart_rx(&self) -> Option<mpsc::UnboundedReceiver<RestartRequest>> {
        let mut guard = self.restart_rx.lock().await;
        let taken = guard.take();
        if taken.is_none() {
            // NEW-012: a second caller is a programmer error. A silent
            // None would leave the channel unconsumed and the restart
            // pipeline dead. Emit a debug-level diagnostic so the bug is
            // surfaced in `tail -F` of the supervisor log even in
            // release builds (where the assertion would compile out).
            debug!("take_restart_rx called twice — programmer error or supervisor restart");
        }
        taken
    }

    /// Spawn every child listed in the config. A child whose binary is
    /// missing (file not found) is skipped with a warning — the daemon
    /// stays up with whatever workers actually exist. Other errors still
    /// propagate and abort startup.
    pub async fn spawn_all(self: &Arc<Self>) -> Result<(), SupervisorError> {
        let specs = self.config.children.clone();
        for spec in specs {
            match self.spawn_child(spec.clone()).await {
                Ok(()) => {}
                Err(SupervisorError::Spawn { name, source })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    tracing::warn!(
                        child = %name,
                        binary = %spec.command,
                        "binary missing — child skipped; daemon continuing"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Spawn a single child and start its monitor task.
    pub async fn spawn_child(self: &Arc<Self>, spec: ChildSpec) -> Result<(), SupervisorError> {
        let initial_backoff = self.config.default_restart_policy.initial_backoff;
        let name = spec.name.clone();

        // Insert (or refresh) the handle.
        {
            let mut guard = self.handles.write().await;
            guard.entry(name.clone()).or_insert_with(|| {
                Arc::new(Mutex::new(ChildHandle::new(spec.clone(), initial_backoff)))
            });
        }

        let handle_arc = {
            let guard = self.handles.read().await;
            guard
                .get(&name)
                .cloned()
                .expect("handle just inserted above")
        };

        let mut child = self.spawn_os_process(&spec).await?;
        let pid = child.id();
        // Capture stdin BEFORE moving the child into the monitor task.
        // This lets the manager dispatch worker jobs later without needing
        // a handle to the Child itself (which is !Send across awaits on
        // Windows named-pipe stdio handles).
        let stdin_handle = child.stdin.take().map(|s| Arc::new(Mutex::new(s)));

        // Move bookkeeping into the spawned task so the surrounding
        // future is Send (Child is Send but holding it across the
        // handle_arc.lock().await above made the future opaque to
        // the auto-trait checker).
        let me = Arc::clone(self);
        let handle_for_task = Arc::clone(&handle_arc);
        let task_name = name.clone();
        let task = tokio::spawn(async move {
            {
                let mut h = handle_for_task.lock().await;
                h.pid = pid;
                h.status = ChildStatus::Running;
                h.last_started_at = Some(Utc::now());
                h.last_started_instant = Some(Instant::now());
                h.last_heartbeat = Some(Instant::now());
                h.stdin = stdin_handle;
            }
            me.monitor_child(task_name, child, handle_for_task).await;
        });

        let mut mons = self.monitors.lock().await;
        mons.insert(spec.name.clone(), task);

        info!(child = %spec.name, pid = ?pid, "child spawned");
        Ok(())
    }

    async fn spawn_os_process(&self, spec: &ChildSpec) -> Result<Child, SupervisorError> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args);
        // I-19: default workers to non-ANSI structured output. Anything
        // the worker sets explicitly via `spec.env` still wins (a worker
        // that forces text output for debugging can override). Adding
        // this BEFORE the user-supplied env loop means user values take
        // precedence.
        cmd.env("MNEME_LOG_FORMAT", "json");
        // Kill common "force colour on" envs so child loggers don't
        // re-introduce ANSI escapes after we asked for JSON.
        cmd.env("NO_COLOR", "1");
        cmd.env_remove("CLICOLOR_FORCE");
        cmd.env_remove("FORCE_COLOR");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // Pipe stdin (don't close it). Workers like parse-worker read stdin
        // for jobs and would otherwise exit cleanly on EOF and be reaped
        // even though nothing crashed.
        cmd.stdin(Stdio::piped());
        cmd.kill_on_drop(true);

        // Bug D (postmortem §3.D + §12.5): suppress console-window
        // creation on Windows. Without these flags the supervisor's 22
        // workers each flash a cmd.exe window on boot — the "hydra
        // heads" storm. See [`windows_worker_spawn_flags`] for the
        // exact composition.
        #[cfg(windows)]
        {
            cmd.creation_flags(windows_worker_spawn_flags());
        }

        let spawned = cmd.spawn();

        // Bug D fallback: some restricted environments (CI sandboxes,
        // test runners under a Job object that disallows breakaway)
        // reject `CREATE_BREAKAWAY_FROM_JOB` with `ERROR_ACCESS_DENIED`
        // (Win32 code 5). When that specific failure mode is observed,
        // retry once without the breakaway flag — the supervisor still
        // gets DETACHED_PROCESS + CREATE_NEW_PROCESS_GROUP +
        // CREATE_NO_WINDOW, which is the load-bearing part of the fix.
        // Production daemons (interactive desktop session) are not in
        // such a Job, so the primary path succeeds.
        #[cfg(windows)]
        let spawned = match spawned {
            Ok(child) => Ok(child),
            Err(e) if e.raw_os_error() == Some(5) => {
                cmd.creation_flags(windows_worker_spawn_flags_no_breakaway());
                cmd.spawn()
            }
            Err(e) => Err(e),
        };

        spawned.map_err(|e| SupervisorError::Spawn {
            name: spec.name.clone(),
            source: e,
        })
    }

    async fn monitor_child(
        self: Arc<Self>,
        name: String,
        mut child: Child,
        handle: Arc<Mutex<ChildHandle>>,
    ) {
        // I-5 / NEW-008: track the stdout/stderr forwarder JoinHandles
        // on the ChildHandle so we can `.abort()` them when the child
        // restarts or the supervisor shuts down. Before this fix the
        // forwarders were detached `tokio::spawn` tasks that lived as
        // long as their pipe — on Windows that meant they routinely
        // outlived the dead child, accumulating per restart.
        //
        // We also abort any *previous* forwarders that may still be in
        // flight from an earlier life of this same child name (defence
        // in depth — `spawn_child` already aborts via abort_io_tasks
        // before re-entry, but a fast crash-restart-crash loop could in
        // theory race).
        {
            let mut h = handle.lock().await;
            h.abort_io_tasks();
        }

        if let Some(stdout) = child.stdout.take() {
            let ring = self.log_ring.clone();
            let n = name.clone();
            let task = tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    ring.push_raw(&n, &line);
                }
            });
            let mut h = handle.lock().await;
            h.stdout_task = Some(task);
        }
        if let Some(stderr) = child.stderr.take() {
            let ring = self.log_ring.clone();
            let n = name.clone();
            let task = tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    ring.push_raw(&n, &line);
                }
            });
            let mut h = handle.lock().await;
            h.stderr_task = Some(task);
        }

        // Block until the OS reports exit. Then explicitly drop the child
        // before any further awaits so its non-Send pieces (stdin/stdout
        // handles) don't poison the surrounding future.
        let exit_status = match child.wait().await {
            Ok(s) => s,
            Err(e) => {
                error!(child = %name, error = %e, "wait() failed");
                return;
            }
        };
        let exit_code = exit_status.code().unwrap_or(-1);
        drop(child);

        {
            let mut h = handle.lock().await;
            h.last_exit_code = Some(exit_code);
            if let Some(start) = h.last_started_instant {
                h.total_uptime = h.total_uptime.saturating_add(start.elapsed());
            }
            h.pid = None;
            h.stdin = None;
            // Phase-A C1: clear the stale RSS sample so /health reports
            // 0 for a worker that's between spawns rather than the last
            // known live memory size of the now-dead process.
            h.record_rss_bytes(None);
            // I-5 / NEW-008: tear down the IO forwarders now that the
            // pipes are closed. On Windows the readers can otherwise
            // sit forever waiting for a pipe-close notification that
            // never arrives.
            h.abort_io_tasks();
        }

        // If this worker had jobs in flight, push them back onto the
        // queue so the next worker in the pool picks them up. Skipping
        // this means a Parse/Scan/Embed job silently disappears on every
        // crash — the whole point of supervisor-mediated dispatch.
        if let Some(queue) = self.job_queue.read().await.clone() {
            let n = queue.requeue_worker(&name);
            if n > 0 {
                info!(child = %name, jobs = n, "requeued in-flight jobs after exit");
            }
        }

        // Honour a graceful supervisor shutdown.
        if *self.shutdown_flag.lock().await {
            let mut h = handle.lock().await;
            h.status = ChildStatus::Stopped;
            info!(child = %name, code = exit_code, "child stopped during shutdown");
            return;
        }

        let strategy = {
            let h = handle.lock().await;
            h.spec.restart
        };

        let should_restart = match strategy {
            RestartStrategy::Permanent => true,
            RestartStrategy::Transient => exit_code != 0,
            RestartStrategy::Temporary => false,
        };

        if !should_restart {
            let mut h = handle.lock().await;
            h.status = ChildStatus::Stopped;
            warn!(child = %name, code = exit_code, "child exited; restart strategy says no");
            return;
        }

        // Mark the child as Restarting and queue a request on the restart
        // channel. The dedicated restart loop (see `run_restart_loop`)
        // performs the actual respawn. This decouples the monitor task —
        // which still owns the dead `Child` handle until function return —
        // from the respawn code path that creates a NEW `Child`. The old
        // recursive `spawn_child` → `monitor_child` call stack forced the
        // compiler to prove the combined future was Send even though
        // Windows named-pipe stdio pieces make `Child` ambiguous across
        // awaits. Splitting via an mpsc boundary lets each side be Send
        // independently.
        {
            let mut h = handle.lock().await;
            h.status = ChildStatus::Restarting;
        }
        // Bug J: unbounded channel. `send` cannot fail on `Full` —
        // only on `Closed`, which happens when the receiver has been
        // dropped (supervisor shutting down). On `Closed` we log AND
        // increment `restart_dropped_count` (Bug L) so the dropped
        // request is observable via `mneme doctor` / Prometheus.
        let req = RestartRequest {
            name: name.clone(),
            exit_code,
            queued_at: Instant::now(),
        };
        if let Err(mpsc::error::SendError(_dropped)) = self.restart_tx.send(req) {
            error!(child = %name, "restart channel closed; cannot queue respawn");
            // Bug L: surface the dropped request via the per-child
            // gauge so `mneme doctor` and Prometheus scrapers see it.
            let mut h = handle.lock().await;
            h.restart_dropped_count = h.restart_dropped_count.saturating_add(1);
        } else {
            debug!(child = %name, exit_code, "restart request queued");
        }
    }

    /// Test-only entrypoint: pushes a `RestartRequest` onto the
    /// supervisor's restart channel. Production code paths queue
    /// requests inline in `monitor_child` after observing a child
    /// exit; tests need a way to drive the channel without spawning
    /// real workers. Returns `Ok(())` on a successful send and
    /// `Err(SendError<RestartRequest>)` if the receiver has been
    /// dropped (Bug L's "Closed" path).
    #[doc(hidden)]
    #[allow(dead_code)]
    pub(crate) fn enqueue_restart_request_for_test(
        &self,
        req: RestartRequest,
    ) -> Result<(), mpsc::error::SendError<RestartRequest>> {
        self.restart_tx.send(req)
    }

    /// Test-only entrypoint: register a `ChildHandle` directly on the
    /// manager without spawning a real OS process. Used by Bug L's
    /// dropped-count test which needs a child to attribute the
    /// `restart_dropped_count` increment to.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub(crate) async fn register_handle_for_test(&self, handle: ChildHandle) {
        let name = handle.spec.name.clone();
        let mut g = self.handles.write().await;
        g.insert(name, Arc::new(Mutex::new(handle)));
    }

    /// Test-only entrypoint: simulate the dropped-restart path that
    /// fires when the restart channel is closed (receiver dropped).
    /// Bumps `restart_dropped_count` on the named child the same way
    /// `monitor_child` does after `SendError`. The test for Bug L
    /// drives this directly because it cannot spawn a real worker.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub(crate) async fn simulate_dropped_restart_for_test(&self, name: &str) {
        let g = self.handles.read().await;
        if let Some(h) = g.get(name) {
            let mut handle = h.lock().await;
            handle.restart_dropped_count = handle.restart_dropped_count.saturating_add(1);
        }
    }

    /// Process queued restart requests forever. Owned by a single task.
    ///
    /// This loop pulls `RestartRequest`s off the channel filled by
    /// [`Self::monitor_child`] and performs the respawn with exponential
    /// backoff + restart-budget enforcement. Because it runs in its own
    /// tokio task with a fresh stack, the opaque-future Send-inference
    /// cycle that blocked v0.1 is avoided structurally.
    pub(crate) async fn run_restart_loop(
        self: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<RestartRequest>,
    ) {
        info!("restart loop online");
        while let Some(req) = rx.recv().await {
            if *self.shutdown_flag.lock().await {
                // BUG-A4-011 fix (2026-05-04): bump
                // `restart_dropped_count` for the affected child even
                // when we discard the request because the supervisor
                // is shutting down. Bug L's gauge previously only
                // incremented on the *send* side (channel closed) so
                // crashes that arrived during the shutdown window were
                // silently lost from the diagnostic surface --
                // `mneme doctor` would under-report restart drops
                // exactly when the system was most stressed.
                if let Some(h) = self.handle_for(&req.name).await {
                    let mut handle = h.lock().await;
                    handle.restart_dropped_count =
                        handle.restart_dropped_count.saturating_add(1);
                }
                debug!(child = %req.name, "shutdown in progress; ignoring restart request");
                continue;
            }
            // REG-020: surface queue-latency telemetry. If a restart
            // sat in the channel for more than ~250ms the supervisor is
            // likely back-pressured (flapping pool) — operators want to
            // see that before the budget kicks in and degrades the child.
            let queue_latency = req.queued_at.elapsed();
            if queue_latency > Duration::from_millis(250) {
                warn!(
                    child = %req.name,
                    queue_latency_ms = queue_latency.as_millis() as u64,
                    "restart request waited unusually long in channel"
                );
            }
            if let Err(e) = self.respawn_one(&req).await {
                error!(child = %req.name, error = %e, "restart failed");
            }
        }
        info!("restart loop offline");
    }

    async fn respawn_one(self: &Arc<Self>, req: &RestartRequest) -> Result<(), SupervisorError> {
        let policy = self.config.default_restart_policy.clone();
        let handle = match self.handle_for(&req.name).await {
            Some(h) => h,
            None => {
                warn!(child = %req.name, "restart for unknown child; dropping");
                return Ok(());
            }
        };

        // Compute backoff + enforce budget under the handle lock.
        let (delay, spec) = {
            let mut h = handle.lock().await;
            h.record_restart(policy.budget_window);
            let in_window = h.restarts_in_window(policy.budget_window);
            if in_window > policy.max_restarts_per_window {
                h.status = ChildStatus::Degraded;
                warn!(
                    child = %req.name,
                    restarts = in_window,
                    window_secs = policy.budget_window.as_secs(),
                    "restart budget exceeded; marking degraded"
                );
                return Err(SupervisorError::RestartBudgetExceeded {
                    name: req.name.clone(),
                    restarts: in_window,
                    window_secs: policy.budget_window.as_secs(),
                });
            }
            let next = (h.current_backoff.as_millis() as f32 * policy.backoff_multiplier) as u64;
            let capped = next.min(policy.max_backoff.as_millis() as u64);
            let delay = h.current_backoff;
            h.current_backoff = Duration::from_millis(capped.max(1));
            (delay, h.spec.clone())
        };

        // Sleep the backoff interval. No `Child` is in scope here, so the
        // compiler can trivially prove the future is Send.
        debug!(
            child = %req.name,
            delay_ms = delay.as_millis() as u64,
            exit_code = req.exit_code,
            "restart scheduled"
        );
        tokio::time::sleep(delay).await;

        if *self.shutdown_flag.lock().await {
            return Ok(());
        }

        // Spawn a fresh child. spawn_child is its own future with its own
        // stack, so nothing in the old monitor's frame is borrowed here.
        self.spawn_child(spec).await?;
        info!(child = %req.name, "child respawned");
        Ok(())
    }

    /// Stop every child in parallel. Used during graceful shutdown.
    pub async fn shutdown_all(self: &Arc<Self>) -> Result<(), SupervisorError> {
        *self.shutdown_flag.lock().await = true;

        let monitors: Vec<JoinHandle<()>> = {
            let mut mons = self.monitors.lock().await;
            mons.drain().map(|(_, j)| j).collect()
        };

        // Sending the kill signal: tokio's `Command::kill_on_drop(true)` is in
        // place, but we explicitly mark every child as Stopped here AND
        // abort any in-flight stdout/stderr forwarder tasks (I-5 / NEW-008)
        // so the process tree can drain cleanly.
        {
            let guard = self.handles.read().await;
            for (_, h) in guard.iter() {
                let mut handle = h.lock().await;
                handle.status = ChildStatus::Stopped;
                handle.abort_io_tasks();
            }
        }

        for j in monitors {
            // Detach: the per-child monitor will exit cleanly when its child
            // stream closes. We don't await — that could deadlock.
            j.abort();
        }
        Ok(())
    }

    /// Force-kill a single child by name. Used by the watchdog when a
    /// heartbeat is missed past the limit.
    pub async fn kill_child(self: &Arc<Self>, name: &str) -> Result<(), SupervisorError> {
        let pid_opt = {
            let guard = self.handles.read().await;
            match guard.get(name) {
                Some(h) => h.lock().await.pid,
                None => None,
            }
        };
        let pid = match pid_opt {
            Some(p) => p,
            None => return Ok(()),
        };

        kill_pid(pid)?;
        warn!(child = %name, pid, "force-killed child");
        Ok(())
    }

    /// Snapshot every child handle for read-only consumers (health server,
    /// IPC layer, watchdog). Cached for [`SNAPSHOT_TTL`] (NEW-015).
    pub async fn snapshot(&self) -> Vec<ChildSnapshot> {
        // Fast-path: serve from cache if still fresh. The lock here is
        // fine — it's held only for a microsecond per call.
        {
            let cache = self.snapshot_cache.lock().await;
            if let Some((stamp, snap)) = cache.as_ref() {
                if stamp.elapsed() < SNAPSHOT_TTL {
                    return snap.clone();
                }
            }
        }

        let guard = self.handles.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for (name, handle) in guard.iter() {
            let h = handle.lock().await;
            let percentiles = h.latency_percentiles_us();
            // I-18: total uptime must include the still-running portion
            // of the current spawn. Before this fix, `total_uptime` was
            // only updated AFTER the child exited, so a long-running
            // worker reported zero total uptime forever.
            let total_uptime = h.total_uptime + h.current_uptime();
            out.push(ChildSnapshot {
                name: name.clone(),
                status: h.status,
                pid: h.pid,
                restart_count: h.restart_count,
                restart_dropped_count: h.restart_dropped_count,
                current_uptime_ms: h.current_uptime().as_millis() as u64,
                total_uptime_ms: total_uptime.as_millis() as u64,
                last_exit_code: h.last_exit_code,
                last_started_at: h.last_started_at,
                last_restart_at: h.last_restart_at,
                p50_us: percentiles.map(|p| p.0),
                p95_us: percentiles.map(|p| p.1),
                p99_us: percentiles.map(|p| p.2),
                last_job_id: h.last_job_id,
                last_job_duration_ms: h.last_job_duration_ms,
                last_job_status: h.last_job_status.map(|s| s.to_string()),
                last_job_completed_at: h.last_job_completed_at,
                avg_job_ms: h.avg_job_ms(),
                total_jobs_completed: h.total_jobs_completed,
                total_jobs_failed: h.total_jobs_failed,
                total_jobs_dispatched: h.total_jobs_dispatched,
                // Phase-A C1: convert bytes → MB. Saturating arithmetic so
                // a sysinfo blip that returns u64::MAX can't overflow.
                rss_mb: h.rss_bytes.map(|b| b / (1024 * 1024)).unwrap_or(0),
            });
        }
        // Phase-A C3: natural-order sort so `parser-worker-2` comes
        // before `parser-worker-10`. The previous lexical sort produced
        // 0, 1, 10, 11, 12, …, 2, 3 — confusing on every /health dump.
        out.sort_by(|a, b| natural_name_cmp(&a.name, &b.name));

        // Refresh the cache.
        {
            let mut cache = self.snapshot_cache.lock().await;
            *cache = Some((Instant::now(), out.clone()));
        }
        out
    }

    /// Update the per-child job telemetry after a `WorkerCompleteJob` IPC
    /// notification. `worker_name` is the ChildSpec name the router
    /// assigned to the job — usually obtained from `JobQueue::complete`.
    pub async fn record_job_completion(
        self: &Arc<Self>,
        worker_name: &str,
        job_id: u64,
        status: &'static str,
        duration_ms: u64,
    ) {
        if let Some(handle) = self.handle_for(worker_name).await {
            let mut h = handle.lock().await;
            h.record_job_completion(job_id, status, duration_ms);
        }
    }

    /// Bump the dispatched-job counter for a worker. Phase-A C5: called
    /// by the router after a successful `dispatch_to_pool` so /health
    /// reports a non-zero counter even before the worker has had a
    /// chance to send its first `WorkerCompleteJob`. Silently no-ops if
    /// the worker name is unknown — the router has nothing useful to do
    /// with that error case.
    pub async fn record_job_dispatch(self: &Arc<Self>, worker_name: &str) {
        if let Some(handle) = self.handle_for(worker_name).await {
            let mut h = handle.lock().await;
            h.record_job_dispatch();
        }
    }

    /// Refresh per-worker RSS samples via `sysinfo`. Phase-A C1.
    ///
    /// Walks the live handle map, collects every running PID, then runs
    /// the actual `sysinfo` refresh inside `tokio::task::spawn_blocking`
    /// because `System::refresh_processes_specifics` does real OS
    /// syscalls (PEB walk on Windows, /proc on Linux) that can take
    /// tens of milliseconds and would otherwise block the runtime.
    /// After the blocking call returns we re-acquire each handle lock
    /// and write the sample. PIDs that no longer exist record `None`
    /// so `/health` doesn't report stale numbers for a dead worker.
    pub async fn refresh_rss_samples(self: &Arc<Self>) {
        // Phase 1: snapshot (name, pid) under read locks. Cheap.
        let pairs: Vec<(String, u32)> = {
            let guard = self.handles.read().await;
            let mut out = Vec::with_capacity(guard.len());
            for (name, handle) in guard.iter() {
                let h = handle.lock().await;
                if let Some(pid) = h.pid {
                    out.push((name.clone(), pid));
                }
            }
            out
        };
        if pairs.is_empty() {
            return;
        }

        // Phase 2: blocking sysinfo refresh, off the runtime.
        let pids: Vec<u32> = pairs.iter().map(|(_, p)| *p).collect();
        let rss_by_pid = match tokio::task::spawn_blocking(move || sample_rss_bytes(&pids)).await {
            Ok(map) => map,
            Err(e) => {
                warn!(error = %e, "rss sample task panicked or was cancelled");
                return;
            }
        };

        // Phase 3: write back. Workers whose PID disappeared between
        // snapshot and refresh get `None` so /health doesn't lie.
        let guard = self.handles.read().await;
        for (name, pid) in pairs {
            let Some(handle) = guard.get(&name) else {
                continue;
            };
            let mut h = handle.lock().await;
            // Skip if the child has already been respawned with a new
            // PID since we sampled — the next refresh tick will catch
            // the new PID's RSS.
            if h.pid != Some(pid) {
                continue;
            }
            h.record_rss_bytes(rss_by_pid.get(&pid).copied());
        }
    }

    /// Return a clone of the live config (used by the IPC `Status` response).
    pub fn config(&self) -> &SupervisorConfig {
        &self.config
    }

    /// Borrow the shared log ring (used by the IPC `Logs` response).
    pub fn log_ring(&self) -> Arc<LogRing> {
        self.log_ring.clone()
    }

    /// Fetch all child names (used by the watchdog loop).
    pub async fn child_names(&self) -> Vec<String> {
        let guard = self.handles.read().await;
        guard.keys().cloned().collect()
    }

    /// Borrow a child handle Arc by name (used by the watchdog).
    pub async fn handle_for(&self, name: &str) -> Option<Arc<Mutex<ChildHandle>>> {
        let guard = self.handles.read().await;
        guard.get(name).cloned()
    }

    /// Update the heartbeat timestamp for a child.
    pub async fn record_heartbeat(&self, name: &str) {
        if let Some(h) = self.handle_for(name).await {
            let mut handle = h.lock().await;
            handle.last_heartbeat = Some(Instant::now());
        }
    }

    /// Dispatch a single JSON-line job to the named worker via its stdin
    /// pipe. The caller serialises the payload; the manager appends a
    /// trailing newline and flushes.
    ///
    /// Returns `Err(SupervisorError::Other)` if the child is not running,
    /// its stdin handle has been reaped, or the write fails.
    pub async fn dispatch_job(&self, name: &str, payload: &str) -> Result<(), SupervisorError> {
        let handle = self
            .handle_for(name)
            .await
            .ok_or_else(|| SupervisorError::Other(format!("unknown child: {name}")))?;
        let stdin_arc = {
            let h = handle.lock().await;
            if h.status != ChildStatus::Running {
                return Err(SupervisorError::Other(format!(
                    "child '{name}' not running (status {:?})",
                    h.status
                )));
            }
            h.stdin
                .clone()
                .ok_or_else(|| SupervisorError::Other(format!("child '{name}' has no stdin")))?
        };
        // Bug F-2 (2026-05-01): bound the IPC write so a saturated
        // worker stdin pipe (Windows pipe buffer = 64 KB) cannot hang
        // the supervisor's router task forever. At ~1100+ files in a
        // single `mneme build`, every worker's stdin buffer fills and
        // every dispatch sits on `flush().await` with no recovery —
        // the entire supervisor goes silent. 10 s is generous for a
        // healthy worker (microseconds in practice) and short enough
        // to surface a real wedge before the user thinks the build
        // hung. On timeout we return an error; `dispatch_to_pool`
        // treats that as "try the next worker", and the watchdog
        // (Bug F-9) eventually forces a restart of the wedged worker.
        const STDIN_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        let mut stdin = stdin_arc.lock().await;
        let payload_bytes = payload.as_bytes().to_vec();
        let needs_newline = !payload.ends_with('\n');
        let write_result: Result<Result<(), std::io::Error>, tokio::time::error::Elapsed> =
            tokio::time::timeout(STDIN_WRITE_TIMEOUT, async {
                stdin.write_all(&payload_bytes).await?;
                if needs_newline {
                    stdin.write_all(b"\n").await?;
                }
                stdin.flush().await?;
                Ok(())
            })
            .await;
        match write_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SupervisorError::Io(e)),
            Err(_elapsed) => Err(SupervisorError::Other(format!(
                "child '{name}' stdin write timed out after {:?} (wedged worker?)",
                STDIN_WRITE_TIMEOUT
            ))),
        }
    }

    /// Pick a worker matching `prefix` (e.g. `"parser-worker-"`) in round
    /// robin fashion and dispatch a job to it. Used by the daemon's
    /// in-process router so the CLI doesn't have to know how many workers
    /// exist.
    pub async fn dispatch_to_pool(
        &self,
        prefix: &str,
        payload: &str,
    ) -> Result<String, SupervisorError> {
        // K10 chaos-test-only hook (compiled out of release binaries):
        // honor the `--inject-crash N` countdown set by the daemon
        // binary's `Start` arm. When the Nth dispatch lands here this
        // call panics, the dispatch task aborts, and the per-child
        // monitor + restart loop respawn the worker. Production builds
        // never compile this branch in.
        //
        // Gated on `feature = "test-hooks"` only (not cfg(test)) — the
        // test_hooks module declaration in lib.rs has the same gate so
        // a lib test without the feature flag would not see the module.
        #[cfg(feature = "test-hooks")]
        {
            crate::test_hooks::crash_if_armed();
        }
        let mut candidates: Vec<String> = {
            let guard = self.handles.read().await;
            guard
                .keys()
                .filter(|n| n.starts_with(prefix))
                .cloned()
                .collect()
        };
        candidates.sort();
        // BUG-A4-013 fix (2026-05-04): rotate the start index per
        // dispatch so we honour the documented round-robin contract
        // instead of always retrying parser-worker-0 first. A wedged
        // head-of-pool worker now eats one timeout per pool revolution,
        // not per dispatch.
        let n = candidates.len();
        let start: usize = if n == 0 {
            0
        } else {
            self.round_robin_idx
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                % n
        };
        for offset in 0..n {
            let name = &candidates[(start + offset) % n];
            match self.dispatch_job(name, payload).await {
                Ok(()) => return Ok(name.clone()),
                Err(e) => {
                    debug!(child = %name, error = %e, "pool dispatch attempt failed; trying next");
                }
            }
        }
        Err(SupervisorError::Other(format!(
            "no worker matching prefix '{prefix}' is accepting jobs ({} candidates)",
            candidates.len()
        )))
    }

    /// Bug I defensive fix: scan every live child and emit a recovery
    /// log line for any worker that has been crash-looping but has now
    /// stabilised.
    ///
    /// A child qualifies as "recovered" when, in a single pass, ALL of
    /// the following hold for its `ChildHandle`:
    ///   1. `restart_count >= 3` — it has crash-looped at least three
    ///      times since supervisor boot.
    ///   2. `current_uptime() >= 60s` — its most recent spawn has been
    ///      stable for at least one minute.
    ///   3. `crash_loop_recovery_logged == false` — we have not already
    ///      logged the recovery for this spawn lifetime.
    ///
    /// On match the function emits exactly one `info!` line of the form
    /// `child=<name> total_restarts=<N> "child recovered from crash
    /// loop after stable 60s uptime"`, then sets
    /// `crash_loop_recovery_logged = true` so the next call is a
    /// no-op until the worker crashes again (at which point
    /// `record_restart` clears the flag).
    ///
    /// Designed to be invoked from a periodic supervisor task at a
    /// modest cadence (the production path uses 5s, matching the RSS
    /// refresher). The check is cheap: it acquires the read lock on
    /// the handle map and a per-handle write lock only on the children
    /// that actually meet the threshold.
    ///
    /// Returns the number of recovery log lines emitted on this pass.
    /// Tests use the count to confirm the one-shot contract holds.
    pub async fn check_recovery_logs(self: &Arc<Self>) -> usize {
        const CRASH_LOOP_THRESHOLD: u64 = 3;
        const STABLE_UPTIME: Duration = Duration::from_secs(60);
        let guard = self.handles.read().await;
        let mut emitted = 0usize;
        for (name, handle) in guard.iter() {
            let mut h = handle.lock().await;
            // Quick filter: only consider live children that have
            // accumulated enough restarts to have been "looping".
            if h.restart_count < CRASH_LOOP_THRESHOLD {
                continue;
            }
            if h.crash_loop_recovery_logged {
                continue;
            }
            if h.current_uptime() < STABLE_UPTIME {
                continue;
            }
            // All three gates passed — fire the one-shot log line and
            // flip the flag so the next pass treats this child as
            // already-acknowledged until the next crash resets it.
            info!(
                child = %name,
                total_restarts = h.restart_count,
                "child recovered from crash loop after stable 60s uptime"
            );
            h.crash_loop_recovery_logged = true;
            emitted = emitted.saturating_add(1);
        }
        emitted
    }
}

/// Bug I defensive fix: probe every unique worker exe path for its
/// `--version` output and confirm it matches the supervisor's compile-
/// time `CARGO_PKG_VERSION`. Refuses to spawn a mixed-version process
/// tree before workers crash-loop with an opaque `STATUS_CONTROL_C_EXIT`
/// (-1073741510 on Windows).
///
/// Behaviour:
///   * Iterates `specs`, deduplicating by `command` path.
///   * For each path: spawn `<path> --version` synchronously with a
///     2-second timeout. Capture stdout (non-UTF-8 bytes are dropped
///     via `String::from_utf8_lossy`).
///   * If the probe exits non-zero, or stdout contains no parseable
///     semver triplet, treat the worker as "version-unknown" and skip
///     it with a single `warn!` line. The check is advisory — workers
///     that don't yet support `--version` (e.g. md-ingest, brain — see
///     the binary main.rs files in this workspace) are explicitly
///     allowed.
///   * If a parseable semver IS found AND it differs from
///     `env!("CARGO_PKG_VERSION")`, return
///     `SupervisorError::BinaryVersionSkew { worker, expected, actual }`.
///     The `worker` field carries the `ChildSpec.name` of the first
///     spec that resolved to the offending exe path so the operator
///     gets a friendly identifier in the error message.
///
/// `expected_version` is plumbed in from the call site (typically
/// `env!("CARGO_PKG_VERSION")`) so the function is testable without
/// having to muck with the build-time constant.
pub fn probe_worker_versions(
    specs: &[ChildSpec],
    expected_version: &str,
) -> Result<(), SupervisorError> {
    use std::collections::HashSet;
    use std::time::Duration as StdDuration;
    let mut seen: HashSet<String> = HashSet::new();
    for spec in specs {
        if !seen.insert(spec.command.clone()) {
            continue;
        }
        // Synchronous probe — runs once at boot before the tokio
        // multi-thread scheduler has anything to do, so blocking the
        // current thread for at most 2s is cheap and avoids dragging
        // the supervisor's tokio context into a child-spawning code
        // path before `spawn_all`.
        let probe = match probe_single_worker(&spec.command, StdDuration::from_secs(2)) {
            Ok(out) => out,
            Err(e) => {
                // The exe is unreachable or doesn't accept --version.
                // That's fine: the check is best-effort. Log and skip.
                warn!(
                    worker = %spec.name,
                    binary = %spec.command,
                    error = %e,
                    "worker --version probe failed; skipping version check for this binary"
                );
                continue;
            }
        };
        let actual = match parse_semver(&probe) {
            Some(v) => v,
            None => {
                debug!(
                    worker = %spec.name,
                    binary = %spec.command,
                    output = %probe.trim(),
                    "worker --version output had no parseable semver; skipping check"
                );
                continue;
            }
        };
        if actual != expected_version {
            return Err(SupervisorError::BinaryVersionSkew {
                worker: spec.name.clone(),
                expected: expected_version.to_string(),
                actual,
            });
        }
        debug!(
            worker = %spec.name,
            binary = %spec.command,
            version = %actual,
            "worker version matches supervisor"
        );
    }
    Ok(())
}

/// Synchronous helper for [`probe_worker_versions`]. Spawns the exe with
/// `--version`, waits up to `timeout`, returns combined stdout+stderr as
/// a UTF-8-lossy string. The caller decides whether the response is
/// authoritative.
fn probe_single_worker(
    command_path: &str,
    timeout: std::time::Duration,
) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind};
    use std::process::{Command as StdCommand, Stdio};
    use std::time::Instant;

    let mut child = StdCommand::new(command_path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Poll-based timeout: try_wait every 50ms until the process exits or
    // the deadline elapses. Avoids dragging in extra crates (wait-timeout,
    // tokio::process here) and keeps the probe synchronous.
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let output = child.wait_with_output()?;
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                if !status.success() && stdout.trim().is_empty() && stderr.trim().is_empty() {
                    return Err(Error::other(format!(
                        "--version exited with {status:?} and no output"
                    )));
                }
                let mut combined = stdout;
                if !stderr.trim().is_empty() {
                    combined.push_str(&stderr);
                }
                return Ok(combined);
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::new(
                        ErrorKind::TimedOut,
                        "--version probe exceeded 2s timeout",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Extract the first `<major>.<minor>.<patch>` triplet from a string.
/// Returns the matched substring (e.g. `"0.3.2"`) or `None` if no
/// semver-shaped sequence is present. No regex dependency — a simple
/// hand-rolled scan is sufficient and keeps the supervisor's dep graph
/// minimal.
fn parse_semver(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // Find the next ASCII digit.
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // Try to consume `<digits>.<digits>.<digits>` starting at `i`.
        let start = i;
        let mut parts = 0u8;
        let mut last_digit_end = i;
        loop {
            // Consume one run of digits.
            let dig_start = i;
            while i < n && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == dig_start {
                break; // No digits — abort this attempt.
            }
            last_digit_end = i;
            parts += 1;
            if parts == 3 {
                break;
            }
            // Need a '.' between parts.
            if i < n && bytes[i] == b'.' {
                i += 1;
            } else {
                break;
            }
        }
        if parts == 3 {
            // Successful match — `bytes[start..last_digit_end]` is the
            // full semver triplet.
            return Some(s[start..last_digit_end].to_string());
        }
        // Otherwise advance past whatever we consumed and try again.
        if i == start {
            i += 1;
        }
    }
    None
}

/// Read-only summary used by the health & IPC layers.
///
/// v0.3.0+ added the `last_job_*` / `avg_job_ms` telemetry — sourced
/// from `WorkerCompleteJob` IPC messages the workers now emit. The
/// fields are `Option` so builds that don't run any jobs stay quiet.
/// All new fields are additive — older CLI clients tolerate their
/// absence via serde's `skip_serializing_if`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChildSnapshot {
    /// Child name.
    pub name: String,
    /// Lifecycle status.
    pub status: ChildStatus,
    /// OS PID if running.
    pub pid: Option<u32>,
    /// Total restarts since boot.
    pub restart_count: u64,
    /// Bug L: total restart requests that were dropped because the
    /// restart channel had been closed (receiver dropped, supervisor
    /// shutting down). Visible in `mneme doctor` per-worker line and
    /// in the Prometheus `mneme_child_restart_dropped_count` series.
    #[serde(default)]
    pub restart_dropped_count: u64,
    /// Uptime since the most recent spawn.
    pub current_uptime_ms: u64,
    /// Cumulative uptime across all spawns.
    pub total_uptime_ms: u64,
    /// Last observed exit code.
    pub last_exit_code: Option<i32>,
    /// Wall-clock time of the most recent successful spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Wall-clock time of the most recent auto-restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_restart_at: Option<chrono::DateTime<chrono::Utc>>,
    /// p50 latency in microseconds.
    pub p50_us: Option<u64>,
    /// p95 latency in microseconds.
    pub p95_us: Option<u64>,
    /// p99 latency in microseconds.
    pub p99_us: Option<u64>,
    /// Job id most recently reported complete by this worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job_id: Option<u64>,
    /// Wall-clock ms the worker spent on its most recent job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job_duration_ms: Option<u64>,
    /// Outcome (`"ok"` or `"error"`) of the most recent completed job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job_status: Option<String>,
    /// UTC timestamp of the most recent `WorkerCompleteJob`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_job_completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Rolling-window average job duration in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_job_ms: Option<u64>,
    /// Cumulative successful job completions reported via IPC.
    #[serde(default)]
    pub total_jobs_completed: u64,
    /// Cumulative failed job completions reported via IPC.
    #[serde(default)]
    pub total_jobs_failed: u64,
    /// Cumulative job dispatches the supervisor router has pushed to
    /// this worker. Phase-A C5: a non-zero value here with
    /// `total_jobs_completed=0` is a strong signal that the worker is
    /// processing but not reporting `WorkerCompleteJob`.
    #[serde(default)]
    pub total_jobs_dispatched: u64,
    /// Resident set size in MB for the child's process. Phase-A C1:
    /// Sampled by the supervisor's RSS refresher task via `sysinfo`.
    /// `0` until the first sample lands or when the child is not
    /// running. Always populated (never `None`) so the existing
    /// `/health` JSON schema stays additive.
    #[serde(default)]
    pub rss_mb: u64,
}

/// Test-only re-export of the private `natural_name_cmp` helper so the
/// unit-test in `tests.rs` can exercise it directly without spawning
/// real child processes (Phase-A C3 contract test).
#[cfg(test)]
pub(crate) fn __test_natural_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    natural_name_cmp(a, b)
}

/// Phase-A C3: natural ordering for child names. Splits each name into
/// a leading non-digit prefix and a trailing decimal suffix so that
/// `parser-worker-2` < `parser-worker-10`. Falls back to a plain
/// lexical compare when no trailing digit run exists, which preserves
/// the prior ordering for non-pool workers (`watchdog`, `livebus`, …).
fn natural_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| -> (String, Option<u64>) {
        let mut digit_start = s.len();
        for (i, c) in s.char_indices().rev() {
            if c.is_ascii_digit() {
                digit_start = i;
            } else {
                break;
            }
        }
        if digit_start == s.len() {
            (s.to_string(), None)
        } else {
            let (prefix, suffix) = s.split_at(digit_start);
            // u64 saturates at 20 digits — far longer than any worker
            // index will ever be. parse failure is impossible here
            // because we just verified every char is ASCII digit.
            let n: u64 = suffix.parse().unwrap_or(u64::MAX);
            (prefix.to_string(), Some(n))
        }
    };
    let (pa, na) = split(a);
    let (pb, nb) = split(b);
    match pa.cmp(&pb) {
        std::cmp::Ordering::Equal => match (na, nb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => std::cmp::Ordering::Equal,
        },
        other => other,
    }
}

/// Phase-A C1: blocking sysinfo helper called from `spawn_blocking`.
/// Refreshes only the PIDs we care about (cheaper than a full system
/// scan) and only the memory field of each one. Returns a map from
/// PID → RSS in bytes; missing PIDs (workers that exited between
/// snapshot and sample) are simply absent from the map and the caller
/// records them as `None`.
fn sample_rss_bytes(pids: &[u32]) -> std::collections::HashMap<u32, u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid_refs: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pid_refs),
        // `remove_dead_processes = true` — sysinfo's docs say to pass
        // true here so a process that died between calls gets dropped
        // from the internal map. We rebuild `sys` every refresh anyway,
        // so the value doesn't matter much, but keep it consistent.
        true,
        ProcessRefreshKind::new().with_memory(),
    );
    let mut out = std::collections::HashMap::with_capacity(pids.len());
    for pid in pids {
        if let Some(p) = sys.process(Pid::from_u32(*pid)) {
            out.insert(*pid, p.memory());
        }
    }
    out
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> Result<(), SupervisorError> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), Signal::SIGKILL)
        .map_err(|e| SupervisorError::Other(format!("kill({pid}) failed: {e}")))
}

/// M12 — Windows `CREATE_NO_WINDOW` flag for `taskkill` spawn from
/// `kill_pid`. Without this flag a windowless supervisor (daemon
/// detached at startup) flashes a console window every time the
/// watchdog kills a worker by PID.
#[cfg(windows)]
const WINDOWS_KILL_PID_FLAGS: u32 = 0x08000000;

#[cfg(windows)]
fn kill_pid(pid: u32) -> Result<(), SupervisorError> {
    // `tokio::process::Child::kill` is the preferred path, but the watchdog
    // only has the PID. Use `taskkill` via the standard library; it ships
    // with every Windows install and avoids a `windows-sys` dep here.
    use std::os::windows::process::CommandExt;
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(WINDOWS_KILL_PID_FLAGS)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| SupervisorError::Other(format!("taskkill spawn failed: {e}")))?;
    if !status.success() {
        return Err(SupervisorError::Other(format!(
            "taskkill exited with {status}"
        )));
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// M12 — supervisor `kill_pid` taskkill spawn must include
    /// `CREATE_NO_WINDOW` (`0x08000000`) so a windowless supervisor does
    /// not flash a console when terminating a worker by PID.
    #[test]
    fn windows_kill_pid_flags() {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        assert_eq!(
            WINDOWS_KILL_PID_FLAGS & CREATE_NO_WINDOW,
            CREATE_NO_WINDOW,
            "kill_pid taskkill spawn must set CREATE_NO_WINDOW"
        );
    }
}
