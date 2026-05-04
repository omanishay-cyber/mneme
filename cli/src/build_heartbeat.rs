//! Build-pipeline progress heartbeat (B-017).
//!
//! ## Why
//!
//! `mneme build` orchestrates a long pipeline:
//! `walk → multimodal → parse → embed → graph → persist`.
//! Phases 3 (parse) and 4 (embed), plus the betweenness / Leiden /
//! architecture passes, do most of their work in tight CPU loops with
//! NO per-file output. On a 5,000-file project the user sees the
//! Tesseract-disabled multimodal warnings, then 5–20 minutes of
//! perfect silence. They assume the process hung and Ctrl-C — which
//! kills the build mid-write and corrupts the WAL. We hit this on
//! local hardware on 2026-04-29.
//!
//! ## What this module gives you
//!
//! A drop-in [`Heartbeat`] handle that:
//!
//! 1. Spawns a tokio interval task emitting a status line every 30s
//!    (configurable via [`Heartbeat::interval`]).
//! 2. Lets the working code call [`Heartbeat::record_processed`] /
//!    [`Heartbeat::set_phase`] / [`Heartbeat::add_total`] from a hot
//!    loop without any locks (atomics only).
//! 3. Cancels the timer task automatically on `Drop` — RAII so a
//!    panic / `?` short-circuit doesn't leak the background task.
//! 4. Honors a `quiet` flag passed at construction — the timer task
//!    stays armed (so the `Drop` lifecycle is uniform) but emits
//!    nothing.
//!
//! ## Why a side task instead of inline `println!`?
//!
//! The whole point of B-017 is to "speak when otherwise silent". An
//! inline progress line that fires per-file is not enough — the
//! embedding pass processes 64 nodes per `embed_batch`, and a single
//! batch can take 60 s on a CPU-only `bge-small` install. The user
//! must see *something* every 30 seconds even when zero per-item
//! events fired in that window.
//!
//! ## Output format
//!
//! ```text
//! [01:34 elapsed] phase=parse processed=512/4218 rate=327/s
//! [02:04 elapsed] phase=parse processed=798/4218 rate=259/s
//! [02:34 elapsed] phase=embed processed=64/3001 rate=21/s
//! ```
//!
//! When `total` is 0 (unknown), the `/<total>` segment is omitted.
//! When `processed` is unchanged from the previous tick, the line
//! still fires — silence is the bug.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tracing::info;

/// How often the heartbeat task wakes by default. Tunable via
/// [`Heartbeat::with_interval`] — but the prompt for B-017 specifies
/// "≥1 status line per 30 seconds during parse/embed/graph", so the
/// floor in production is 30s.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// One progress "tick" line emitted by the heartbeat task. Captured by
/// tests via [`HeartbeatSink::Capture`]; in production we emit via
/// `println!` + `tracing::info!`.
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatLine {
    /// Wall-clock seconds since the heartbeat was started.
    pub elapsed_secs: u64,
    /// The current phase name (e.g. `"parse"`, `"embed"`,
    /// `"graph-leiden"`, `"graph-betweenness"`).
    pub phase: String,
    /// Items processed so far, as observed by the working code.
    pub processed: u64,
    /// Optional total. `0` means "unknown" — the formatter omits the
    /// `/<total>` segment in that case.
    pub total: u64,
    /// Items per second, computed from `processed / elapsed_secs`.
    /// Returns `0.0` for the first tick when `elapsed_secs` is 0.
    pub rate: f64,
}

impl HeartbeatLine {
    /// Render the line in the human-readable format the user sees in
    /// their terminal. Format pinned by `format_includes_phase_and_processed`
    /// in the test module — keep `phase=` / `processed=` / `rate=`
    /// segments stable so log scrapers can parse them.
    pub fn format(&self) -> String {
        let mins = self.elapsed_secs / 60;
        let secs = self.elapsed_secs % 60;
        let processed_segment = if self.total > 0 {
            format!("processed={}/{}", self.processed, self.total)
        } else {
            format!("processed={}", self.processed)
        };
        format!(
            "[{:02}:{:02} elapsed] phase={} {} rate={:.1}/s",
            mins, secs, self.phase, processed_segment, self.rate
        )
    }
}

/// Where heartbeat lines go. Production wiring uses [`HeartbeatSink::Stdout`]
/// (which also mirrors to `tracing::info!` for JSON-log capture). Tests
/// — both the in-crate `#[cfg(test)]` module AND external integration
/// tests under `cli/tests/` — use [`HeartbeatSink::Capture`] to assert
/// on exact emitted lines. The `Capture` variant is `pub` rather than
/// `#[cfg(test)]`-gated because integration tests are compiled against
/// the lib WITHOUT `cfg(test)`, and they need this seam.
#[derive(Debug)]
pub enum HeartbeatSink {
    /// Print to stdout AND emit a `tracing::info!` event with
    /// `target="build.heartbeat"`. The default for `mneme build`.
    Stdout,
    /// Push the formatted line into a shared `Vec<HeartbeatLine>` so
    /// tests can assert on exact emitted lines. The `Mutex` is
    /// required because the heartbeat task runs on a separate tokio
    /// worker.
    Capture(Arc<Mutex<Vec<HeartbeatLine>>>),
}

impl HeartbeatSink {
    fn emit(&self, line: &HeartbeatLine) {
        match self {
            HeartbeatSink::Stdout => {
                println!("  {}", line.format());
                info!(
                    target: "build.heartbeat",
                    phase = %line.phase,
                    processed = line.processed,
                    total = line.total,
                    rate = line.rate,
                    elapsed_secs = line.elapsed_secs,
                    "{}",
                    line.format()
                );
            }
            HeartbeatSink::Capture(buf) => {
                if let Ok(mut g) = buf.lock() {
                    g.push(line.clone());
                }
            }
        }
    }
}

/// Shared state accessed by both the working code (caller) and the
/// background timer task. Atomics avoid any lock on the hot path.
#[derive(Debug)]
struct Inner {
    started: Instant,
    /// Mutex around a `String` rather than an atomic because the phase
    /// is a free-form label and changes infrequently (once per pass).
    phase: Mutex<String>,
    processed: AtomicU64,
    total: AtomicU64,
    quiet: AtomicBool,
    cancelled: AtomicBool,
}

/// Active heartbeat handle. Held by the calling pipeline; dropping it
/// stops the background tick task. RAII so panics / early returns
/// don't leak.
#[derive(Debug)]
pub struct Heartbeat {
    inner: Arc<Inner>,
    task: Option<JoinHandle<()>>,
}

impl Heartbeat {
    /// Start a heartbeat for the named phase. `total = 0` means
    /// "unknown" — call [`Heartbeat::add_total`] later when the upper
    /// bound is discovered.
    ///
    /// MUST be called from inside a tokio runtime — internally spawns
    /// a `tokio::time::interval` task. `mneme build` is `#[tokio::main]`
    /// at the top level so this is satisfied for every build path.
    pub fn start(phase: impl Into<String>, total: u64, quiet: bool) -> Self {
        Self::start_with(phase, total, quiet, DEFAULT_INTERVAL, HeartbeatSink::Stdout)
    }

    /// Test seam: configurable interval + sink. Production uses
    /// [`Heartbeat::start`]; tests use this overload to drive the
    /// timer with `tokio::time::pause()` + `advance(31s)`.
    #[doc(hidden)]
    pub fn start_with(
        phase: impl Into<String>,
        total: u64,
        quiet: bool,
        interval: Duration,
        sink: HeartbeatSink,
    ) -> Self {
        let inner = Arc::new(Inner {
            started: Instant::now(),
            phase: Mutex::new(phase.into()),
            processed: AtomicU64::new(0),
            total: AtomicU64::new(total),
            quiet: AtomicBool::new(quiet),
            cancelled: AtomicBool::new(false),
        });

        let task_inner = Arc::clone(&inner);
        let task = tokio::spawn(async move {
            // Initial sleep so we don't immediately spam at t=0; the
            // first tick lands at +interval. Any phase that completes
            // within the interval window will never emit a heartbeat,
            // which is desirable — short-running phases don't need
            // progress chatter.
            let mut ticker = tokio::time::interval(interval);
            // Skip the first immediate tick that `tokio::time::interval`
            // emits at t=0.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if task_inner.cancelled.load(Ordering::Relaxed) {
                    break;
                }
                if task_inner.quiet.load(Ordering::Relaxed) {
                    continue;
                }
                let elapsed_secs = task_inner.started.elapsed().as_secs();
                let processed = task_inner.processed.load(Ordering::Relaxed);
                let total = task_inner.total.load(Ordering::Relaxed);
                let phase = task_inner
                    .phase
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                let rate = if elapsed_secs > 0 {
                    processed as f64 / elapsed_secs as f64
                } else {
                    0.0
                };
                let line = HeartbeatLine {
                    elapsed_secs,
                    phase,
                    processed,
                    total,
                    rate,
                };
                sink.emit(&line);
            }
        });

        Heartbeat {
            inner,
            task: Some(task),
        }
    }

    /// Update the processed-count atomic. Lock-free; safe to call from
    /// the hot loop on every iteration.
    pub fn record_processed(&self, n: u64) {
        self.inner.processed.store(n, Ordering::Relaxed);
    }

    /// Bump the total upward. Used by the parse-pass walker which
    /// discovers files incrementally. `add_total(0)` is a no-op.
    pub fn add_total(&self, n: u64) {
        if n > 0 {
            self.inner.total.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Replace the current total wholesale. Used when a pass starts
    /// and the upper bound is known up-front (e.g. embedding pass
    /// already has `with_text.len()` in hand).
    pub fn set_total(&self, n: u64) {
        self.inner.total.store(n, Ordering::Relaxed);
    }

    /// Switch to a new phase label. Used when the same heartbeat
    /// handle is re-used across pipeline stages (saves spawning N
    /// timer tasks for N stages).
    ///
    /// A1-026 (2026-05-04): also reset `total` to 0. Previously only
    /// `processed` was zeroed; the stale `total` from the previous
    /// phase persisted and the heartbeat output read e.g.
    /// "phase=graph-betweenness processed=0/12345" -- which the user
    /// reasonably interpreted as "12345 pending nodes, none done yet"
    /// when in reality the betweenness phase doesn't have a meaningful
    /// node-level count at all (it's a single graph computation).
    /// Callers that have a real total for the new phase MUST call
    /// `set_total(n)` AFTER `set_phase(...)`. Phases with no
    /// meaningful per-item count emit "processed=0/0" which the
    /// formatter renders as a phase label without the rate suffix.
    pub fn set_phase(&self, phase: impl Into<String>) {
        let new_phase = phase.into();
        if let Ok(mut g) = self.inner.phase.lock() {
            *g = new_phase;
        }
        // Reset processed when switching phase so the rate metric
        // reflects the NEW phase's throughput, not the previous one's.
        self.inner.processed.store(0, Ordering::Relaxed);
        // A1-026: also clear total so a stale parser-pass total of
        // 12345 doesn't bleed into a betweenness/leiden/embed phase
        // where the count is meaningless.
        self.inner.total.store(0, Ordering::Relaxed);
    }

    /// Stop the timer task explicitly. `Drop` calls this too; explicit
    /// stop is exposed so callers can wait for the abort to complete
    /// (e.g. before printing the build-summary line).
    pub fn stop(&mut self) {
        self.inner.cancelled.store(true, Ordering::Relaxed);
        if let Some(handle) = self.task.take() {
            handle.abort();
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The heartbeat formatter is the primary user-visible contract.
    /// Pin the string so log scrapers / dashboards / future operators
    /// don't get a silent format change.
    #[test]
    fn format_includes_phase_and_processed() {
        let line = HeartbeatLine {
            elapsed_secs: 94,
            phase: "parse".into(),
            processed: 512,
            total: 4218,
            rate: 327.0 / 6.0,
        };
        let s = line.format();
        assert!(s.contains("[01:34 elapsed]"), "elapsed mm:ss missing: {s}");
        assert!(s.contains("phase=parse"), "phase token missing: {s}");
        assert!(
            s.contains("processed=512/4218"),
            "processed/total token missing: {s}"
        );
        assert!(s.contains("rate="), "rate token missing: {s}");
    }

    #[test]
    fn format_omits_total_when_unknown() {
        let line = HeartbeatLine {
            elapsed_secs: 5,
            phase: "graph-leiden".into(),
            processed: 17,
            total: 0,
            rate: 3.4,
        };
        let s = line.format();
        assert!(s.contains("processed=17"), "processed missing: {s}");
        assert!(
            !s.contains("processed=17/0"),
            "must NOT show /0 for unknown total: {s}"
        );
    }

    /// Acceptance for B-017: when a long-running phase produces zero
    /// per-item events for a configured interval window, the heartbeat
    /// task MUST emit at least one line. We use a 50 ms interval and
    /// real-clock `tokio::time::sleep` instead of `start_paused` /
    /// `advance`, because under paused clock + multi-thread runtime
    /// the spawned interval task races the test's `yield_now` cadence
    /// and deflakes are fragile. 50 ms × ~120 ms wall = 1+ ticks
    /// observed reliably on every runner. Total wall-clock per test
    /// stays well under 200 ms.
    #[tokio::test(flavor = "current_thread")]
    async fn fires_at_least_one_tick_after_interval() {
        let captured: Arc<Mutex<Vec<HeartbeatLine>>> = Arc::new(Mutex::new(Vec::new()));
        let hb = Heartbeat::start_with(
            "parse",
            1000,
            false,
            Duration::from_millis(50),
            HeartbeatSink::Capture(Arc::clone(&captured)),
        );
        hb.record_processed(600);

        // Wait for ~3 intervals so we get at least 2 ticks even on a
        // slow runner. Real clock — deterministic enough at ms scale.
        tokio::time::sleep(Duration::from_millis(180)).await;

        drop(hb); // RAII stop — exercises the Drop path.

        let lines = captured.lock().unwrap().clone();
        assert!(
            !lines.is_empty(),
            "heartbeat must emit ≥1 line per interval; got 0 lines"
        );
        let first = &lines[0];
        assert_eq!(first.phase, "parse", "phase label should round-trip");
        assert_eq!(first.processed, 600, "processed counter should round-trip");
        assert_eq!(first.total, 1000, "total should round-trip");
        // The production code skips the immediate t=0 tick; the first
        // emitted line should be from at least one full interval after
        // start. Real-clock variance — accept >=0 secs (the elapsed
        // field is in seconds, and 50 ms intervals truncate to 0).
        // The format test pins the formatting; this test only asserts
        // the SIDE-CHANNEL contract: a line was emitted at all.
    }

    /// Quiet mode: the timer is armed (so the lifecycle is uniform
    /// across `--quiet` and noisy modes) but `emit` is suppressed.
    /// Real-clock based — same rationale as
    /// `fires_at_least_one_tick_after_interval`.
    #[tokio::test(flavor = "current_thread")]
    async fn quiet_mode_suppresses_emission() {
        let captured: Arc<Mutex<Vec<HeartbeatLine>>> = Arc::new(Mutex::new(Vec::new()));
        let hb = Heartbeat::start_with(
            "embed",
            64,
            true, // quiet
            Duration::from_millis(50),
            HeartbeatSink::Capture(Arc::clone(&captured)),
        );
        hb.record_processed(64);

        // Wait for ~4 intervals — under quiet=false this would emit at
        // least 3 lines, so any non-zero output is a quiet-flag bug.
        tokio::time::sleep(Duration::from_millis(220)).await;

        drop(hb);

        let lines = captured.lock().unwrap().clone();
        assert!(
            lines.is_empty(),
            "quiet=true must suppress all heartbeat emission; got {} lines",
            lines.len()
        );
    }

    /// `set_phase` resets the processed counter so the throughput
    /// metric reflects the new phase only. Without this, the embed
    /// pass would inherit the parse pass's accumulated count and the
    /// rate would be meaningless.
    #[test]
    fn set_phase_resets_processed_counter() {
        let inner = Arc::new(Inner {
            started: Instant::now(),
            phase: Mutex::new("parse".to_string()),
            processed: AtomicU64::new(500),
            total: AtomicU64::new(1000),
            quiet: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let hb = Heartbeat {
            inner: Arc::clone(&inner),
            task: None,
        };
        hb.set_phase("embed");
        assert_eq!(
            inner.processed.load(Ordering::Relaxed),
            0,
            "set_phase must reset processed so per-phase rate is meaningful"
        );
        // A1-026 (2026-05-04): set_phase also resets total so a stale
        // parser-pass total doesn't bleed into a phase with no
        // meaningful per-item count (graph-betweenness, leiden, etc.).
        // Callers that have a real total for the new phase must call
        // set_total AFTER set_phase.
        assert_eq!(
            inner.total.load(Ordering::Relaxed),
            0,
            "set_phase must reset total so the next phase doesn't show stale 0/N"
        );
        assert_eq!(
            *inner.phase.lock().unwrap(),
            "embed",
            "set_phase must replace the label"
        );
    }

    /// `add_total(0)` is a documented no-op. Defensive against caller
    /// arithmetic that subtracts to 0.
    #[test]
    fn add_total_zero_is_noop() {
        let inner = Arc::new(Inner {
            started: Instant::now(),
            phase: Mutex::new("parse".to_string()),
            processed: AtomicU64::new(0),
            total: AtomicU64::new(42),
            quiet: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        let hb = Heartbeat {
            inner: Arc::clone(&inner),
            task: None,
        };
        hb.add_total(0);
        assert_eq!(
            inner.total.load(Ordering::Relaxed),
            42,
            "add_total(0) must not bump total"
        );
        hb.add_total(8);
        assert_eq!(
            inner.total.load(Ordering::Relaxed),
            50,
            "add_total(8) must bump 42 → 50"
        );
    }

    /// Drop cancels the timer task. We can't directly observe the
    /// JoinHandle from outside, so we verify by confirming a heartbeat
    /// dropped well before its first tick produces zero output. Real-
    /// clock based — same rationale as the other heartbeat tests.
    #[tokio::test(flavor = "current_thread")]
    async fn drop_before_first_tick_emits_nothing() {
        let captured: Arc<Mutex<Vec<HeartbeatLine>>> = Arc::new(Mutex::new(Vec::new()));
        let hb = Heartbeat::start_with(
            "parse",
            1000,
            false,
            Duration::from_secs(60), // long interval — Drop happens before any tick
            HeartbeatSink::Capture(Arc::clone(&captured)),
        );
        hb.record_processed(50);

        // Wait << interval, then drop. The interval is 60s; we wait
        // 25 ms; the timer task can never reach its first emit window.
        tokio::time::sleep(Duration::from_millis(25)).await;
        drop(hb);

        // Brief grace so Drop's `task.abort()` propagates. Even if it
        // didn't, the interval is 60s — no real-time emission can sneak
        // in within the test's wall-clock budget.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let lines = captured.lock().unwrap().clone();
        assert!(
            lines.is_empty(),
            "Drop must cancel the timer task before its first emit; got {} lines",
            lines.len()
        );
    }
}
