//! In-memory cache of the latest packed reserves slot for UniswapV2 /
//! SushiSwap pools, fed by the WebSocket `Sync` event stream.
//!
//! UniV2's `getReserves()` reads a single packed slot at storage index 8:
//!
//! ```text
//! slot8 = (blockTimestampLast << 224) | (reserve1 << 112) | reserve0
//! ```
//!
//! The detector already subscribes to every monitored pool's `Sync` event
//! and decodes the new reserve pair on every block. Today we throw that
//! data away after the price-graph edge update and pay a fresh
//! `eth_getStorageAt` round-trip per pool on every pre-warm cycle to refetch
//! information that just arrived for free over the WS.
//!
//! `V2ReservesCache` captures the latest decoded reserves and exposes them
//! as a synthesised slot-8 value for the pre-warm path. The `blockTimestampLast`
//! field is *not* part of a Sync event, so we leave it zero — UniV2 swap
//! math does not depend on it (only `_update()` writes the field, and that
//! happens *after* the swap quantities are computed). Any consumer that
//! does need a live timestamp must still fall through to RPC.
//!
//! All operations are O(1) lock-free via `DashMap`. The cache is cheap to
//! clone (it's `Arc`-backed) so the engine writer side and the pre-warm
//! reader side share a single shared instance.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use dashmap::DashMap;

/// Bit shift for `reserve1` inside UniV2's packed slot 8.
const RESERVE1_SHIFT: u32 = 112;

/// 112-bit mask used when packing `reserve0` / `reserve1` into slot 8.
fn uint112_mask() -> U256 {
    (U256::from(1u64) << 112) - U256::from(1u64)
}

/// Latest reserve pair captured for a single pool. `block_number` is the
/// chain head at which the underlying Sync event was emitted; the pre-warm
/// path can use it to reject stale entries during reorgs.
#[derive(Clone, Copy, Debug)]
pub struct ReserveSnapshot {
    pub reserve0: U256,
    pub reserve1: U256,
    pub block_number: u64,
}

impl ReserveSnapshot {
    /// Encode the snapshot as a UniV2 packed slot-8 value.
    /// `blockTimestampLast` is forced to 0 — see module docs.
    pub fn pack_slot8(&self) -> U256 {
        let mask = uint112_mask();
        let r0 = self.reserve0 & mask;
        let r1 = self.reserve1 & mask;
        r0 | (r1 << RESERVE1_SHIFT)
    }
}

/// Lock-free cache of the latest per-pool reserve snapshot.
#[derive(Clone, Default)]
pub struct V2ReservesCache {
    inner: Arc<DashMap<Address, ReserveSnapshot>>,
}

impl V2ReservesCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct pools currently cached. Useful for diagnostics.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Record the latest reserves for `pool` at `block_number`. Newer
    /// `block_number` always wins; older updates are dropped so a delayed
    /// out-of-order event cannot poison the cache with stale state.
    pub fn record(&self, pool: Address, reserve0: U256, reserve1: U256, block_number: u64) {
        self.inner
            .entry(pool)
            .and_modify(|existing| {
                if block_number >= existing.block_number {
                    existing.reserve0 = reserve0;
                    existing.reserve1 = reserve1;
                    existing.block_number = block_number;
                }
            })
            .or_insert(ReserveSnapshot {
                reserve0,
                reserve1,
                block_number,
            });
    }

    /// Look up the latest snapshot for `pool` if one has been captured.
    pub fn get(&self, pool: Address) -> Option<ReserveSnapshot> {
        self.inner.get(&pool).map(|v| *v.value())
    }

    /// Look up the latest snapshot for `pool` only if it is fresh enough for
    /// `target_block` — i.e. emitted no earlier than `target_block.saturating_sub(max_lag)`.
    /// Stale entries return `None` so the caller falls through to RPC.
    pub fn get_fresh(
        &self,
        pool: Address,
        target_block: u64,
        max_lag: u64,
    ) -> Option<ReserveSnapshot> {
        let snap = self.get(pool)?;
        let oldest_acceptable = target_block.saturating_sub(max_lag);
        if snap.block_number >= oldest_acceptable {
            Some(snap)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn record_then_get_roundtrip() {
        let cache = V2ReservesCache::new();
        let pool = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        cache.record(pool, U256::from(100u64), U256::from(200u64), 42);
        let s = cache.get(pool).expect("hit");
        assert_eq!(s.reserve0, U256::from(100u64));
        assert_eq!(s.reserve1, U256::from(200u64));
        assert_eq!(s.block_number, 42);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn get_returns_none_when_unseen() {
        let cache = V2ReservesCache::new();
        let pool = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert!(cache.get(pool).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn newer_block_overwrites_older() {
        let cache = V2ReservesCache::new();
        let pool = address!("cccccccccccccccccccccccccccccccccccccccc");
        cache.record(pool, U256::from(1u64), U256::from(2u64), 100);
        cache.record(pool, U256::from(3u64), U256::from(4u64), 101);
        let s = cache.get(pool).unwrap();
        assert_eq!(s.reserve0, U256::from(3u64));
        assert_eq!(s.reserve1, U256::from(4u64));
        assert_eq!(s.block_number, 101);
    }

    #[test]
    fn out_of_order_older_event_is_dropped() {
        // Sync events occasionally arrive out of order during a reorg or a
        // mid-stream WS reconnect. An older `block_number` must not replace
        // the freshest snapshot or the cache would poison the pre-warm path
        // with stale state.
        let cache = V2ReservesCache::new();
        let pool = address!("dddddddddddddddddddddddddddddddddddddddd");
        cache.record(pool, U256::from(3u64), U256::from(4u64), 101);
        cache.record(pool, U256::from(1u64), U256::from(2u64), 100); // older
        let s = cache.get(pool).unwrap();
        assert_eq!(s.reserve0, U256::from(3u64));
        assert_eq!(s.block_number, 101);
    }

    #[test]
    fn pack_slot8_layout_matches_uniswap_v2() {
        // The synthesised slot8 layout must place reserve0 in bits 0..111,
        // reserve1 in bits 112..223, and leave the high 32 bits (where
        // `blockTimestampLast` would live) untouched. Use small, distinct
        // values that fit comfortably inside 112 bits.
        let snap = ReserveSnapshot {
            reserve0: U256::from(0xAABBCCu64),
            reserve1: U256::from(0xDDEEFFu64),
            block_number: 1,
        };
        let packed = snap.pack_slot8();
        let mask112 = uint112_mask();
        let r0_back = packed & mask112;
        let r1_back = (packed >> RESERVE1_SHIFT) & mask112;
        assert_eq!(r0_back, U256::from(0xAABBCCu64));
        assert_eq!(r1_back, U256::from(0xDDEEFFu64));
        // High 32 bits = blockTimestampLast region must be zero.
        let ts_region = packed >> 224;
        assert_eq!(ts_region, U256::ZERO, "timestamp region must stay zero");
    }

    #[test]
    fn pack_slot8_truncates_to_uint112() {
        // Real on-chain values are already uint112, but defensively the
        // packer must mask anything wider so a malformed input never
        // contaminates the reserve1 / timestamp bands.
        let mask112 = uint112_mask();
        let oversized = (U256::from(1u64) << 130) | U256::from(7u64);
        let snap = ReserveSnapshot {
            reserve0: oversized,
            reserve1: oversized,
            block_number: 1,
        };
        let packed = snap.pack_slot8();
        let r0_back = packed & mask112;
        let r1_back = (packed >> RESERVE1_SHIFT) & mask112;
        // The bit-130 value is dropped because it falls outside the 112-bit
        // window; only the low 7 survives.
        assert_eq!(r0_back, U256::from(7u64));
        assert_eq!(r1_back, U256::from(7u64));
        // No overflow into the timestamp band.
        let ts_region = packed >> 224;
        assert_eq!(ts_region, U256::ZERO);
    }

    #[test]
    fn get_fresh_accepts_within_lag_window() {
        let cache = V2ReservesCache::new();
        let pool = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
        cache.record(pool, U256::from(1u64), U256::from(2u64), 100);
        // Target block 102, lag 3 -> oldest acceptable = 99; cached at 100 -> hit.
        assert!(cache.get_fresh(pool, 102, 3).is_some());
    }

    #[test]
    fn get_fresh_rejects_stale_snapshot() {
        let cache = V2ReservesCache::new();
        let pool = address!("ffffffffffffffffffffffffffffffffffffffff");
        cache.record(pool, U256::from(1u64), U256::from(2u64), 50);
        // Target block 100, lag 3 -> oldest acceptable = 97; cached at 50 -> miss.
        assert!(cache.get_fresh(pool, 100, 3).is_none());
    }
}
