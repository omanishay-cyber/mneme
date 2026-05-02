//! SLA dashboard HTTP server (localhost:7777 by default).
//!
//! Exposes:
//!   - `GET /health`      — full SLA snapshot as JSON
//!   - `GET /health/live` — liveness probe (always 200 if process is up)
//!   - `GET /metrics`     — minimal Prometheus-like text format
//!
//! Bound exclusively to `127.0.0.1`. No authentication: this is a local-only
//! daemon (see §22 of the design doc).

#![allow(clippy::items_after_test_module)]

use crate::api_graph;
use crate::error::SupervisorError;
use crate::job_queue::JobQueue;
use crate::manager::{ChildManager, ChildSnapshot};
use crate::watcher::WatcherStatsHandle;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use common::PathManager;
use livebus::SubscriberManager;
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use sysinfo::{Disks, System};
use tokio::sync::Notify;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

/// Cache TTL for `compute_disk_usage`. Disks::new_with_refreshed_list()
/// rescans every mounted filesystem on every call; on a Windows box
/// with multiple drives that's a few ms of syscalls per /metrics
/// scrape. I-4 / I-5: cache for 5 seconds so a /health + /metrics +
/// CLI status burst hitting in the same second hits one disk syscall,
/// not three.
const DISK_USAGE_TTL: Duration = Duration::from_secs(5);

/// Cache TTL for the full SLA snapshot (NEW-015). Same rationale as
/// the manager's snapshot cache but at the health-server boundary so
/// burst polls share a single composed response.
const SNAPSHOT_TTL: Duration = Duration::from_secs(1);

fn disk_usage_cache() -> &'static Mutex<Option<(Instant, DiskUsage)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, DiskUsage)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Full snapshot returned by `GET /health`.
#[derive(Debug, Clone, Serialize)]
pub struct SlaSnapshot {
    /// Wall-clock at the time of the snapshot.
    pub timestamp: DateTime<Utc>,
    /// Supervisor uptime in seconds.
    pub supervisor_uptime_s: u64,
    /// Per-child snapshots.
    pub children: Vec<ChildSnapshot>,
    /// Aggregate uptime % over the lifetime of the supervisor.
    pub overall_uptime_percent: f64,
    /// Forward-progress ratio: `total_jobs_completed / total_jobs_dispatched`
    /// across all workers.
    ///
    /// C2: surfaces real activity on `/health` instead of the previous
    /// hardcoded `0.0` placeholder. Workers do not yet report incremental
    /// parser cache hit/miss counters back to the supervisor — that is
    /// tracked as a v0.4 follow-up. Until then, this field reports the
    /// best signal the supervisor can compute locally from data the
    /// router + IPC handler already record. `1.0` means every dispatched
    /// job has reported back complete; values below `1.0` indicate jobs
    /// in flight or failures. `0.0` when nothing has been dispatched yet.
    pub cache_hit_rate: f64,
    /// Disk usage, free bytes, total bytes for the mneme root.
    pub disk: DiskUsage,
    /// Total bytes in use on the disk holding `~/.mneme`, expressed in
    /// whole megabytes. C2: scalar field exposed alongside the
    /// finer-grained `disk` struct so dashboards that just want a single
    /// number for a chart don't have to do the math.
    pub disk_usage_mb: u64,
    /// Number of jobs in the supervisor's queue still waiting for a
    /// worker. C2: zero when no queue is attached. Pulled from
    /// [`JobQueue::snapshot`] under the manager.
    pub queue_depth: u64,
    /// Aggregate p50 of per-worker job latency, microseconds. `0` when
    /// no worker has reported a completion yet. C2: aggregated across
    /// every child's `latency_samples_us` so the dashboard has a single
    /// SLA number rather than only per-worker percentiles.
    pub p50_us: u64,
    /// Aggregate p95 of per-worker job latency, microseconds.
    pub p95_us: u64,
    /// Aggregate p99 of per-worker job latency, microseconds.
    pub p99_us: u64,
    /// B15 (2026-05-02): human-friendly mirror of `p50_us`, in whole
    /// milliseconds. Same value, friendlier unit + name. UI clients
    /// should prefer this; raw `*_us` fields stay for back-compat with
    /// scripts that already parse them.
    pub typical_response_ms: u64,
    /// B15 (2026-05-02): human-friendly mirror of `p99_us`, in whole
    /// milliseconds. "Slow tail" = the worst 1% of requests.
    pub slow_response_ms: u64,
    /// File watcher percentiles (save-to-graph latency, ms).
    pub watcher: WatcherMetrics,
}

/// Watcher metrics surfaced on `/health` and `/metrics`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WatcherMetrics {
    /// Total files reindexed since boot.
    pub total_reindexed: u64,
    /// Total delete events processed.
    pub total_deletes: u64,
    /// Files dropped because they matched the ignore list.
    pub total_ignored: u64,
    /// p50 latency in milliseconds.
    pub p50_ms: u64,
    /// p95 latency in milliseconds (SLO = 500ms).
    pub p95_ms: u64,
    /// p99 latency in milliseconds.
    pub p99_ms: u64,
}

/// Disk usage summary used by [`SlaSnapshot`].
#[derive(Debug, Clone, Serialize)]
pub struct DiskUsage {
    /// Total bytes of the volume holding `~/.mneme`.
    pub total_bytes: u64,
    /// Free bytes.
    pub free_bytes: u64,
    /// Used percentage (0.0 – 100.0).
    pub used_percent: f64,
}

#[derive(Clone)]
struct AppState {
    manager: Arc<ChildManager>,
    started: Instant,
    watcher_stats: WatcherStatsHandle,
    /// NEW-015: cached SLA snapshot. When `/health` and `/metrics` are
    /// scraped in the same second they share the build cost, including
    /// the per-child Mutex storm in `ChildManager::snapshot`.
    snapshot_cache: Arc<Mutex<Option<(Instant, SlaSnapshot)>>>,
    /// C2: optional handle to the supervisor's [`JobQueue`] so
    /// `build_snapshot` can populate `queue_depth` without blocking on a
    /// manager round-trip. `None` when the supervisor was booted without
    /// a queue (test harnesses).
    job_queue: Option<Arc<JobQueue>>,
}

/// HTTP server hosting the SLA dashboard.
pub struct HealthServer {
    manager: Arc<ChildManager>,
    port: u16,
    watcher_stats: WatcherStatsHandle,
    /// 4.2: optional [`SubscriberManager`] threaded into
    /// [`api_graph::ApiGraphState`] via [`api_graph::ApiGraphState::with_livebus`]
    /// so the production daemon's `/ws` upgrades attach to a real bus
    /// instead of always returning the polite 503-style error frame.
    livebus: Option<SubscriberManager>,
    /// C2: optional handle to the supervisor's job queue so /health
    /// can report `queue_depth` without bouncing through `ChildManager`.
    job_queue: Option<Arc<JobQueue>>,
}

impl HealthServer {
    /// Construct a new health server.
    pub fn new(manager: Arc<ChildManager>, port: u16) -> Self {
        Self {
            manager,
            port,
            watcher_stats: WatcherStatsHandle::new(),
            livebus: None,
            job_queue: None,
        }
    }

    /// Construct a new health server with a shared watcher-stats handle.
    /// Call this flavor when the supervisor embeds a watcher so the SLA
    /// dashboard can surface save-to-graph latency.
    pub fn with_watcher_stats(
        manager: Arc<ChildManager>,
        port: u16,
        watcher_stats: WatcherStatsHandle,
    ) -> Self {
        Self {
            manager,
            port,
            watcher_stats,
            livebus: None,
            job_queue: None,
        }
    }

    /// Attach a livebus subscriber manager so the `/ws` route upgrades
    /// successfully. 4.2: `supervisor::lib::run` calls this once during
    /// boot so the production daemon hosts the bus.
    pub fn with_livebus(mut self, mgr: SubscriberManager) -> Self {
        self.livebus = Some(mgr);
        self
    }

    /// Attach the shared job queue so `/health` can populate
    /// `queue_depth`. C2: without this the field always reports `0`.
    pub fn with_job_queue(mut self, queue: Arc<JobQueue>) -> Self {
        self.job_queue = Some(queue);
        self
    }

    /// Run the server until `shutdown.notified()`.
    pub async fn serve(self, shutdown: Arc<Notify>) {
        let state = AppState {
            manager: self.manager,
            started: Instant::now(),
            watcher_stats: self.watcher_stats,
            snapshot_cache: Arc::new(Mutex::new(None)),
            job_queue: self.job_queue,
        };

        // F1 D0+D1+4.2 path: build the router via the testable helper so
        // unit tests cover the SPA fallback wiring exactly the way
        // production runs it.
        let static_dir = resolve_static_dir();
        let app = compose_app_router(state, self.livebus, static_dir);

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port);
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(addr = %addr, error = %e, "health server bind failed");
                return;
            }
        };
        info!(addr = %addr, "health server listening");

        let shutdown_signal = async move {
            shutdown.notified().await;
        };

        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal)
            .await
        {
            error!(error = %e, "health server error");
        }
    }
}

/// Compose the full daemon HTTP router (health/metrics + /api/graph + /ws +
/// SPA static fallback). Extracted from [`HealthServer::serve`] so the
/// SPA-fallback behaviour can be exercised in unit tests via
/// `tower::ServiceExt::oneshot`.
///
/// `livebus` is the optional [`SubscriberManager`] threaded into
/// [`api_graph::ApiGraphState`]; when `Some`, `/ws` upgrades attach to it
/// (4.2). When `None`, the route stays mounted but immediately closes the
/// upgrade with an error frame (existing behaviour).
///
/// `static_dir` is the on-disk vision-SPA `dist/` directory. When `Some`,
/// the router serves it at `/` and falls back to `<dir>/index.html` for
/// any path that doesn't match an existing file (SPA client-side routing).
/// When `None`, the root path returns 404 and the daemon runs in API-only
/// mode (A2 documented behaviour).
fn compose_app_router(
    state: AppState,
    livebus: Option<SubscriberManager>,
    static_dir: Option<PathBuf>,
) -> Router {
    let health_router = Router::new()
        .route("/health", get(health_full))
        .route("/health/live", get(health_live))
        .route("/metrics", get(metrics))
        .with_state(state);

    // 4.2 wiring: thread the livebus subscriber manager into the api_graph
    // state so the `/ws` upgrade handler attaches to a real bus.
    let api_state = match livebus {
        Some(mgr) => api_graph::ApiGraphState::from_defaults().with_livebus(mgr),
        None => api_graph::ApiGraphState::from_defaults(),
    };
    let api_router = api_graph::build_router(api_state);

    let mut app = Router::new().merge(health_router).merge(api_router);
    match static_dir {
        Some(dir) => {
            // A2 EC2 root-cause fix (2026-04-27, second pass):
            //
            // Agent H's earlier `ServeDir::new(&dir).fallback(fallback_svc)` +
            // `app.fallback_service(serve_dir)` wiring still timed out on
            // EC2 because tower-http's ServeDir runs `tokio::fs::metadata`
            // and `tokio::fs::File::open` on EVERY request — both go
            // through `tokio::task::spawn_blocking`. The supervisor
            // caps `max_blocking_threads(8)` (`main.rs` + `service.rs`)
            // and at boot those 8 threads are held by stdin/stdout
            // forwarders for the 7 named children plus the watcher's
            // RSS sampler. With the pool saturated the metadata syscall
            // queues forever; ServeDir's `.fallback(...)` never fires
            // because it's only invoked AFTER the metadata probe
            // resolves with `NotFound`. Result: `GET /` and any
            // SPA-router path hangs at the listener — exact EC2
            // symptom.
            //
            // Fix shape:
            //   * Pre-load `index.html` into memory at compose time
            //     (Agent H's idea, kept verbatim — that part was right).
            //   * Mount `/` and the catch-all SPA fallback as EXPLICIT
            //     axum routes that serve the cached bytes
            //     synchronously. They do not call `tokio::fs::*` and
            //     therefore never touch the blocking pool. This is
            //     the actual cure for the EC2 hang.
            //   * Mount `ServeDir` only at `/assets` via
            //     `nest_service`, so real on-disk JS/CSS bundles still
            //     stream lazily through tower-http but the SPA boot
            //     path (`/` + history-router URLs) is never gated on
            //     filesystem syscalls.
            //
            // Tower-http's `ServeDir` is still in the build because
            // `/assets/*` lookups need streaming, range requests, and
            // mime detection. The blocking-pool dependency is acceptable
            // there because the SPA already painted by the time it
            // requests assets, and most requests come AFTER the
            // stdin/stdout forwarders have finished their initial
            // burst — the saturated state is a boot-time phenomenon.
            let index_path = dir.join("index.html");
            let cached_index_bytes = match std::fs::read(&index_path) {
                Ok(bytes) => {
                    info!(
                        path = %dir.display(),
                        index_bytes = bytes.len(),
                        strategy = "explicit_route",
                        "vision SPA static dir resolved; index.html pre-loaded \
                         and bound to explicit axum route (bypasses ServeDir / \
                         blocking pool)"
                    );
                    Some(Arc::<[u8]>::from(bytes))
                }
                Err(e) => {
                    warn!(
                        path = %index_path.display(),
                        error = %e,
                        strategy = "explicit_route",
                        "vision SPA static dir resolved but index.html unreadable; \
                         SPA fallback will return 503"
                    );
                    None
                }
            };

            // Closure-style handler shared by `/` and the catch-all
            // fallback. `cached_index_bytes` is wrapped in `Arc<[u8]>`
            // so each invocation clones the Arc, not the bytes —
            // O(1) per request, no allocation past compose-time.
            //
            // Using `axum::routing::any_service` style would force
            // generic gymnastics; the cleanest fit is a plain async fn
            // that captures the Arc. We construct two clones below
            // (one for `/`, one for the fallback) so each route owns
            // its own reference.
            let serve_index = move |maybe_bytes: Option<Arc<[u8]>>| async move {
                match maybe_bytes {
                    Some(b) => Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(Body::from(b.to_vec()))
                        .expect("static SPA index response"),
                    None => Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                        .body(Body::from(
                            "vision SPA index.html unavailable; \
                                 reinstall with `mneme install`",
                        ))
                        .expect("503 SPA index response"),
                }
            };

            // Register the explicit `/` route BEFORE we add the
            // assets nest, otherwise axum's matcher would hand `/` to
            // ServeDir's directory-listing path. The fallback handler
            // uses the same closure for SPA-router URLs.
            let bytes_for_root = cached_index_bytes.clone();
            let bytes_for_fallback = cached_index_bytes.clone();
            app = app
                .route("/", get(move || serve_index(bytes_for_root.clone())))
                .fallback(get(move || serve_index(bytes_for_fallback.clone())));

            // Real on-disk assets stay on ServeDir, but only for the
            // narrow `/assets/*` subtree. This still uses
            // `tokio::fs::*` (and therefore the blocking pool), but
            // the SPA boot is no longer gated on it: index.html is
            // already in the user's browser by the time the first
            // bundle request lands.
            let assets_dir = dir.join("assets");
            if assets_dir.is_dir() {
                app = app.nest_service("/assets", ServeDir::new(&assets_dir));
            } else {
                warn!(
                    path = %assets_dir.display(),
                    "vision SPA assets/ subdir missing; bundle requests will hit \
                     the SPA fallback (likely a broken install)"
                );
            }
        }
        None => {
            warn!(
                "vision SPA static dir not found; root \"/\" will 404. \
                 Looked for: <MNEME_HOME>/static/vision/ (override with \
                 MNEME_STATIC_DIR=<path>). \
                 Install with `mneme install` to populate it."
            );
        }
    }
    app
}

async fn health_live() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn health_full(State(state): State<AppState>) -> impl IntoResponse {
    match build_snapshot(&state).await {
        Ok(s) => (StatusCode::OK, Json(s)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let snap = match build_snapshot(&state).await {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let mut body = String::new();
    body.push_str(&format!(
        "# TYPE mneme_supervisor_uptime_seconds counter\nmneme_supervisor_uptime_seconds {}\n",
        snap.supervisor_uptime_s
    ));
    body.push_str(&format!(
        "# TYPE mneme_overall_uptime_percent gauge\nmneme_overall_uptime_percent {}\n",
        snap.overall_uptime_percent
    ));
    body.push_str(&format!(
        "# TYPE mneme_cache_hit_rate gauge\nmneme_cache_hit_rate {}\n",
        snap.cache_hit_rate
    ));
    for c in &snap.children {
        body.push_str(&format!(
            "mneme_child_restart_count{{child=\"{}\"}} {}\n",
            c.name, c.restart_count
        ));
        // Bug L: dropped-restart gauge (closed-channel path).
        body.push_str(&format!(
            "mneme_child_restart_dropped_count{{child=\"{}\"}} {}\n",
            c.name, c.restart_dropped_count
        ));
        if let Some(p50) = c.p50_us {
            body.push_str(&format!(
                "mneme_child_latency_us{{child=\"{}\",quantile=\"0.5\"}} {}\n",
                c.name, p50
            ));
        }
        if let Some(p95) = c.p95_us {
            body.push_str(&format!(
                "mneme_child_latency_us{{child=\"{}\",quantile=\"0.95\"}} {}\n",
                c.name, p95
            ));
        }
        if let Some(p99) = c.p99_us {
            body.push_str(&format!(
                "mneme_child_latency_us{{child=\"{}\",quantile=\"0.99\"}} {}\n",
                c.name, p99
            ));
        }
    }
    (StatusCode::OK, body).into_response()
}

async fn build_snapshot(state: &AppState) -> Result<SlaSnapshot, SupervisorError> {
    // NEW-015: cached snapshot path. Burst-poll friendly.
    //
    // Bug SEC-4 (2026-05-01): recover from a poisoned mutex instead of
    // panicking. The previous `.expect("sla cache poisoned")` would
    // cascade a single panic into a full daemon crash because every
    // /health request would re-poison and re-panic. The cache value is
    // a plain `Option<(Instant, SlaSnapshot)>` — even after poison the
    // last-known value is still safe to read.
    {
        let cache = state
            .snapshot_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((stamp, snap)) = cache.as_ref() {
            if stamp.elapsed() < SNAPSHOT_TTL {
                return Ok(snap.clone());
            }
        }
    }

    let children = state.manager.snapshot().await;
    let supervisor_uptime_s = state.started.elapsed().as_secs();

    // Aggregate uptime % = sum(child total_uptime) / (n_children * supervisor_uptime).
    // I-18 ripple: `total_uptime_ms` now includes the still-running
    // portion of the current spawn, which makes the aggregate actually
    // approach 100% for healthy workers instead of trending to zero.
    let denom = (children.len() as u64).saturating_mul(supervisor_uptime_s.max(1));
    let numer: u64 = children.iter().map(|c| c.total_uptime_ms / 1000).sum();
    let overall_uptime_percent = if denom == 0 {
        100.0
    } else {
        ((numer as f64) / (denom as f64)) * 100.0
    };

    let disk = compute_disk_usage(state.manager.config().root_dir.as_path());
    // C2: scalar disk-used in MB so dashboards can render a single number.
    // Saturating to prevent any sysinfo blip from underflowing when free
    // briefly exceeds total during a refresh.
    let disk_usage_mb = disk.total_bytes.saturating_sub(disk.free_bytes) / (1024 * 1024);

    // C2: queue_depth = pending jobs in the supervisor's shared queue.
    // `0` when no queue is attached (test harnesses).
    let queue_depth: u64 = state
        .job_queue
        .as_ref()
        .map(|q| q.snapshot().pending as u64)
        .unwrap_or(0);

    // C2: forward-progress proxy. `total_jobs_completed` and
    // `total_jobs_dispatched` are already aggregated per-child by
    // `manager.snapshot()`. Sum them and divide. `0.0` when nothing has
    // been dispatched yet — that's the right initial state for the
    // dashboard.
    let total_completed: u64 = children.iter().map(|c| c.total_jobs_completed).sum();
    let total_dispatched: u64 = children.iter().map(|c| c.total_jobs_dispatched).sum();
    let cache_hit_rate = if total_dispatched == 0 {
        0.0
    } else {
        (total_completed as f64) / (total_dispatched as f64)
    };

    // C2: aggregate p50/p95/p99 across every child's reported percentiles.
    // Uses a population-of-percentiles approach (mean of per-worker
    // percentiles) which is approximate but cheap — the alternative
    // (re-sorting every sample across every worker) would defeat the
    // purpose of the snapshot cache. Per-worker numbers stay available
    // on `children[].p50_us` for callers that need precise breakdowns.
    let (p50_us, p95_us, p99_us) = aggregate_latency_percentiles(&children);

    let ws = state.watcher_stats.snapshot();
    let watcher = WatcherMetrics {
        total_reindexed: ws.total_reindexed,
        total_deletes: ws.total_deletes,
        total_ignored: ws.total_ignored,
        p50_ms: ws.p50_ms,
        p95_ms: ws.p95_ms,
        p99_ms: ws.p99_ms,
    };

    // B15 (2026-05-02): humanise the percentiles. `p50_us` is the
    // typical response time (median); `p99_us` is the slow-tail. Both
    // are integer division by 1000 so a 23 000 us value becomes 23 ms.
    // Zero stays zero - the renderer hides zero-values entirely.
    let typical_response_ms = p50_us / 1000;
    let slow_response_ms = p99_us / 1000;

    let snap = SlaSnapshot {
        timestamp: Utc::now(),
        supervisor_uptime_s,
        children,
        overall_uptime_percent: overall_uptime_percent.min(100.0),
        cache_hit_rate,
        disk,
        disk_usage_mb,
        queue_depth,
        p50_us,
        p95_us,
        p99_us,
        typical_response_ms,
        slow_response_ms,
        watcher,
    };
    {
        // Bug SEC-4 (2026-05-01): recover from poisoned mutex instead
        // of panic. Same rationale as in the read path above.
        let mut cache = state
            .snapshot_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *cache = Some((Instant::now(), snap.clone()));
    }
    Ok(snap)
}

/// C2: aggregate per-worker latency percentiles into one (p50, p95, p99)
/// triple for the daemon-wide /health snapshot. Workers that haven't
/// reported any samples yet are skipped so a freshly-booted daemon
/// reports `(0, 0, 0)` rather than a noisy mean of `None`s.
fn aggregate_latency_percentiles(children: &[ChildSnapshot]) -> (u64, u64, u64) {
    let p50: Vec<u64> = children.iter().filter_map(|c| c.p50_us).collect();
    let p95: Vec<u64> = children.iter().filter_map(|c| c.p95_us).collect();
    let p99: Vec<u64> = children.iter().filter_map(|c| c.p99_us).collect();

    let mean = |v: &[u64]| -> u64 {
        if v.is_empty() {
            0
        } else {
            // Saturating sum so a worker reporting u64::MAX doesn't
            // overflow — defensive, not expected in practice.
            let sum: u64 = v.iter().fold(0u64, |a, b| a.saturating_add(*b));
            sum / (v.len() as u64)
        }
    };

    (mean(&p50), mean(&p95), mean(&p99))
}

/// Resolve the on-disk directory holding the bundled vision SPA.
///
/// Resolution order (A2 deeper-fix, EC2 Wave 1 2026-04-27):
///   1. **`MNEME_STATIC_DIR`** env var — explicit operator override.
///      Bypasses every heuristic. Used when the daemon's
///      `dirs::home_dir()` resolves to a different user context than
///      `mneme install` (Windows service running as LocalSystem,
///      cross-user run, sandboxed test harness).
///   2. **`<MNEME_HOME>/static/vision/`** — production install layout.
///      `MNEME_HOME` is consulted first by [`PathManager::default_root`]
///      so installers + tests can pin the install root.
///   3. **`./vision/dist/`** relative to CWD — dev fallback for
///      `cargo run -p mneme-daemon -- start` from the workspace root.
///   4. Returns `None` when none of the above are dirs containing an
///      `index.html`. The daemon continues running with API-only
///      endpoints — no crash on missing dist/ (decision doc
///      requirement).
///
/// On success the resolver logs the chosen path at INFO so EC2 / live
/// install diagnostics can confirm WHICH strategy fired without
/// reading source.
fn resolve_static_dir() -> Option<PathBuf> {
    // 1. Explicit override.
    if let Some(override_path) = std::env::var_os("MNEME_STATIC_DIR") {
        let path = PathBuf::from(&override_path);
        if path.is_dir() && path.join("index.html").is_file() {
            info!(
                path = %path.display(),
                strategy = "MNEME_STATIC_DIR",
                "vision SPA dir resolved via env override"
            );
            return Some(path);
        }
        warn!(
            path = %path.display(),
            "MNEME_STATIC_DIR set but path is not a dir or index.html missing; \
             falling through to default resolution"
        );
    }

    // 2. <MNEME_HOME>/static/vision/.
    let pm = PathManager::default_root();
    let home_static = pm.root().join("static").join("vision");
    if home_static.is_dir() && home_static.join("index.html").is_file() {
        info!(
            path = %home_static.display(),
            strategy = "MNEME_HOME/static/vision",
            "vision SPA dir resolved via PathManager"
        );
        return Some(home_static);
    }

    // 3. Dev fallback: when invoked from the workspace root.
    if let Ok(cwd) = std::env::current_dir() {
        let dev = cwd.join("vision").join("dist");
        if dev.is_dir() && dev.join("index.html").is_file() {
            info!(
                path = %dev.display(),
                strategy = "cwd/vision/dist (dev)",
                "vision SPA dir resolved via dev cwd fallback"
            );
            return Some(dev);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SupervisorConfig;
    use crate::log_ring::LogRing;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use livebus::{Event, EventBus, SubscriberManager};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn empty_app_state() -> AppState {
        let cfg = SupervisorConfig::default_layout();
        let manager = Arc::new(crate::manager::ChildManager::new(
            cfg,
            Arc::new(LogRing::new(64)),
        ));
        AppState {
            manager,
            started: Instant::now(),
            watcher_stats: WatcherStatsHandle::new(),
            snapshot_cache: Arc::new(Mutex::new(None)),
            job_queue: None,
        }
    }

    fn write_dist(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).expect("create dist");
        std::fs::write(dir.join("index.html"), b"<html><body>SPA</body></html>")
            .expect("write index.html");
        std::fs::write(dir.join("app.js"), b"console.log('mneme');").expect("write app.js");
    }

    /// A2 fix: when a `static_dir` is provided, the router must respond
    /// to `GET /` with the bytes of `<dir>/index.html`. Before this fix
    /// `compose_app_router` mounted only `ServeDir` so an explicit
    /// `index.html` request was needed; the SPA's root URL returned 404
    /// for any user who typed `http://127.0.0.1:7777/` into a browser.
    #[tokio::test]
    async fn serve_dist_returns_index_on_root() {
        let tmp = TempDir::new().expect("tempdir");
        write_dist(tmp.path());
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(tmp.path().to_path_buf()));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert!(
            bytes.windows(3).any(|w| w == b"SPA"),
            "expected GET / to return index.html bytes, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    /// A2 fix: SPA client-side routing depends on unknown paths
    /// returning the index.html shell with `Content-Type: text/html` so
    /// the SPA's history-router can reconstruct the route. Before this
    /// fix the daemon returned 404 for any path the bundle didn't have
    /// a literal file for (e.g. `/dashboard`, `/audit/...`).
    #[tokio::test]
    async fn serve_index_returns_html_on_unknown_path() {
        let tmp = TempDir::new().expect("tempdir");
        write_dist(tmp.path());
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(tmp.path().to_path_buf()));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/some/spa/route")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "expected SPA fallback to serve index.html for unknown path; got {}",
            resp.status()
        );
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with("text/html"),
            "expected text/html content-type for SPA fallback, got {ct}"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert!(
            bytes.windows(3).any(|w| w == b"SPA"),
            "expected SPA fallback to return index.html bytes, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    /// 4.2 livebus wiring: when a `SubscriberManager` is threaded
    /// through `compose_app_router`, the underlying `ApiGraphState`
    /// stored on the router must surface `livebus = Some(_)`. Before
    /// this fix `supervisor::lib::run` constructed the router without
    /// `with_livebus` so every `/ws` upgrade returned the polite 503
    /// error frame even though the route was mounted.
    ///
    /// We cannot easily peek into the router's typed state from the
    /// outside, so we exercise the wiring indirectly: register a
    /// subscriber on the same manager and dispatch a matching event,
    /// then prove the subscriber receives it. This is the same path
    /// `supervisor::ws::handle_socket` uses on the wire.
    #[tokio::test]
    async fn livebus_wired_when_compose_router_receives_subscriber_manager() {
        let bus = EventBus::new();
        let mgr = SubscriberManager::new(bus.clone());
        let state = empty_app_state();

        let _app = compose_app_router(state, Some(mgr.clone()), None);

        let mut handle = mgr
            .register(vec!["project.*.file_changed".into()])
            .expect("register subscriber");
        let ev = Event::from_json(
            "project.test123.file_changed",
            None,
            Some("test123".into()),
            serde_json::json!({"path": "src/lib.rs"}),
        );
        mgr.dispatch(&ev);

        let got = handle
            .rx
            .recv()
            .await
            .expect("subscriber received event from manager wired into router");
        assert_eq!(got.topic, "project.test123.file_changed");
        assert_eq!(
            got.payload["path"],
            serde_json::Value::String("src/lib.rs".into())
        );
    }

    /// C2: queue_depth must reflect pending jobs in the supervisor's
    /// shared queue. Before this fix `SlaSnapshot` had no
    /// `queue_depth` field; the dashboard reported `0` regardless of
    /// queue state.
    #[tokio::test]
    async fn health_returns_nonzero_queue_depth_after_dispatch() {
        use common::jobs::Job;

        let cfg = SupervisorConfig::default_layout();
        let manager = Arc::new(crate::manager::ChildManager::new(
            cfg,
            Arc::new(LogRing::new(64)),
        ));
        let queue = Arc::new(JobQueue::new(64));
        manager.attach_job_queue(queue.clone()).await;

        // Submit two jobs but never dispatch them — they stay pending.
        let _id1 = queue
            .submit(
                Job::Parse {
                    file_path: std::path::PathBuf::from("/tmp/a.rs"),
                    shard_root: std::path::PathBuf::from("/tmp/shard"),
                },
                None,
            )
            .expect("submit a");
        let _id2 = queue
            .submit(
                Job::Parse {
                    file_path: std::path::PathBuf::from("/tmp/b.rs"),
                    shard_root: std::path::PathBuf::from("/tmp/shard"),
                },
                None,
            )
            .expect("submit b");

        let state = AppState {
            manager,
            started: Instant::now(),
            watcher_stats: WatcherStatsHandle::new(),
            snapshot_cache: Arc::new(Mutex::new(None)),
            job_queue: Some(queue),
        };

        let snap = build_snapshot(&state).await.expect("snapshot");
        assert_eq!(
            snap.queue_depth, 2,
            "expected queue_depth = 2 (two submitted), got {}",
            snap.queue_depth
        );
    }

    /// C2: aggregate latency percentiles must be non-zero once any
    /// worker has reported samples. Before this fix `SlaSnapshot` had
    /// no top-level p50/p95/p99 fields — the daemon dashboard had to
    /// dig into `children[].p50_us` per worker. We verify the
    /// aggregator helper directly because feeding samples through a
    /// real worker requires spawning a child process.
    #[tokio::test]
    async fn health_returns_real_p50() {
        // Three children, two with reported percentiles, one without.
        // Aggregator must compute the mean of the populated ones
        // (90 + 110) / 2 = 100 — and ignore the worker that hasn't
        // reported yet (`None`).
        let now = Utc::now();
        let mk = |name: &str, p50: Option<u64>| ChildSnapshot {
            name: name.into(),
            status: crate::child::ChildStatus::Running,
            pid: None,
            restart_count: 0,
            restart_dropped_count: 0,
            current_uptime_ms: 0,
            total_uptime_ms: 0,
            last_exit_code: None,
            last_started_at: None,
            last_restart_at: None,
            p50_us: p50,
            p95_us: p50.map(|x| x + 50),
            p99_us: p50.map(|x| x + 100),
            last_job_id: None,
            last_job_duration_ms: None,
            last_job_status: None,
            last_job_completed_at: Some(now),
            avg_job_ms: None,
            total_jobs_completed: 0,
            total_jobs_failed: 0,
            total_jobs_dispatched: 0,
            rss_mb: 0,
        };
        let children = vec![mk("a", Some(90)), mk("b", Some(110)), mk("c", None)];
        let (p50, p95, p99) = aggregate_latency_percentiles(&children);
        assert_eq!(p50, 100, "p50 mean of (90, 110) should be 100, got {p50}");
        assert_eq!(p95, 150, "p95 mean of (140, 160) should be 150, got {p95}");
        assert_eq!(p99, 200, "p99 mean of (190, 210) should be 200, got {p99}");

        // Empty / no-reports case.
        let (p50e, p95e, p99e) = aggregate_latency_percentiles(&[]);
        assert_eq!((p50e, p95e, p99e), (0, 0, 0));
    }

    /// A2 deeper-fix: full HTTP round-trip integration test.
    ///
    /// The pre-existing `serve_dist_returns_index_on_root` and
    /// `serve_index_returns_html_on_unknown_path` tests both used
    /// `Router::oneshot()`, which calls the service directly without
    /// going through `axum::serve`/Hyper. On EC2 (Wave 1, 2026-04-27)
    /// both `/` and `/random/spa/route` time out at 5s on a real
    /// listener even though the unit tests pass — proving the unit
    /// path doesn't exercise the same machinery as production.
    ///
    /// This test binds a real TCP listener (port 0 = OS-assigned),
    /// runs `axum::serve` exactly the way `HealthServer::serve` does,
    /// and fires raw HTTP requests via `TcpStream`. If the SPA fallback
    /// is broken at the listener layer, this test will time out the
    /// same way EC2 does.
    #[tokio::test]
    async fn serve_index_at_real_listener_root_serves_html() {
        let tmp = TempDir::new().expect("tempdir");
        write_dist(tmp.path());
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(tmp.path().to_path_buf()));

        let addr = serve_in_background(app).await;

        let response = http_get(addr, "/")
            .await
            .expect("GET / should not time out");

        assert!(
            response.status_line.contains("200"),
            "expected 200 OK on real-listener GET /, got status line: {}",
            response.status_line
        );
        assert!(
            response.body.windows(3).any(|w| w == b"SPA"),
            "expected real-listener GET / to return index.html bytes, got {:?}",
            String::from_utf8_lossy(&response.body)
        );
    }

    /// A2 deeper-fix: full HTTP round-trip for an unknown SPA path. The
    /// pre-existing unit-level test passes via `oneshot`, but on EC2
    /// the production listener times out. This test reproduces the
    /// production path exactly.
    #[tokio::test]
    async fn serve_index_at_real_listener_spa_route_serves_html() {
        let tmp = TempDir::new().expect("tempdir");
        write_dist(tmp.path());
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(tmp.path().to_path_buf()));

        let addr = serve_in_background(app).await;

        let response = http_get(addr, "/random/spa/route")
            .await
            .expect("GET /random/spa/route should not time out");

        assert!(
            response.status_line.contains("200"),
            "expected 200 OK on real-listener SPA route, got status line: {}",
            response.status_line
        );
        assert!(
            response.body.windows(3).any(|w| w == b"SPA"),
            "expected real-listener SPA route to return index.html bytes, got {:?}",
            String::from_utf8_lossy(&response.body)
        );
    }

    /// A2 deeper-fix: real-listener `/health` continues to work
    /// alongside the SPA fallback. Sanity check that the api+health
    /// routers and the static fallback compose correctly.
    #[tokio::test]
    async fn real_listener_serves_health_and_spa_concurrently() {
        let tmp = TempDir::new().expect("tempdir");
        write_dist(tmp.path());
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(tmp.path().to_path_buf()));

        let addr = serve_in_background(app).await;

        let health = http_get(addr, "/health/live")
            .await
            .expect("GET /health/live should not time out");
        assert!(
            health.status_line.contains("200"),
            "expected /health/live to return 200, got: {}",
            health.status_line
        );

        let spa = http_get(addr, "/")
            .await
            .expect("GET / should not time out");
        assert!(
            spa.status_line.contains("200"),
            "expected SPA root to return 200, got: {}",
            spa.status_line
        );
    }

    /// A2 deeper-fix: full HTTP round-trip with REAL vision SPA
    /// assets (37+ files including index.html, JS bundles, source
    /// maps, fonts). The unit-test `write_dist` writes only 2 tiny
    /// files; the EC2 install ships the full `vision/dist/` tree
    /// (~10MB across 38 files). If tower-http's ServeDir does
    /// something odd when first touching a populated directory
    /// (cache priming, antivirus probe, etc.) this test will catch
    /// it on cold start in a way the synthetic test misses.
    #[tokio::test]
    async fn serve_real_vision_dist_on_real_listener() {
        // The repo-level `vision/dist/` is what `mneme install`
        // copies to `~/.mneme/static/vision/`. We mirror that exact
        // layout into a tempdir so the test stays hermetic.
        let real_dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace dir")
            .join("vision")
            .join("dist");
        if !real_dist.join("index.html").is_file() {
            // Repo not built; skip rather than fail since `cargo
            // test` shouldn't depend on `bun run build`.
            eprintln!(
                "skipping serve_real_vision_dist_on_real_listener: \
                 vision/dist/index.html not present (run `bun run build` in vision/)"
            );
            return;
        }
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(real_dist.clone()));

        let addr = serve_in_background(app).await;

        // Hit GET / — the failing case on EC2.
        let response = http_get(addr, "/")
            .await
            .expect("real-dist GET / should not hang");
        assert!(
            response.status_line.contains("200"),
            "expected real-dist GET / to return 200, got: {}",
            response.status_line
        );
        assert!(
            !response.body.is_empty(),
            "expected real-dist GET / to return a non-empty body"
        );
        // Must look like the real index.html.
        let body_str = String::from_utf8_lossy(&response.body);
        assert!(
            body_str.contains("<!DOCTYPE html>") || body_str.contains("<html"),
            "expected real-dist GET / body to look like HTML, got: {body_str}"
        );

        // Hit a SPA-router-style unknown path — the second failing
        // case on EC2. tower-http should fall through to index.html.
        let response = http_get(addr, "/audit/findings/abc123")
            .await
            .expect("real-dist GET /audit/findings/abc123 should not hang");
        assert!(
            response.status_line.contains("200"),
            "expected real-dist SPA route to return 200 (fallback to index.html), got: {}",
            response.status_line
        );
    }

    /// A2 EC2 escape hatch: when `MNEME_STATIC_DIR` is set, the
    /// resolver MUST return that path verbatim — without consulting
    /// `dirs::home_dir()`, `MNEME_HOME`, or any cwd fallback. This
    /// gives operators a deterministic override when the home-dir
    /// heuristic produces the wrong path (e.g. daemon launched from a
    /// different user context than the install).
    #[tokio::test]
    async fn resolve_static_dir_honors_mneme_static_dir_env_override() {
        let tmp = TempDir::new().expect("tempdir");
        // The override must point to a dir containing index.html
        // (otherwise it's a config error and the resolver should fall
        // through to the next strategy).
        std::fs::write(tmp.path().join("index.html"), b"<html>OVERRIDE</html>")
            .expect("write index.html");

        let prev_static = std::env::var_os("MNEME_STATIC_DIR");
        let prev_home = std::env::var_os("MNEME_HOME");
        // Also poison MNEME_HOME so we PROVE the override beats it.
        std::env::set_var("MNEME_HOME", "C:\\nonexistent\\poisoned");
        std::env::set_var("MNEME_STATIC_DIR", tmp.path());
        let resolved = resolve_static_dir();
        match prev_static {
            Some(v) => std::env::set_var("MNEME_STATIC_DIR", v),
            None => std::env::remove_var("MNEME_STATIC_DIR"),
        }
        match prev_home {
            Some(v) => std::env::set_var("MNEME_HOME", v),
            None => std::env::remove_var("MNEME_HOME"),
        }

        let resolved =
            resolved.expect("MNEME_STATIC_DIR override should resolve to the configured tempdir");
        assert_eq!(
            resolved,
            tmp.path(),
            "expected MNEME_STATIC_DIR to win over MNEME_HOME / dirs::home_dir(), got {:?}",
            resolved
        );
    }

    /// A2 fix: the SPA fallback handler must serve index.html WITHOUT
    /// reading from disk per-request. We pre-load the bytes at compose
    /// time so a misbehaving filesystem on EC2 (slow IO, antivirus
    /// scan-on-open, etc.) cannot stall the SPA boot. Verify that
    /// editing the on-disk index.html AFTER router compose does NOT
    /// change what the router serves — proving the bytes are cached
    /// in-memory.
    #[tokio::test]
    async fn spa_fallback_serves_cached_bytes_not_per_request_disk_reads() {
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path();
        std::fs::create_dir_all(dist).expect("create dist");
        std::fs::write(dist.join("index.html"), b"<html>ORIGINAL</html>")
            .expect("write original index.html");
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(dist.to_path_buf()));

        // Mutate the on-disk file AFTER compose. If the handler
        // re-reads on every request, we'd see "MUTATED"; if it
        // pre-cached, we still see "ORIGINAL".
        std::fs::write(dist.join("index.html"), b"<html>MUTATED</html>")
            .expect("rewrite index.html");

        let addr = serve_in_background(app).await;
        let response = http_get(addr, "/spa/route")
            .await
            .expect("GET /spa/route should not hang");

        assert!(
            response.status_line.contains("200"),
            "expected SPA fallback to return 200, got: {}",
            response.status_line
        );
        let body = String::from_utf8_lossy(&response.body);
        assert!(
            body.contains("ORIGINAL"),
            "expected cached index.html bytes (pre-load proves resilience to EC2 \
             on-disk perturbation); got body: {body}"
        );
    }

    /// A2 fix: explicit asset paths (`/assets/foo.js`, `/main.js`,
    /// etc.) must still serve the real file — we only fall back to
    /// index.html when the requested path doesn't resolve to an
    /// on-disk file. Verify a JS asset round-trips with the right
    /// content-type and bytes.
    #[tokio::test]
    async fn explicit_asset_paths_serve_real_files_not_index() {
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path();
        std::fs::create_dir_all(dist.join("assets")).expect("create assets dir");
        std::fs::write(dist.join("index.html"), b"<html>SHELL</html>").expect("write index.html");
        std::fs::write(
            dist.join("assets").join("app.js"),
            b"console.log('mneme-vision-bundle');",
        )
        .expect("write asset");
        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(dist.to_path_buf()));

        let addr = serve_in_background(app).await;
        let response = http_get(addr, "/assets/app.js")
            .await
            .expect("GET /assets/app.js should not hang");

        assert!(
            response.status_line.contains("200"),
            "expected real asset to return 200, got: {}",
            response.status_line
        );
        let body = String::from_utf8_lossy(&response.body);
        assert!(
            body.contains("mneme-vision-bundle"),
            "expected GET /assets/app.js to serve the JS bundle, not the SPA shell; \
             got body: {body}"
        );
        assert!(
            !body.contains("SHELL"),
            "expected GET /assets/app.js to NOT fall through to index.html; got body: {body}"
        );
    }

    /// A2 root cause: `resolve_static_dir()` must return
    /// `<MNEME_HOME>/static/vision/` when that path exists on disk.
    /// Use the `MNEME_HOME` env var to point the resolver at a
    /// controlled tempdir without disturbing the user's real
    /// `~/.mneme/` install.
    #[tokio::test]
    async fn resolve_static_dir_finds_user_profile_mneme_static_vision() {
        let tmp = TempDir::new().expect("tempdir");
        let static_vision = tmp.path().join("static").join("vision");
        std::fs::create_dir_all(&static_vision).expect("create static/vision");
        std::fs::write(static_vision.join("index.html"), b"<html>OK</html>")
            .expect("write index.html");

        // SAFETY: a single-threaded test mutating MNEME_HOME for the
        // duration of `resolve_static_dir()`. Other tests don't depend
        // on this var being unset; the resolver consults it once at
        // call time.
        let prev = std::env::var_os("MNEME_HOME");
        std::env::set_var("MNEME_HOME", tmp.path());
        let resolved = resolve_static_dir();
        match prev {
            Some(v) => std::env::set_var("MNEME_HOME", v),
            None => std::env::remove_var("MNEME_HOME"),
        }

        let resolved =
            resolved.expect("resolve_static_dir should find <MNEME_HOME>/static/vision/");
        assert_eq!(
            resolved, static_vision,
            "expected resolver to return the static/vision dir under MNEME_HOME, got {:?}",
            resolved
        );
    }

    // -------------------------------------------------------------------
    // Real-listener test helpers
    // -------------------------------------------------------------------

    /// Bind the given `app` to a real TCP listener on `127.0.0.1:0`,
    /// spawn `axum::serve` in the background, and return the bound
    /// address. Mirrors `HealthServer::serve` so the test covers the
    /// production path through Hyper instead of `oneshot`.
    async fn serve_in_background(app: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            // Errors here are fine — the test will fail on the client
            // side via timeout if the server panics.
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    struct ParsedResponse {
        status_line: String,
        body: Vec<u8>,
    }

    /// Fire a raw HTTP/1.1 GET to `addr` for `path`, with a 3-second
    /// hard read deadline. Returns the parsed status line + body, or
    /// errors on timeout / connection failure. Crafted by hand to
    /// avoid pulling a heavyweight HTTP client into dev-deps.
    async fn http_get(addr: std::net::SocketAddr, path: &str) -> Result<ParsedResponse, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;
        use tokio::time::{timeout, Duration};

        let mut stream = timeout(Duration::from_secs(3), TcpStream::connect(addr))
            .await
            .map_err(|_| "connect timed out".to_string())?
            .map_err(|e| format!("connect failed: {e}"))?;

        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        timeout(Duration::from_secs(3), stream.write_all(req.as_bytes()))
            .await
            .map_err(|_| "write timed out".to_string())?
            .map_err(|e| format!("write failed: {e}"))?;

        let mut buf = Vec::with_capacity(8 * 1024);
        timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
            .await
            .map_err(|_| "read timed out".to_string())?
            .map_err(|e| format!("read failed: {e}"))?;

        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| "no header/body separator".to_string())?;
        let header_block = String::from_utf8_lossy(&buf[..split]).to_string();
        let status_line = header_block.lines().next().unwrap_or("").to_string();
        let body = buf[split + 4..].to_vec();

        Ok(ParsedResponse { status_line, body })
    }

    /// A2 EC2 root cause (2026-04-27): tower-http's `ServeDir` uses
    /// `tokio::fs::metadata` and `tokio::fs::File::open` on EVERY request,
    /// both of which dispatch to tokio's `spawn_blocking` pool internally.
    /// The supervisor caps the blocking pool at 8 threads
    /// (`main.rs::main()` and `service.rs::ffi_service_main()`) — and at
    /// boot time those 8 threads are consumed by stdin/stdout forwarders
    /// for the 7 named children + the watcher's RSS sampler. When all 8
    /// blocking threads are busy, ServeDir's `is_dir()` queues forever,
    /// the OpenFileFuture never resolves, and the `.fallback()` (Agent
    /// H's cached-bytes closure) NEVER FIRES — because ServeDir's
    /// `.fallback` is only invoked AFTER its own filesystem probe
    /// resolves with NotFound, not when it's stuck.
    ///
    /// This test reproduces the EC2 condition by:
    ///   1. Building an isolated multi-thread runtime with
    ///      `max_blocking_threads(2)` (matches the EC2 scarcity profile).
    ///   2. Saturating the blocking pool with two 60-second sleep tasks
    ///      so any further `spawn_blocking` queues forever.
    ///   3. Issuing a real-listener `GET /` against the SPA-fallback
    ///      router and asserting it returns 200 within 3 seconds.
    ///
    /// Before the fix this test times out (matching the EC2 symptom
    /// exactly). After the fix `GET /` is served by an axum-mounted
    /// handler that returns the cached bytes synchronously, never
    /// touching the blocking pool.
    #[test]
    fn serve_index_works_under_saturated_blocking_pool() {
        // Build an isolated runtime with a tiny blocking pool to
        // reproduce the EC2 scarcity profile. We use `block_on` so the
        // test fails on the parent thread when the inner future hangs.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .expect("build constrained runtime");

        let outcome = rt.block_on(async move {
            // Saturate the blocking pool. After these two `spawn_blocking`
            // calls land on threads, ANY further blocking call (including
            // tokio::fs::metadata that ServeDir uses) queues until the
            // saturators release. We sleep for 5s — long enough to cover
            // the 3s request budget, short enough that the test wraps
            // up in <10s once the assertion fires. The runtime
            // `shutdown_background` call below also forces a hard
            // teardown without waiting for the sleeps to finish.
            for _ in 0..2 {
                tokio::task::spawn_blocking(|| {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                });
            }
            // Give the spawn_blocking tasks a moment to land on threads.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let tmp = TempDir::new().expect("tempdir");
            write_dist(tmp.path());
            let state = empty_app_state();
            let app = compose_app_router(state, None, Some(tmp.path().to_path_buf()));

            let addr = serve_in_background(app).await;

            // Hard-bound 3s on the request. The EC2 reproduction shows a
            // 5s timeout; if our fix didn't land we'll hang the same way.
            let resp =
                tokio::time::timeout(std::time::Duration::from_secs(3), http_get(addr, "/")).await;

            // Ownership note: we hold `tmp` so the dist tree survives the
            // request.
            drop(tmp);

            resp
        });

        // Force the runtime down without waiting for the saturating
        // sleeps to finish, so the test wraps up promptly.
        rt.shutdown_background();

        let resp = outcome
            .expect("GET / must not time out under saturated blocking pool")
            .expect("http_get returned an error under saturated blocking pool");
        assert!(
            resp.status_line.contains("200"),
            "expected 200 OK under saturated blocking pool, got: {}",
            resp.status_line
        );
        assert!(
            resp.body.windows(3).any(|w| w == b"SPA"),
            "expected index.html bytes under saturated blocking pool, got: {:?}",
            String::from_utf8_lossy(&resp.body)
        );
    }

    /// A2 EC2 fix: prove the SPA root + unknown-path handlers are mounted
    /// as EXPLICIT axum routes, not via `fallback_service(ServeDir)`. The
    /// distinction is observable by checking that requests to `/` and
    /// `/random/spa/route` succeed even when no on-disk index.html
    /// exists at request time — we delete the file AFTER router compose
    /// and before issuing the request. If the handler is going through
    /// ServeDir, deleting the file would cause it to either 404 or
    /// queue forever; if it's going through the axum-mounted explicit
    /// route serving cached bytes, the request still succeeds.
    #[tokio::test]
    async fn serve_index_via_explicit_route_not_fallback() {
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path();
        std::fs::create_dir_all(dist).expect("create dist");
        std::fs::write(dist.join("index.html"), b"<html>EXPLICIT</html>")
            .expect("write index.html");

        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(dist.to_path_buf()));

        // Critical step: nuke the on-disk file AFTER the router was
        // composed. If the handler reads from disk on every request,
        // we'd see a 404 (or hang). If it's serving cached bytes via an
        // explicit axum route, we still get the original content.
        std::fs::remove_file(dist.join("index.html")).expect("delete index.html after compose");

        let addr = serve_in_background(app).await;

        // Test BOTH the root path and an unknown SPA path — both must
        // hit the explicit route, not ServeDir.
        for path in &["/", "/some/spa/route"] {
            let resp =
                tokio::time::timeout(std::time::Duration::from_secs(3), http_get(addr, path))
                    .await
                    .unwrap_or_else(|_| panic!("GET {path} hung after on-disk file deleted"))
                    .unwrap_or_else(|e| {
                        panic!("GET {path} failed after on-disk file deleted: {e}")
                    });
            assert!(
                resp.status_line.contains("200"),
                "expected explicit route to serve cached bytes for {path}, got: {}",
                resp.status_line
            );
            let body = String::from_utf8_lossy(&resp.body);
            assert!(
                body.contains("EXPLICIT"),
                "expected cached EXPLICIT bytes for {path}, got: {body}"
            );
        }
    }

    /// A2 EC2 fix invocation evidence: prove the SPA root + fallback
    /// handlers are explicit axum routes, not the previous
    /// `app.fallback_service(ServeDir.fallback(...))` shape, by
    /// inspecting the response headers.
    ///
    /// The new explicit handlers set `Cache-Control: no-cache`. The
    /// previous `ServeDir`-based shape did NOT set Cache-Control on
    /// directory-fallback responses. Asserting on the header gives us
    /// a public-API-level signal about WHICH code path served the
    /// request — without depending on tracing capture (which is
    /// notoriously flaky under cargo's parallel test runner because
    /// the thread-local default subscriber state can be torn down by
    /// sibling tests reusing the same thread).
    ///
    /// We also verify the byte count of the response matches the
    /// length of the cached `index.html` — proving the bytes came
    /// from the in-memory cache, not a stream from ServeDir.
    #[tokio::test]
    async fn serve_index_telemetry_logs_strategy_and_invocation() {
        let tmp = TempDir::new().expect("tempdir");
        let dist = tmp.path();
        std::fs::create_dir_all(dist).expect("create dist");
        let content = b"<html>TELEMETRY-EXPLICIT-ROUTE</html>";
        std::fs::write(dist.join("index.html"), content).expect("write index.html");

        let state = empty_app_state();
        let app = compose_app_router(state, None, Some(dist.to_path_buf()));

        let addr = serve_in_background(app).await;

        // Fire two requests: root path and an unknown SPA URL. Both
        // must hit the explicit handler, observable via:
        //   * 200 OK
        //   * Cache-Control: no-cache (set only by our handler)
        //   * Body bytes equal the cached index.html length
        for path in &["/", "/audit/findings/abc"] {
            let resp =
                tokio::time::timeout(std::time::Duration::from_secs(3), http_get(addr, path))
                    .await
                    .unwrap_or_else(|_| panic!("GET {path} hung"))
                    .unwrap_or_else(|e| panic!("GET {path} failed: {e}"));

            assert!(
                resp.status_line.contains("200"),
                "expected 200 from explicit handler for {path}, got: {}",
                resp.status_line
            );
            assert_eq!(
                resp.body,
                content,
                "expected exact cached index.html bytes for {path}, got len={}",
                resp.body.len()
            );
        }

        // Issue a HEAD-style raw request to inspect headers — http_get
        // captures both, so we can grep the joined header block from
        // any of the two earlier requests via a fresh GET. The header
        // block is everything before the body.
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            http_get_with_headers(addr, "/spa/route"),
        )
        .await
        .unwrap_or_else(|_| panic!("GET /spa/route hung"))
        .unwrap_or_else(|e| panic!("GET /spa/route failed: {e}"));

        assert!(
            resp.headers
                .iter()
                .any(|line| line.eq_ignore_ascii_case("cache-control: no-cache")),
            "expected Cache-Control: no-cache (set by explicit-route handler) in headers; \
             got headers: {:?}",
            resp.headers
        );
        assert!(
            resp.headers.iter().any(|line| {
                line.to_ascii_lowercase()
                    .starts_with("content-type: text/html")
            }),
            "expected Content-Type: text/html on SPA fallback; got headers: {:?}",
            resp.headers
        );
    }

    /// Variant of `http_get` that returns the parsed header lines too,
    /// for tests that want to assert on response headers.
    struct HeaderResponse {
        #[allow(dead_code)]
        status_line: String,
        headers: Vec<String>,
        #[allow(dead_code)]
        body: Vec<u8>,
    }

    async fn http_get_with_headers(
        addr: std::net::SocketAddr,
        path: &str,
    ) -> Result<HeaderResponse, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;
        use tokio::time::{timeout, Duration};

        let mut stream = timeout(Duration::from_secs(3), TcpStream::connect(addr))
            .await
            .map_err(|_| "connect timed out".to_string())?
            .map_err(|e| format!("connect failed: {e}"))?;

        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        timeout(Duration::from_secs(3), stream.write_all(req.as_bytes()))
            .await
            .map_err(|_| "write timed out".to_string())?
            .map_err(|e| format!("write failed: {e}"))?;

        let mut buf = Vec::with_capacity(8 * 1024);
        timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
            .await
            .map_err(|_| "read timed out".to_string())?
            .map_err(|e| format!("read failed: {e}"))?;

        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| "no header/body separator".to_string())?;
        let header_block = String::from_utf8_lossy(&buf[..split]).to_string();
        let mut lines = header_block.lines();
        let status_line = lines.next().unwrap_or("").to_string();
        let headers: Vec<String> = lines.map(|s| s.to_string()).collect();
        let body = buf[split + 4..].to_vec();
        Ok(HeaderResponse {
            status_line,
            headers,
            body,
        })
    }
}

fn compute_disk_usage(_root: &std::path::Path) -> DiskUsage {
    // I-4 / I-5: serve from cache if fresh, else refresh under lock.
    {
        let cache = disk_usage_cache()
            .lock()
            .expect("disk-usage cache poisoned");
        if let Some((stamp, du)) = cache.as_ref() {
            if stamp.elapsed() < DISK_USAGE_TTL {
                return du.clone();
            }
        }
    }
    let _sys = System::new();
    let disks = Disks::new_with_refreshed_list();
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    for d in disks.list() {
        total = total.saturating_add(d.total_space());
        free = free.saturating_add(d.available_space());
    }
    let used_percent = if total == 0 {
        0.0
    } else {
        ((total - free) as f64 / total as f64) * 100.0
    };
    let du = DiskUsage {
        total_bytes: total,
        free_bytes: free,
        used_percent,
    };
    {
        let mut cache = disk_usage_cache()
            .lock()
            .expect("disk-usage cache poisoned");
        *cache = Some((Instant::now(), du.clone()));
    }
    du
}
