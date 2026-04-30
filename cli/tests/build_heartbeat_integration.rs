//! Integration test for the B-017 heartbeat (best-effort).
//!
//! The full `mneme build` pipeline is too heavy to spin up from a
//! single integration test (it pulls in tree-sitter, brain, scanners,
//! and the multimodal sidecar). Instead we exercise the public
//! [`Heartbeat`] API the same way the CLI does — start the handle,
//! drive the per-file progress feed, drop the handle, and assert the
//! emitted lines match the prompt's contract:
//!
//! * "≥1 status line per 30 seconds during parse/embed/graph"
//! * "format = `[mm:ss elapsed] phase=<name> processed=<n>/<total> rate=<eps>/s`"
//!
//! We use a 50 ms interval for fast wall-clock turnaround. The
//! production interval is 30 s; the formatting and lifecycle contract
//! is interval-agnostic.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mneme_cli::build_heartbeat::{Heartbeat, HeartbeatLine, HeartbeatSink};

/// Drive a 5-step "build" through the heartbeat the same way `run_inline`
/// does (add_total per file walked, record_processed per file indexed)
/// and assert at least one status line lands per interval.
#[tokio::test(flavor = "current_thread")]
async fn heartbeat_emits_status_line_during_parse_phase() {
    let captured: Arc<Mutex<Vec<HeartbeatLine>>> = Arc::new(Mutex::new(Vec::new()));
    let hb = Heartbeat::start_with(
        "parse",
        0,
        false,
        Duration::from_millis(40),
        HeartbeatSink::Capture(Arc::clone(&captured)),
    );

    // Simulate a 5-file walk + indexing loop. In production this loop
    // would take seconds to minutes; here we sleep just enough to
    // cross the heartbeat's 40 ms interval.
    for i in 1..=5u64 {
        hb.add_total(1);
        // Pretend the parse takes a moment.
        tokio::time::sleep(Duration::from_millis(15)).await;
        hb.record_processed(i);
    }

    drop(hb);

    let lines = captured.lock().unwrap().clone();
    assert!(
        !lines.is_empty(),
        "heartbeat must emit ≥1 status line per interval during a 75-ms simulated parse pass; got 0"
    );

    // At least one line should label the parse phase.
    let parse_lines: Vec<_> = lines.iter().filter(|l| l.phase == "parse").collect();
    assert!(
        !parse_lines.is_empty(),
        "expected at least one phase=parse line; got phases: {:?}",
        lines.iter().map(|l| l.phase.clone()).collect::<Vec<_>>()
    );

    // Format contract: every emitted line MUST carry the `phase=` /
    // `processed=` / `rate=` tokens so log scrapers parse them.
    for line in &lines {
        let s = line.format();
        assert!(s.contains("phase="), "line missing phase= token: {s}");
        assert!(s.contains("processed="), "line missing processed= token: {s}");
        assert!(s.contains("rate="), "line missing rate= token: {s}");
        assert!(s.contains("elapsed]"), "line missing elapsed mm:ss: {s}");
    }
}

/// Phase transition: when the build moves from parse → embed, the
/// heartbeat MUST relabel subsequent emissions. Without this the
/// status line would lie ("phase=parse" while we're actually
/// embedding).
#[tokio::test(flavor = "current_thread")]
async fn heartbeat_relabels_on_phase_transition() {
    let captured: Arc<Mutex<Vec<HeartbeatLine>>> = Arc::new(Mutex::new(Vec::new()));
    let hb = Heartbeat::start_with(
        "parse",
        100,
        false,
        Duration::from_millis(30),
        HeartbeatSink::Capture(Arc::clone(&captured)),
    );

    hb.record_processed(50);
    tokio::time::sleep(Duration::from_millis(70)).await;

    hb.set_phase("embed");
    hb.set_total(64);
    hb.record_processed(32);
    tokio::time::sleep(Duration::from_millis(70)).await;

    drop(hb);

    let lines = captured.lock().unwrap().clone();
    let parse_lines: Vec<_> = lines.iter().filter(|l| l.phase == "parse").collect();
    let embed_lines: Vec<_> = lines.iter().filter(|l| l.phase == "embed").collect();

    assert!(
        !parse_lines.is_empty(),
        "expected ≥1 phase=parse line before transition; got phases: {:?}",
        lines.iter().map(|l| l.phase.clone()).collect::<Vec<_>>()
    );
    assert!(
        !embed_lines.is_empty(),
        "expected ≥1 phase=embed line after transition; got phases: {:?}",
        lines.iter().map(|l| l.phase.clone()).collect::<Vec<_>>()
    );
}
