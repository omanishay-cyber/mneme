//! Subscriber registry and slow-consumer eviction.
//!
//! A [`Subscriber`] is the in-process representation of an SSE or WebSocket
//! client. It owns:
//!
//! 1. A bounded MPSC sender to the transport task that writes to the wire.
//! 2. A list of topic patterns it registered for.
//! 3. A monotonic counter of how many consecutive events it has failed to
//!    accept (the "lag window"). When the lag exceeds [`BACKPRESSURE_WINDOW`]
//!    the [`SubscriberManager`] evicts it and emits a `system.degraded_mode`
//!    event so other subscribers can see the drop.
//!
//! The registry is kept in a single `RwLock<HashMap>` keyed by subscriber id.
//! This is contended only on subscribe/unsubscribe; the per-event fast path
//! does NOT touch the registry — each subscriber is driven by its own
//! independent broadcast `Receiver`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::bus::{topic_matches_any, validate_topic, EventBus};
use crate::error::LivebusError;
use crate::event::{DegradedMode, Event, EventPayload};

/// Per-subscriber lag budget. If the subscriber falls this many events behind
/// it is evicted and a `system.degraded_mode` event is published.
///
/// AI-DNA pace: bumped from 50 to 256 (5× headroom). When AI edits 10 files
/// in 30s the bus emits ~50-100 events in a sub-second burst; the legacy
/// 50-cap evicted normal subscribers (vision app, MCP) the moment they were
/// behind by half a second. The bumped cap absorbs full burst windows
/// before declaring a subscriber slow. The `mpsc::channel(cap)` sized at
/// `register_with_capacity` time is bumped in lockstep — see line ~142.
///
/// See `feedback_mneme_ai_dna_pace.md` Principle B: "every queue depth
/// tuned for AI-rate, not human-rate".
pub const BACKPRESSURE_WINDOW: usize = 256;

/// Snapshot of subscriber-related stats for `/health`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubscriberStats {
    pub active_subscribers: usize,
    pub evicted_subscribers: u64,
}

/// In-process handle a transport task uses to push events to a single client.
#[derive(Debug)]
pub struct SubscriberHandle {
    pub id: String,
    pub rx: mpsc::Receiver<Event>,
}

/// Registry entry tracking a single live subscriber.
#[derive(Debug)]
pub struct Subscriber {
    pub id: String,
    pub patterns: Vec<String>,
    pub tx: mpsc::Sender<Event>,
    /// Monotonic count of consecutive failed `try_send` calls — reset on a
    /// successful send.
    pub lag: AtomicU64,
    /// Total events delivered to this subscriber.
    pub delivered: AtomicU64,
    /// Per-subscriber lag/eviction cap. Defaults to [`BACKPRESSURE_WINDOW`];
    /// override via [`Subscriber::with_capacity`] or
    /// [`SubscriberManager::register_with_capacity`].
    pub cap: usize,
}

impl Subscriber {
    fn new(id: String, patterns: Vec<String>, tx: mpsc::Sender<Event>) -> Self {
        Self {
            id,
            patterns,
            tx,
            lag: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            cap: BACKPRESSURE_WINDOW,
        }
    }

    /// Builder-style override of the per-subscriber lag/eviction cap.
    /// Note: the underlying mpsc channel is still sized at construction
    /// time; use [`SubscriberManager::register_with_capacity`] to size both.
    pub fn with_capacity(mut self, cap: usize) -> Self {
        self.cap = cap;
        self
    }

    /// True if this subscriber wants to see `topic`.
    pub fn matches(&self, topic: &str) -> bool {
        topic_matches_any(&self.patterns, topic)
    }
}

/// Registry of all live subscribers.
#[derive(Debug, Clone)]
pub struct SubscriberManager {
    inner: Arc<ManagerInner>,
}

#[derive(Debug)]
struct ManagerInner {
    subscribers: RwLock<HashMap<String, Arc<Subscriber>>>,
    bus: EventBus,
    next_id: AtomicU64,
    evicted: AtomicU64,
}

impl SubscriberManager {
    pub fn new(bus: EventBus) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                subscribers: RwLock::new(HashMap::new()),
                bus,
                next_id: AtomicU64::new(1),
                evicted: AtomicU64::new(0),
            }),
        }
    }

    /// Register a new subscriber with the given topic patterns. Returns a
    /// [`SubscriberHandle`] whose `rx` the transport task should drain to the
    /// wire. Uses the default [`BACKPRESSURE_WINDOW`] for both the mpsc
    /// channel and the eviction cap.
    pub fn register(
        &self,
        patterns: Vec<String>,
    ) -> Result<SubscriberHandle, LivebusError> {
        self.register_with_capacity(patterns, BACKPRESSURE_WINDOW)
    }

    /// Like [`Self::register`] but with a custom per-subscriber cap. The cap
    /// sizes both the mpsc channel and the lag-based eviction threshold so
    /// the two stay in lockstep (eviction fires when the channel is full
    /// `cap` times in a row).
    pub fn register_with_capacity(
        &self,
        patterns: Vec<String>,
        cap: usize,
    ) -> Result<SubscriberHandle, LivebusError> {
        for p in &patterns {
            validate_topic(p)?;
        }
        let cap = cap.max(1);
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("sub-{n}");
        let (tx, rx) = mpsc::channel::<Event>(cap);
        let sub =
            Arc::new(Subscriber::new(id.clone(), patterns, tx).with_capacity(cap));
        self.write_registry().insert(id.clone(), sub);
        info!(subscriber = %id, cap, "subscriber registered");
        Ok(SubscriberHandle { id, rx })
    }

    /// Replace the topic patterns of an existing subscriber. Used by the
    /// WebSocket `subscribe` / `unsubscribe` control messages.
    pub fn update_patterns(
        &self,
        id: &str,
        patterns: Vec<String>,
    ) -> Result<(), LivebusError> {
        for p in &patterns {
            validate_topic(p)?;
        }
        let mut guard = self.write_registry();
        let Some(existing) = guard.get(id).cloned() else {
            return Err(LivebusError::SubscriberEvicted(
                id.into(),
                "subscriber not found".into(),
            ));
        };
        let replaced = Arc::new(Subscriber {
            id: existing.id.clone(),
            patterns,
            tx: existing.tx.clone(),
            lag: AtomicU64::new(existing.lag.load(Ordering::Relaxed)),
            delivered: AtomicU64::new(existing.delivered.load(Ordering::Relaxed)),
            cap: existing.cap,
        });
        guard.insert(id.to_string(), replaced);
        Ok(())
    }

    /// Remove a subscriber by id (no-op if already gone).
    pub fn unregister(&self, id: &str) {
        if self.write_registry().remove(id).is_some() {
            info!(subscriber = %id, "subscriber unregistered");
        }
    }

    fn read_registry(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<Subscriber>>> {
        self.inner
            .subscribers
            .read()
            .expect("livebus subscriber registry poisoned")
    }

    fn write_registry(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<Subscriber>>> {
        self.inner
            .subscribers
            .write()
            .expect("livebus subscriber registry poisoned")
    }

    /// Fan-out an event to every matching subscriber, evicting any that
    /// exceed the backpressure window.
    pub fn dispatch(&self, event: &Event) {
        // Snapshot the subscribers (cheap Arc clones) so we don't hold the
        // lock across `.try_send`.
        let snapshot: Vec<Arc<Subscriber>> =
            self.read_registry().values().cloned().collect();

        let mut to_evict: Vec<(String, String)> = Vec::new();
        for sub in snapshot {
            if !sub.matches(&event.topic) {
                continue;
            }
            match sub.tx.try_send(event.clone()) {
                Ok(()) => {
                    sub.lag.store(0, Ordering::Relaxed);
                    sub.delivered.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let lag = sub.lag.fetch_add(1, Ordering::Relaxed) + 1;
                    if lag as usize >= sub.cap {
                        to_evict.push((
                            sub.id.clone(),
                            format!(
                                "lag {lag} >= backpressure window {cap}",
                                cap = sub.cap
                            ),
                        ));
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    to_evict.push((sub.id.clone(), "channel closed".into()));
                }
            }
        }

        for (id, reason) in to_evict {
            self.evict(&id, &reason);
        }
    }

    /// Forcibly remove a subscriber and emit a `system.degraded_mode` warning.
    pub fn evict(&self, id: &str, reason: &str) {
        let removed = self.write_registry().remove(id).is_some();
        if !removed {
            return;
        }
        self.inner.evicted.fetch_add(1, Ordering::Relaxed);
        self.inner.bus.record_drop();
        warn!(subscriber = %id, %reason, "subscriber evicted (slow consumer)");

        // Post a degraded-mode notice so the rest of the world knows.
        let payload = EventPayload::DegradedMode(DegradedMode {
            reason: reason.into(),
            subscriber_id: Some(id.into()),
            dropped_count: Some(self.inner.evicted.load(Ordering::Relaxed)),
        });
        let ev = Event::from_typed("system.degraded_mode", None, None, payload);
        // Publish to the broadcast bus (for raw subscribers + HTTP SSE).
        let _ = self.inner.bus.publish(ev.clone());
        // Also fan out through the subscriber manager so topic-filtered
        // subscribers registered via `register()` receive it on their
        // bounded channel — `bus.publish` alone only reaches raw receivers.
        // Avoid recursion: evictions from this dispatch are silently ignored
        // because the registry has already been mutated for the current id.
        self.dispatch_internal(&ev, /* recursion_guard */ true);
    }

    /// Internal dispatch that mirrors `dispatch()` but with optional recursion
    /// guard. When `guard` is true, subscribers that fail to send are NOT
    /// evicted (they'll be cleaned up on the next real dispatch pass).
    fn dispatch_internal(&self, event: &Event, guard: bool) {
        let snapshot: Vec<Arc<Subscriber>> =
            self.read_registry().values().cloned().collect();
        let mut to_evict: Vec<(String, String)> = Vec::new();
        for sub in snapshot {
            if !sub.matches(&event.topic) {
                continue;
            }
            match sub.tx.try_send(event.clone()) {
                Ok(()) => {
                    sub.lag.store(0, Ordering::Relaxed);
                    sub.delivered.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let lag = sub.lag.fetch_add(1, Ordering::Relaxed) + 1;
                    if !guard && lag as usize >= sub.cap {
                        to_evict.push((
                            sub.id.clone(),
                            format!(
                                "lag {lag} >= backpressure window {cap}",
                                cap = sub.cap
                            ),
                        ));
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if !guard {
                        to_evict.push((sub.id.clone(), "channel closed".into()));
                    }
                }
            }
        }
        for (id, reason) in to_evict {
            self.evict(&id, &reason);
        }
    }

    /// Current registry size and lifetime eviction count.
    pub fn stats(&self) -> SubscriberStats {
        SubscriberStats {
            active_subscribers: self.read_registry().len(),
            evicted_subscribers: self.inner.evicted.load(Ordering::Relaxed),
        }
    }

    /// Borrow the underlying bus.
    pub fn bus(&self) -> EventBus {
        self.inner.bus.clone()
    }
}

#[cfg(test)]
mod sub_tests {
    use super::*;

    #[tokio::test]
    async fn register_and_dispatch_matches() {
        let bus = EventBus::new();
        let mgr = SubscriberManager::new(bus.clone());
        let mut h = mgr
            .register(vec!["project.*.file_changed".into()])
            .unwrap();
        let ev = Event::from_json(
            "project.abc.file_changed",
            None,
            Some("abc".into()),
            serde_json::json!({"x": 1}),
        );
        mgr.dispatch(&ev);
        let got = h.rx.recv().await.unwrap();
        assert_eq!(got.topic, "project.abc.file_changed");
    }

    #[tokio::test]
    async fn non_matching_does_not_deliver() {
        let bus = EventBus::new();
        let mgr = SubscriberManager::new(bus.clone());
        let mut h = mgr.register(vec!["system.health".into()]).unwrap();
        mgr.dispatch(&Event::from_json(
            "project.abc.file_changed",
            None,
            None,
            serde_json::Value::Null,
        ));
        assert!(h.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn slow_subscriber_is_evicted() {
        let bus = EventBus::new();
        let mgr = SubscriberManager::new(bus.clone());
        // Register but never drain the receiver.
        let _h = mgr.register(vec!["system.health".into()]).unwrap();
        for i in 0..(BACKPRESSURE_WINDOW * 4) {
            mgr.dispatch(&Event::from_json(
                "system.health",
                None,
                None,
                serde_json::json!({"i": i}),
            ));
        }
        assert_eq!(mgr.stats().active_subscribers, 0);
        assert!(mgr.stats().evicted_subscribers >= 1);
    }

    #[tokio::test]
    async fn subscriber_eviction_uses_configured_cap() {
        // With cap=10, the bounded mpsc accepts the first 10 events; the
        // next 10 trip `try_send -> Full` and bump `lag`. By iteration 20
        // the lag has reached the configured cap of 10 and eviction fires.
        // Under the legacy hardcoded BACKPRESSURE_WINDOW=50 this would not
        // happen until iteration 100, so 25 events is a clean discriminator.
        let bus = EventBus::new();
        let mgr = SubscriberManager::new(bus.clone());
        let _h = mgr
            .register_with_capacity(vec!["system.health".into()], 10)
            .unwrap();
        for i in 0..25 {
            mgr.dispatch(&Event::from_json(
                "system.health",
                None,
                None,
                serde_json::json!({"i": i}),
            ));
        }
        assert_eq!(
            mgr.stats().active_subscribers,
            0,
            "subscriber with cap=10 should be evicted well before 25 events \
             (would need 100 under the hardcoded 50-cap)"
        );
        assert!(
            mgr.stats().evicted_subscribers >= 1,
            "evicted count should reflect the cap=10 eviction"
        );
    }

    #[tokio::test]
    async fn subscriber_no_eviction_under_cap() {
        // With cap=1000 and only 50 un-drained events, eviction must NOT fire
        // — proving the cap is honored over the legacy 50-constant.
        let bus = EventBus::new();
        let mgr = SubscriberManager::new(bus.clone());
        let _h = mgr
            .register_with_capacity(vec!["system.health".into()], 1000)
            .unwrap();
        for i in 0..50 {
            mgr.dispatch(&Event::from_json(
                "system.health",
                None,
                None,
                serde_json::json!({"i": i}),
            ));
        }
        assert_eq!(
            mgr.stats().active_subscribers,
            1,
            "subscriber with cap=1000 must survive 50 un-drained events"
        );
        assert_eq!(
            mgr.stats().evicted_subscribers,
            0,
            "no eviction should fire below the configured cap"
        );
    }
}
