//! # mneme-livebus
//!
//! Local-only event fan-out service for the Mneme multi-process daemon.
//!
//! Worker processes (drift detector, file watcher, test runner, vision, brain,
//! supervisor) push events into the bus over a Unix socket / Windows named pipe.
//! Subscribers (Claude Code via MCP, the vision app, multi-agent sessions)
//! connect over Server-Sent Events (`GET /events/:topic`) or WebSocket
//! (`/ws`) to receive a fan-out stream of events filtered by topic pattern.
//!
//! ## Design constraints (see `docs/design/2026-04-23-mneme-design.md` §11)
//!
//! - **Local only.** The HTTP listener binds to `127.0.0.1` exclusively;
//!   binding to `0.0.0.0` is forbidden by construction (see [`bind_addr`]).
//! - **Backpressure.** Each subscriber owns a bounded queue (default 50). When
//!   a subscriber lags past the window we drop the subscriber and emit a
//!   `system.degraded_mode` warning event.
//! - **Topic patterns.** Topics use dot-segments
//!   (`project.<hash>.file_changed`) and may be matched with single-segment
//!   wildcards (`project.*.file_changed`) or trailing multi-segment wildcards
//!   (`project.<hash>.#`).
//! - **Latency budget.** <50ms emit→deliver under nominal load.
//! - **Throughput target.** 10K events/sec sustained.
//!
//! ## Module layout
//!
//! - [`bus`]    – in-memory broadcast channel and topic registry
//! - [`event`]  – `Event` envelope and typed payload variants
//! - [`sse`]    – HTTP Server-Sent Events endpoint
//! - [`ws`]     – WebSocket endpoint with `subscribe`/`unsubscribe` control
//! - [`subscriber`] – subscriber manager + slow-consumer eviction policy
//! - [`ipc_input`]  – Unix socket / named pipe ingest from sibling workers
//! - [`health`] – `GET /health` JSON status endpoint
//! - [`error`]  – crate error type

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]

pub mod bus;
pub mod error;
pub mod event;
pub mod health;
pub mod ipc_input;
pub mod sse;
pub mod subscriber;
pub mod ws;

#[cfg(test)]
mod tests;

pub use bus::{
    topic_matches, topic_matches_any, validate_topic, BusConfig, EventBus, PublishOutcome,
};
pub use error::LivebusError;
pub use event::{
    CompactionDetected, DegradedMode, DriftFinding, Event, EventPayload, FileChanged, HealthUpdate,
    StepAdvanced, SubagentEvent, TestStatus,
};
pub use health::{HealthCtx, HealthSnapshot, HealthState, RateSampler};
pub use ipc_input::{default_ipc_path, run_ipc_listener};
pub use subscriber::{
    Subscriber, SubscriberHandle, SubscriberManager, SubscriberStats, BACKPRESSURE_WINDOW,
};

/// Default loopback address. **Never** rebind to a non-loopback interface.
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// Default TCP port for the SSE/WebSocket/health surface.
pub const DEFAULT_PORT: u16 = 7778;

/// Build a `SocketAddr`-friendly string and refuse anything non-loopback.
///
/// Returns `Err(LivebusError::Bind)` if `host` does not resolve to a
/// loopback (`127.0.0.0/8`, `::1`) address.
pub fn bind_addr(host: &str, port: u16) -> Result<std::net::SocketAddr, LivebusError> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    let ip: IpAddr = host
        .parse()
        .map_err(|_| LivebusError::Bind(format!("invalid host: {host}")))?;

    let is_loopback = match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4 == Ipv4Addr::LOCALHOST,
        IpAddr::V6(v6) => v6.is_loopback() || v6 == Ipv6Addr::LOCALHOST,
    };

    if !is_loopback {
        return Err(LivebusError::Bind(format!(
            "refusing to bind livebus to non-loopback address {host}; \
             livebus is local-only by policy"
        )));
    }

    Ok(SocketAddr::new(ip, port))
}
