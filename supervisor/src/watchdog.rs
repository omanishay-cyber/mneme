//! Watchdog: 1-second heartbeat check, 60-second deep self-test.
//!
//! - Heartbeat tick (every 1s): for every running child, ensure the last
//!   heartbeat is within `HEARTBEAT_DEADLINE`. If a child has missed it, the
//!   watchdog force-kills the PID and lets the [`crate::ChildManager`]
//!   monitor task observe the exit and queue an auto-restart request on
//!   the restart channel (see `manager::run_restart_loop`).
//! - Deep self-test (every `health_check_interval`, default 60s): pings each
//!   child's `/health` endpoint over its dedicated IPC channel.
//!
//! The auto-restart path was disabled in v0.1 due to a Send-recursion
//! cycle triggered by `tokio::process::Child` stdio handles on Windows.
//! It is re-enabled as of this commit by decoupling the restart decision
//! from the monitor task via an `mpsc::UnboundedChannel` (see
//! `manager.rs :: RestartRequest`). No change to the watchdog contract.

use crate::child::ChildStatus;
use crate::error::SupervisorError;
use crate::manager::ChildManager;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

/// Default maximum time a running child can go without sending a
/// heartbeat before the watchdog force-kills it.
///
/// NEW-014 (v0.3.1): restored from the v0.1 1-hour stub to 30 seconds so
/// the watchdog is actually doing its job. Workers that legitimately
/// idle for longer (md-ingest, brain-stub) MUST set
/// [`crate::child::ChildSpec::heartbeat_deadline`] to a larger duration
/// (or use a long-but-finite value such as 24h to opt out in practice).
pub const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(30);

/// Watchdog that supervises the [`ChildManager`].
pub struct Watchdog {
    manager: Arc<ChildManager>,
    self_test_interval: Duration,
}

impl Watchdog {
    /// Construct a new watchdog.
    pub fn new(manager: Arc<ChildManager>, self_test_interval: Duration) -> Self {
        Self {
            manager,
            self_test_interval,
        }
    }

    /// Run the watchdog forever (until `shutdown.notified()`).
    pub async fn run(&self, shutdown: Arc<Notify>) {
        info!(
            self_test_interval_s = self.self_test_interval.as_secs(),
            heartbeat_deadline_s = HEARTBEAT_DEADLINE.as_secs(),
            "watchdog started"
        );
        let mut heartbeat_tick = tokio::time::interval(Duration::from_secs(1));
        let mut self_test_tick = tokio::time::interval(self.self_test_interval);
        // First tick fires immediately for both — skip it.
        heartbeat_tick.tick().await;
        self_test_tick.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    info!("watchdog shutting down");
                    break;
                }
                _ = heartbeat_tick.tick() => {
                    if let Err(e) = self.heartbeat_pass().await {
                        warn!(error = %e, "heartbeat pass error");
                    }
                }
                _ = self_test_tick.tick() => {
                    if let Err(e) = self.self_test_pass().await {
                        warn!(error = %e, "self-test pass error");
                    }
                }
            }
        }
    }

    async fn heartbeat_pass(&self) -> Result<(), SupervisorError> {
        let names = self.manager.child_names().await;
        let now = Instant::now();
        for name in names {
            let handle = match self.manager.handle_for(&name).await {
                Some(h) => h,
                None => continue,
            };
            // S-PHASE NEW-055: heartbeat_deadline=None means "no enforcement"
            // (worker has not yet wired heartbeat-send). Previously this fell
            // back to the global 30 s default, which killed every worker on a
            // 30 s cadence because no worker actually sends heartbeats. The
            // doc comment on HEARTBEAT_DEADLINE already says workers "MUST set
            // ChildSpec::heartbeat_deadline" to opt in, so absence == opt-out.
            let (status, last_hb, deadline_opt) = {
                let h = handle.lock().await;
                (h.status, h.last_heartbeat, h.spec.heartbeat_deadline)
            };
            if status != ChildStatus::Running {
                continue;
            }
            let deadline = match deadline_opt {
                Some(d) => d,
                None => continue, // worker opted out of heartbeat enforcement
            };
            let last = match last_hb {
                Some(t) => t,
                None => continue,
            };
            let missed = now.duration_since(last);
            if missed > deadline {
                error!(
                    child = %name,
                    missed_ms = missed.as_millis() as u64,
                    "heartbeat missed past deadline; force-kill"
                );
                if let Err(e) = self.manager.kill_child(&name).await {
                    warn!(child = %name, error = %e, "kill_child failed");
                }
            } else {
                debug!(child = %name, missed_ms = missed.as_millis() as u64, "heartbeat ok");
            }
        }
        Ok(())
    }

    async fn self_test_pass(&self) -> Result<(), SupervisorError> {
        // The deep self-test pings each child's per-process /health endpoint
        // over its dedicated IPC channel. Children publish a one-shot
        // socket/pipe at `<root>/<child-name>.sock`. The supervisor only
        // verifies the channel responds; semantic results are interpreted by
        // each worker.
        let names = self.manager.child_names().await;
        for name in names {
            let handle = match self.manager.handle_for(&name).await {
                Some(h) => h,
                None => continue,
            };
            // B-021 v2 (concurrency-audit F3 fix, 2026-04-30): read ALL
            // four fields under ONE lock acquisition to avoid the TOCTOU
            // gap where status/last_heartbeat could change between two
            // separate lock blocks (audit found this would surface as
            // "self-test logs `pending_first_hb` for a worker that just
            // sent its first heartbeat" — telemetry inconsistency, not
            // safety bug, but cheap to fix and worth doing).
            let (status, endpoint, deadline_opt, last_hb_opt) = {
                let h = handle.lock().await;
                (
                    h.status,
                    h.spec.health_endpoint.clone(),
                    h.spec.heartbeat_deadline,
                    h.last_heartbeat,
                )
            };
            if status != ChildStatus::Running {
                continue;
            }
            let endpoint = match endpoint {
                Some(e) => e,
                None => continue,
            };
            // B-021 (D:\Mneme Dome cycle, 2026-04-30): if the worker has
            // opted out of heartbeat enforcement (heartbeat_deadline=None
            // per the S-PHASE NEW-055 contract on `heartbeat_pass`), the
            // self-test log MUST NOT report `healthy:false` every cycle —
            // that fills `supervisor.log` with thousands of false alarms
            // (observed: 22 workers × every 60s for 7+ minutes on POS PC).
            // Workers that wired heartbeat-send keep the real check.
            match (deadline_opt, last_hb_opt) {
                (None, _) => {
                    // Opted-out worker: log liveness as opt-out, not failure.
                    debug!(child = %name, endpoint = %endpoint, healthy = "n/a", reason = "no_heartbeat_wired", "self-test");
                }
                (Some(_), None) => {
                    // Has a deadline but never sent a single heartbeat —
                    // worker is alive (would not be Running otherwise) but
                    // hasn't wired the emit. Distinct from "missed".
                    debug!(child = %name, endpoint = %endpoint, healthy = "pending_first_hb", "self-test");
                }
                (Some(_deadline), Some(t)) => {
                    let elapsed_ms = t.elapsed().as_millis() as u64;
                    let healthy = elapsed_ms < self.self_test_interval.as_millis() as u64;
                    debug!(
                        child = %name,
                        endpoint = %endpoint,
                        healthy,
                        last_hb_ms = elapsed_ms,
                        "self-test"
                    );
                }
            }
        }
        Ok(())
    }
}
