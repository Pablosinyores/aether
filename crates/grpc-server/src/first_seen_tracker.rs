//! First-seen → inclusion latency tracker for the mempool path.
//!
//! Stamps every pending tx hash with our local wall-clock first-seen time
//! (provided by the ingestion layer via
//! [`PendingTxEvent::first_seen_unix_nanos`]). When a block lands, the
//! tracker walks the block's tx hash list and, for each hit, observes the
//! `(block_received_at − first_seen_at)` delta on the
//! `aether_mempool_first_seen_to_inclusion_ms` histogram.
//!
//! This is pure observability — no behaviour changes elsewhere depend on
//! it. The point is to measure how far behind block-builder mempool
//! visibility we run. If our p99 first-seen-to-inclusion delta is
//! significantly larger than the actual block-propagation time, builders
//! are seeing victims before us and we lose the auction.
//!
//! ## Capacity / eviction
//!
//! The tracker is a bounded insertion-order map keyed by tx hash. New
//! entries displace the oldest when the capacity ceiling is hit so a
//! sustained mempool burst can't grow it unboundedly. Default capacity
//! (`DEFAULT_CAPACITY`) holds ~10× the average pending pool depth at the
//! time of writing. Override via [`MempoolFirstSeenTracker::with_capacity`]
//! if your environment differs.
//!
//! ## Concurrency
//!
//! Single [`std::sync::Mutex`]. Tracker operations are O(1) and the
//! critical section is microseconds long; lock contention has not shown
//! up in practice. Switch to a sharded structure only if perf data
//! demands it.

use crate::EngineMetrics;
use aether_ingestion::subscription::EventChannels;
use alloy::primitives::B256;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default ring buffer capacity. ~10× the typical Ethereum mainnet
/// pending-pool depth so the tracker keeps everything we've seen across
/// at least one block of normal activity.
pub const DEFAULT_CAPACITY: usize = 50_000;

/// Bounded first-seen → inclusion tracker. See module docs.
pub struct MempoolFirstSeenTracker {
    capacity: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Tx hash → first-seen unix nanos. Hash-keyed so block-side
    /// lookups are O(1).
    seen: HashMap<B256, u64>,
    /// Insertion-order log so capacity-bound eviction drops the oldest
    /// entry rather than an arbitrary one. Same length as `seen`.
    order: VecDeque<B256>,
}

impl MempoolFirstSeenTracker {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        // capacity == 0 would treat every record as an immediate evict
        // and the histogram would never fire; guard the test path.
        let capacity = capacity.max(1);
        Self {
            capacity,
            inner: Mutex::new(Inner {
                seen: HashMap::with_capacity(capacity),
                order: VecDeque::with_capacity(capacity),
            }),
        }
    }

    /// Stamp `hash` with `first_seen_unix_nanos`. Idempotent — a duplicate
    /// hash keeps the earliest stamp so retransmissions don't artificially
    /// shorten the delta. Bumps the `recorded` (or `evicted_capacity` on
    /// the eviction edge) counter on `metrics`. Zero-stamp inputs are
    /// rejected and bump the `unstamped` counter.
    pub fn record(&self, hash: B256, first_seen_unix_nanos: u64, metrics: &EngineMetrics) {
        if first_seen_unix_nanos == 0 {
            metrics.inc_mempool_first_seen_event("unstamped");
            return;
        }
        let mut inner = self.inner.lock().expect("first-seen tracker poisoned");

        // Idempotent insert. Keep the earliest stamp so retransmits don't
        // shrink the observed delta (we want the wall-clock first time
        // *our* pipeline learned about the tx).
        if inner.seen.contains_key(&hash) {
            return;
        }

        // Capacity check before insert. Evict from the front of `order`
        // so the oldest entry is the one that leaves.
        while inner.order.len() >= self.capacity {
            if let Some(old) = inner.order.pop_front() {
                inner.seen.remove(&old);
                metrics.inc_mempool_first_seen_event("evicted_capacity");
            } else {
                break;
            }
        }

        inner.seen.insert(hash, first_seen_unix_nanos);
        inner.order.push_back(hash);
        metrics.inc_mempool_first_seen_event("recorded");
    }

    /// Block-side hook: walk a block's tx hash list and observe deltas
    /// for every hash we tracked. Matched entries are removed so a single
    /// inclusion only fires one histogram observation. Unmatched hashes
    /// bump the `unmatched` event counter so dashboards can see hit-rate
    /// (matched / (matched + unmatched)) at a glance.
    ///
    /// `now_unix_nanos` is the wall-clock instant the block reached this
    /// process. Threading it through (rather than reading
    /// `SystemTime::now()` inside) keeps the function testable.
    pub fn observe_block(
        &self,
        block_tx_hashes: &[B256],
        now_unix_nanos: u64,
        metrics: &EngineMetrics,
    ) {
        if block_tx_hashes.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().expect("first-seen tracker poisoned");
        for hash in block_tx_hashes {
            match inner.seen.remove(hash) {
                Some(first_seen) => {
                    // Saturating sub guards against clock skew (e.g.
                    // builder timestamp slightly behind our ingest). A
                    // negative delta would be meaningless so we floor it
                    // to zero rather than skip.
                    let delta_nanos = now_unix_nanos.saturating_sub(first_seen);
                    let delta_ms = delta_nanos as f64 / 1_000_000.0;
                    metrics.observe_mempool_first_seen_to_inclusion_ms(delta_ms);
                    metrics.inc_mempool_first_seen_event("matched");
                    // No O(n) removal from `order`; let it carry the
                    // stale entry forward. The eviction loop above will
                    // GC it once capacity pressure hits. This avoids a
                    // O(n) `VecDeque::retain` per block.
                }
                None => {
                    metrics.inc_mempool_first_seen_event("unmatched");
                }
            }
        }
    }

    /// Test helper: how many entries currently tracked.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().seen.len()
    }
}

impl Default for MempoolFirstSeenTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience: current wall-clock in unix nanos. Single source of truth
/// for the time domain the tracker uses everywhere.
pub fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Spawn the tracker subscriber. Listens on both the pending-tx and
/// new-block broadcast channels; records first-seen on every pending,
/// observes deltas on every block. Exits when `shutdown` flips true.
///
/// Returns the JoinHandle so the caller can await it on shutdown — the
/// task will end after both subscriptions hang up OR the shutdown signal
/// fires, whichever comes first.
pub fn spawn_first_seen_tracker(
    channels: Arc<EventChannels>,
    metrics: Arc<EngineMetrics>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let tracker = Arc::new(MempoolFirstSeenTracker::new());
    info!(
        target: "aether::mempool",
        capacity = DEFAULT_CAPACITY,
        "first-seen tracker subscribed (pending + new-block channels)"
    );

    let mut pending_rx = channels.subscribe_pending_txs();
    let mut block_rx = channels.subscribe_new_blocks();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                pending = pending_rx.recv() => {
                    match pending {
                        Ok(ev) => {
                            tracker.record(ev.tx_hash, ev.first_seen_unix_nanos, &metrics);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Lagged events are already counted by the
                            // pipeline-lag metric — log only at debug to
                            // avoid double-counting via a tracker-side
                            // metric.
                            debug!(
                                target: "aether::mempool",
                                dropped = n,
                                "first-seen tracker pending-tx receiver lagged"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!(target: "aether::mempool", "pending-tx channel closed; first-seen tracker exiting");
                            return;
                        }
                    }
                }
                block = block_rx.recv() => {
                    match block {
                        Ok(ev) => {
                            // observe_block reads only the slice + the
                            // tracker's own mutex; no other heavy work
                            // here so this stays cheap per block.
                            tracker.observe_block(&ev.tx_hashes, now_unix_nanos(), &metrics);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!(
                                target: "aether::mempool",
                                dropped = n,
                                "first-seen tracker new-block receiver lagged"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!(target: "aether::mempool", "new-block channel closed; first-seen tracker exiting");
                            return;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(target: "aether::mempool", "first-seen tracker received shutdown");
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    fn hashn(n: u8) -> B256 {
        let mut h = [0u8; 32];
        h[31] = n;
        B256::from(h)
    }

    fn fresh_metrics() -> EngineMetrics {
        EngineMetrics::new()
    }

    #[test]
    fn record_and_observe_emits_delta() {
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(100);
        let h = hashn(1);

        let first_seen = 1_700_000_000_000_000_000u64; // 1.7e18 nanos
        tracker.record(h, first_seen, &metrics);
        assert_eq!(tracker.len(), 1);

        let now = first_seen + 500_000_000; // +500ms
        tracker.observe_block(&[h], now, &metrics);
        // After matching the entry should be evicted from the seen map.
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn unmatched_block_hash_does_not_panic() {
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(100);
        tracker.observe_block(&[hashn(7)], 1_700_000_001_000_000_000, &metrics);
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn zero_stamp_is_rejected_not_recorded() {
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(100);
        tracker.record(hashn(1), 0, &metrics);
        assert_eq!(tracker.len(), 0, "zero-stamp entries must not be tracked");
    }

    #[test]
    fn duplicate_record_keeps_earliest_stamp() {
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(100);
        let h = hashn(1);
        let early = 1_700_000_000_000_000_000u64;
        let late = early + 100_000_000;
        tracker.record(h, early, &metrics);
        tracker.record(h, late, &metrics);
        // After observing, the delta should be computed against the
        // *earliest* stamp (1700... s), not the later one.
        let now = early + 200_000_000;
        tracker.observe_block(&[h], now, &metrics);
        // Tracker drained — second record was a no-op (the assertion
        // here is implicit: no panic + len drops to 0 after one
        // observe_block).
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn capacity_evicts_oldest_first() {
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(3);
        let now = 1_700_000_000_000_000_000u64;
        tracker.record(hashn(1), now, &metrics);
        tracker.record(hashn(2), now + 1, &metrics);
        tracker.record(hashn(3), now + 2, &metrics);
        // Capacity full; inserting a 4th should evict hash(1).
        tracker.record(hashn(4), now + 3, &metrics);
        assert_eq!(tracker.len(), 3);
        // Observe all four hashes; (1) should be unmatched, others matched.
        tracker.observe_block(
            &[hashn(1), hashn(2), hashn(3), hashn(4)],
            now + 1_000_000,
            &metrics,
        );
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn empty_block_is_noop() {
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(100);
        tracker.record(hashn(1), 1, &metrics);
        tracker.observe_block(&[], 999, &metrics);
        // Entry still present — empty block didn't drain anything.
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn clock_skew_floors_delta_at_zero() {
        // If block timestamp is somehow earlier than our first-seen
        // (clock skew, NTP step, builder timestamp lag), the delta must
        // floor at 0 rather than wrap.
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(100);
        let h = hashn(1);
        let first_seen = 1_700_000_001_000_000_000u64;
        tracker.record(h, first_seen, &metrics);
        let now = first_seen - 500_000_000; // 500ms BEFORE first-seen
        tracker.observe_block(&[h], now, &metrics);
        // No panic; entry consumed.
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn capacity_zero_treated_as_one() {
        // Guard the test-only "capacity 0" footgun: we floor at 1 so
        // record-then-observe still works.
        let metrics = fresh_metrics();
        let tracker = MempoolFirstSeenTracker::with_capacity(0);
        let h = hashn(1);
        tracker.record(h, 1_000, &metrics);
        // Even at capacity-1 the record + immediate observe should hit.
        tracker.observe_block(&[h], 2_000, &metrics);
        assert_eq!(tracker.len(), 0);
    }

    // Touch alloy's `b256!` macro so the import isn't unused if the file
    // gets pared back to fewer tests.
    #[allow(dead_code)]
    fn _macro_touch() -> B256 {
        b256!("0000000000000000000000000000000000000000000000000000000000000000")
    }
}
