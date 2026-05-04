//! In-memory broadcast bus.
//!
//! The bus is a *single* `async-broadcast` channel that fans every published
//! [`Event`] to every active subscriber. Topic filtering is applied per
//! subscriber on the consume side — this trades a tiny amount of CPU for a
//! drastically simpler concurrency model and keeps publish on the hot path
//! lock-free.
//!
//! ## Topic syntax
//!
//! Topics are dot-separated tokens, e.g. `project.abc123.file_changed`.
//! Patterns may use:
//!
//! - `*` — matches exactly one segment (`project.*.file_changed`)
//! - `#` — matches one or more trailing segments (`project.abc123.#`)
//!
//! Patterns are case-sensitive. Empty segments are rejected.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_broadcast::{InactiveReceiver, Receiver, Sender, TrySendError};
use tracing::{debug, warn};

use crate::error::LivebusError;
use crate::event::Event;

/// Configuration for [`EventBus`] construction.
#[derive(Debug, Clone)]
pub struct BusConfig {
    /// Capacity of the underlying broadcast channel. When full, the oldest
    /// undelivered event is dropped (`overflow=true`) and counted in
    /// `dropped_events`.
    pub channel_capacity: usize,
}

impl Default for BusConfig {
    fn default() -> Self {
        // 4096 events ≈ ~400ms of headroom at 10K events/sec.
        Self {
            channel_capacity: 4096,
        }
    }
}

/// Shared, cheaply-cloneable handle to the in-memory event bus.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

struct BusInner {
    sender: Sender<Event>,
    /// Inactive receiver kept alive for the lifetime of the bus so the
    /// channel never closes when there are zero subscribers, but does NOT
    /// count as an active receiver — events with no real subscriber are
    /// dropped immediately rather than queued.
    _keepalive: InactiveReceiver<Event>,
    started_at: Instant,
    published_count: AtomicU64,
    dropped_count: AtomicU64,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("capacity", &self.inner.sender.capacity())
            .field("receivers", &self.inner.sender.receiver_count())
            .field(
                "published",
                &self.inner.published_count.load(Ordering::Relaxed),
            )
            .field("dropped", &self.inner.dropped_count.load(Ordering::Relaxed))
            .finish()
    }
}

impl EventBus {
    /// Create a new bus with default capacity.
    pub fn new() -> Self {
        Self::with_config(BusConfig::default())
    }

    /// Create a new bus with explicit capacity.
    pub fn with_config(cfg: BusConfig) -> Self {
        let (mut sender, keepalive) = async_broadcast::broadcast(cfg.channel_capacity);
        // Overflow mode: drop oldest, never block the publisher.
        sender.set_overflow(true);
        // Convert keepalive to an inactive receiver — it keeps the channel
        // open without consuming events.
        let keepalive = keepalive.deactivate();
        let inner = BusInner {
            sender,
            _keepalive: keepalive,
            started_at: Instant::now(),
            published_count: AtomicU64::new(0),
            dropped_count: AtomicU64::new(0),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Subscribe and receive *every* published event from this point forward.
    /// Topic filtering is the caller's responsibility — see
    /// [`crate::topic_matches`].
    pub fn subscribe_raw(&self) -> Receiver<Event> {
        self.inner.sender.new_receiver()
    }

    /// Publish an event to all active receivers. If the channel is full the
    /// oldest pending event is dropped (counted in `dropped_events`).
    ///
    /// **Loss semantics:** this method is **lossy**. It returns `Ok(())`
    /// whenever the event was accepted by the broadcast machinery -- even if
    /// an older queued event was *evicted* to make room (overflow mode), and
    /// even when the channel was inactive (no active receivers). Producers
    /// that need to detect loss must call [`Self::publish_with_outcome`]
    /// instead. See BUG-A4-008 for context.
    pub fn publish(&self, event: Event) -> Result<(), LivebusError> {
        // Delegate to the outcome-returning variant so the counter
        // bookkeeping lives in exactly one place.
        match self.publish_with_outcome(event) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Publish an event and return a tri-state describing what actually
    /// happened to it. **BUG-A4-008 fix:** lets producers that need
    /// exactly-once-ish semantics (drift detector emitting `DriftFinding`,
    /// audit pipeline emitting completion events) detect overflow eviction
    /// or queue-full drops rather than silently trusting `Ok(())`.
    pub fn publish_with_outcome(&self, event: Event) -> Result<PublishOutcome, LivebusError> {
        match self.inner.sender.try_broadcast(event) {
            Ok(None) => {
                self.inner.published_count.fetch_add(1, Ordering::Relaxed);
                Ok(PublishOutcome::Delivered)
            }
            Ok(Some(evicted)) => {
                self.inner.published_count.fetch_add(1, Ordering::Relaxed);
                self.inner.dropped_count.fetch_add(1, Ordering::Relaxed);
                debug!("livebus: oldest event evicted by overflow");
                Ok(PublishOutcome::Evicted(evicted))
            }
            Err(TrySendError::Full(_)) => {
                // Should not happen because overflow is enabled, but guard
                // anyway.
                self.inner.dropped_count.fetch_add(1, Ordering::Relaxed);
                warn!("livebus: broadcast queue full; event dropped");
                Ok(PublishOutcome::Dropped)
            }
            Err(TrySendError::Closed(_)) => Err(LivebusError::BusClosed),
            Err(TrySendError::Inactive(_)) => {
                // No active receivers right now: the event is "published"
                // into the void. That's fine for fire-and-forget telemetry,
                // but producers that need delivery confirmation should treat
                // this as a soft drop -- it never reached anyone.
                self.inner.published_count.fetch_add(1, Ordering::Relaxed);
                Ok(PublishOutcome::Inactive)
            }
        }
    }

    /// Number of currently active receivers.
    pub fn receiver_count(&self) -> usize {
        self.inner.sender.receiver_count()
    }

    /// Total events accepted by `publish`.
    pub fn published_count(&self) -> u64 {
        self.inner.published_count.load(Ordering::Relaxed)
    }

    /// Total events that were evicted by overflow.
    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped_count.load(Ordering::Relaxed)
    }

    /// Bus uptime in whole seconds.
    pub fn uptime_seconds(&self) -> u64 {
        self.inner.started_at.elapsed().as_secs()
    }

    /// Increment the dropped counter externally — used by the subscriber
    /// manager when it evicts a slow subscriber.
    pub fn record_drop(&self) {
        self.inner.dropped_count.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate a topic string. Returns `Err` on empty topics or empty segments.
///
/// Wildcards (`*`, `#`) are allowed in *patterns* but `validate_topic` accepts
/// both — concrete topics generally won't contain them but we don't reject
/// them here. Use [`is_concrete_topic`] when you need a no-wildcard topic.
pub fn validate_topic(topic: &str) -> Result<(), LivebusError> {
    if topic.is_empty() {
        return Err(LivebusError::InvalidTopic("topic is empty".into()));
    }
    for seg in topic.split('.') {
        if seg.is_empty() {
            return Err(LivebusError::InvalidTopic(format!(
                "topic '{topic}' contains empty segment"
            )));
        }
        if seg.chars().any(|c| c.is_control() || c == ' ' || c == '\n') {
            return Err(LivebusError::InvalidTopic(format!(
                "topic '{topic}' contains control or whitespace character"
            )));
        }
    }
    Ok(())
}

/// Returns true iff `topic` contains no wildcard segments.
pub fn is_concrete_topic(topic: &str) -> bool {
    !topic.split('.').any(|s| s == "*" || s == "#")
}

/// Match a concrete topic against a (possibly wildcard) pattern.
///
/// Wildcard semantics:
/// - `*` matches exactly one segment.
/// - `#` matches one or more trailing segments. Must be the LAST segment of
///   the pattern; otherwise it is treated as a literal `#` (and will never
///   match a real topic).
///
/// Examples:
/// ```
/// use mneme_livebus::topic_matches;
/// assert!(topic_matches("project.*.file_changed", "project.abc.file_changed"));
/// assert!(topic_matches("project.abc.#",         "project.abc.file_changed"));
/// assert!(topic_matches("project.abc.#",         "project.abc.test_status"));
/// assert!(!topic_matches("project.*.file_changed", "session.x.file_changed"));
/// assert!(topic_matches("system.health", "system.health"));
/// ```
pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    let pat_segs: Vec<&str> = pattern.split('.').collect();
    let top_segs: Vec<&str> = topic.split('.').collect();

    // Trailing # — multi-segment wildcard.
    if let Some(last) = pat_segs.last() {
        if *last == "#" && pat_segs.len() >= 2 {
            let prefix = &pat_segs[..pat_segs.len() - 1];
            if top_segs.len() < prefix.len() + 1 {
                // # must consume at least one segment
                return false;
            }
            for (p, t) in prefix.iter().zip(top_segs.iter()) {
                if *p != "*" && p != t {
                    return false;
                }
            }
            return true;
        }
        if *last == "#" && pat_segs.len() == 1 {
            // bare "#" matches any non-empty topic
            return !top_segs.is_empty() && !topic.is_empty();
        }
    }

    if pat_segs.len() != top_segs.len() {
        return false;
    }
    for (p, t) in pat_segs.iter().zip(top_segs.iter()) {
        if *p == "*" {
            continue;
        }
        if p != t {
            return false;
        }
    }
    true
}

/// Check whether a topic matches *any* of the supplied patterns.
pub fn topic_matches_any(patterns: &[String], topic: &str) -> bool {
    patterns.iter().any(|p| topic_matches(p, topic))
}

/// Outcome of a [`EventBus::publish_with_outcome`] call.
///
/// BUG-A4-008 fix (2026-05-04): the legacy `publish` returns `Ok(())` on
/// three different non-success states (overflow eviction, queue full, no
/// active receivers), making it impossible for producers that care about
/// delivery (drift detector, audit completion) to detect loss. This
/// enum gives them an explicit signal.
#[derive(Debug)]
pub enum PublishOutcome {
    /// The event was accepted by the broadcast and no prior event was
    /// evicted. This is the normal happy path.
    Delivered,
    /// The event was accepted but the channel was already full, so the
    /// oldest pending event was evicted to make room. Counted in
    /// `dropped_count` as well.
    Evicted(Event),
    /// The event was rejected outright (channel full, overflow disabled).
    /// Should not happen in default config (overflow=true) but kept as
    /// an explicit case so the caller does not have to assume.
    Dropped,
    /// The event reached the broadcast machinery but there were no
    /// active receivers, so the event was discarded immediately rather
    /// than queued. The published_count still ticks (the producer's
    /// side-effect is recorded) but no consumer ever saw the payload.
    Inactive,
}

#[cfg(test)]
mod bus_tests {
    use super::*;

    #[test]
    fn validate_basic() {
        assert!(validate_topic("project.abc.file_changed").is_ok());
        assert!(validate_topic("").is_err());
        assert!(validate_topic("project..file").is_err());
        assert!(validate_topic("project. .x").is_err());
    }

    #[test]
    fn wildcard_single_segment() {
        assert!(topic_matches(
            "project.*.file_changed",
            "project.abc.file_changed"
        ));
        assert!(!topic_matches(
            "project.*.file_changed",
            "project.abc.def.file_changed"
        ));
        assert!(!topic_matches(
            "project.*.file_changed",
            "session.abc.file_changed"
        ));
    }

    #[test]
    fn wildcard_hash_trailing() {
        assert!(topic_matches("project.abc.#", "project.abc.file_changed"));
        assert!(topic_matches("project.abc.#", "project.abc.x.y.z"));
        assert!(!topic_matches("project.abc.#", "project.def.file_changed"));
        assert!(!topic_matches("project.abc.#", "project.abc"));
    }

    #[test]
    fn exact_match() {
        assert!(topic_matches("system.health", "system.health"));
        assert!(!topic_matches("system.health", "system.health.x"));
    }

    #[test]
    fn is_concrete_helper() {
        assert!(is_concrete_topic("project.abc.file_changed"));
        assert!(!is_concrete_topic("project.*.file_changed"));
        assert!(!is_concrete_topic("project.abc.#"));
    }
}
