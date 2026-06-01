//! Mempool hot-token frequency tracker.
//!
//! Counts how often each *unordered* token pair appears in decoded pending-tx
//! swaps, with exponential time-decay so the ranking reflects **recent**
//! trading activity rather than all-time totals. It feeds two consumers:
//!
//!   1. **Operator visibility** — "which tokens is the mempool trading most
//!      right now", surfaced by the periodic reporter in the mempool pipeline.
//!   2. **The pool-admission gate** (follow-up work) — a ranked candidate list
//!      of pairs whose pools are not yet in the registry, to be qualified
//!      (≥2 venues, liquidity/age, fee-on-transfer screen) before admission.
//!
//! Design notes:
//! - **Clock-injected.** `record` / `ranked` / `prune` take an explicit
//!   `now_secs`, so the core ranking is fully deterministic and unit-testable
//!   without a real clock or RPC. The pipeline supplies wall-clock seconds.
//! - **Registry-agnostic.** The tracker never looks at the pool registry; it
//!   only counts what the decoder emits. Cross-checking which pairs are already
//!   registered (and the authoritative ≥2-qualified-venue check) is the
//!   admission gate's job.
//! - **Bounded memory.** [`HotTokenTracker::prune`] drops pairs once their
//!   decayed score falls below a floor, so transient shitcoin churn does not
//!   grow the table without bound.

use std::collections::HashMap;
use std::sync::Mutex;

use aether_pools::router_decoder::Protocol;
use alloy::primitives::Address;

/// Unordered token-pair key: the two token addresses sorted ascending so that
/// `(A, B)` and `(B, A)` collapse to a single entry.
pub type PairKey = (Address, Address);

/// Canonicalise a token pair into a [`PairKey`] (addresses sorted ascending).
#[inline]
pub fn pair_key(a: Address, b: Address) -> PairKey {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Tuning for the tracker. Defaults target a ~5 min half-life — recent bursts
/// dominate the ranking, while day-old noise decays toward zero.
#[derive(Clone, Debug)]
pub struct HotTokenConfig {
    /// Decay half-life in seconds: a pair's score halves for every
    /// `half_life_secs` of silence. Smaller = more reactive to fresh bursts.
    pub half_life_secs: f64,
    /// Drop a pair from the table once its decayed score falls below this floor
    /// (keeps memory bounded as transient tokens go cold).
    pub prune_below_score: f64,
    /// Minimum distinct decoder-visible venues (protocol + optional pool) before
    /// a pair is reported as an admission *candidate*. Coarse pre-filter only —
    /// the authoritative "≥2 qualified venues" check belongs to the admission
    /// gate, which queries factories/registry for the pair.
    pub min_venues: usize,
}

impl Default for HotTokenConfig {
    fn default() -> Self {
        Self {
            half_life_secs: 300.0,
            prune_below_score: 0.05,
            min_venues: 2,
        }
    }
}

/// Per-pair accumulator. `score` is only correct as of `last_update_secs`;
/// callers must re-decay to "now" when reading (see [`decay_factor`]).
#[derive(Clone, Debug)]
struct PairStat {
    score: f64,
    last_update_secs: u64,
    /// Raw lifetime hit count (never decays) — handy for sanity / debugging.
    hits: u64,
    /// Distinct decoder-visible venues: `(protocol_code, pool_address)`. V2 /
    /// SushiSwap swaps carry no pool address in calldata (it is resolved
    /// downstream via the registry pair-index), so they dedupe by protocol.
    venues: Vec<(u8, Option<Address>)>,
    first_seen_secs: u64,
    last_seen_secs: u64,
}

/// One ranked hot pair, returned by [`HotTokenTracker::ranked`] /
/// [`HotTokenTracker::candidates`].
#[derive(Clone, Debug, PartialEq)]
pub struct HotPair {
    pub token_a: Address,
    pub token_b: Address,
    /// Decayed activity score as of the queried `now_secs`.
    pub score: f64,
    pub hits: u64,
    /// Count of distinct decoder-visible venues observed for this pair.
    pub venues: usize,
    pub first_seen_secs: u64,
    pub last_seen_secs: u64,
}

/// Stable small-int code for a decoder protocol, used for venue de-duplication.
fn protocol_code(p: Protocol) -> u8 {
    match p {
        Protocol::UniswapV2 => 1,
        Protocol::UniswapV3 => 2,
        Protocol::SushiSwap => 3,
        Protocol::Curve => 4,
        Protocol::BalancerV2 => 5,
        Protocol::BancorV3 => 6,
        Protocol::OneInchV6 => 7,
    }
}

/// Multiplicative decay applied to a score after `dt_secs` of silence.
/// `0.5 ^ (dt / half_life)` — i.e. one half-life halves the score.
#[inline]
fn decay_factor(dt_secs: u64, half_life_secs: f64) -> f64 {
    if half_life_secs <= 0.0 {
        return 1.0;
    }
    0.5_f64.powf(dt_secs as f64 / half_life_secs)
}

/// Thread-safe, time-decaying frequency counter over token pairs.
///
/// Written from the single mempool recv-loop task and read infrequently (the
/// periodic reporter / admission gate), so a `Mutex<HashMap>` is more than
/// adequate and avoids pulling in a concurrent map dependency.
pub struct HotTokenTracker {
    cfg: HotTokenConfig,
    stats: Mutex<HashMap<PairKey, PairStat>>,
}

impl HotTokenTracker {
    pub fn new(cfg: HotTokenConfig) -> Self {
        Self {
            cfg,
            stats: Mutex::new(HashMap::new()),
        }
    }

    /// Record one decoded swap observed at `now_secs`. `pool_address` is the
    /// decoder-peeled pool when known (UniswapV3 / 1inch encode it in
    /// calldata), `None` otherwise. Self-swaps (`token_in == token_out`) are
    /// ignored.
    pub fn record(
        &self,
        token_in: Address,
        token_out: Address,
        protocol: Protocol,
        pool_address: Option<Address>,
        now_secs: u64,
    ) {
        if token_in == token_out {
            return;
        }
        let key = pair_key(token_in, token_out);
        let venue = (protocol_code(protocol), pool_address);
        let mut stats = self.stats.lock().expect("hot-token mutex poisoned");
        let entry = stats.entry(key).or_insert_with(|| PairStat {
            score: 0.0,
            last_update_secs: now_secs,
            hits: 0,
            venues: Vec::new(),
            first_seen_secs: now_secs,
            last_seen_secs: now_secs,
        });
        // Decay the running score to `now` before adding this hit. Clamp the
        // delta at zero so an out-of-order / regressing clock cannot inflate
        // the score (it would otherwise raise 0.5 to a negative power).
        let dt = now_secs.saturating_sub(entry.last_update_secs);
        entry.score = entry.score * decay_factor(dt, self.cfg.half_life_secs) + 1.0;
        entry.last_update_secs = entry.last_update_secs.max(now_secs);
        entry.last_seen_secs = entry.last_seen_secs.max(now_secs);
        entry.hits += 1;
        if !entry.venues.contains(&venue) {
            entry.venues.push(venue);
        }
    }

    /// Decayed score for a single pair as of `now_secs` (0.0 if never seen).
    pub fn score(&self, key: &PairKey, now_secs: u64) -> f64 {
        let stats = self.stats.lock().expect("hot-token mutex poisoned");
        stats
            .get(key)
            .map(|s| {
                let dt = now_secs.saturating_sub(s.last_update_secs);
                s.score * decay_factor(dt, self.cfg.half_life_secs)
            })
            .unwrap_or(0.0)
    }

    /// Number of distinct token pairs currently tracked.
    pub fn len(&self) -> usize {
        self.stats.lock().expect("hot-token mutex poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All tracked pairs ranked by decayed score (descending) as of `now_secs`.
    pub fn ranked(&self, now_secs: u64) -> Vec<HotPair> {
        let stats = self.stats.lock().expect("hot-token mutex poisoned");
        let mut out: Vec<HotPair> = stats
            .iter()
            .map(|(k, s)| {
                let dt = now_secs.saturating_sub(s.last_update_secs);
                HotPair {
                    token_a: k.0,
                    token_b: k.1,
                    score: s.score * decay_factor(dt, self.cfg.half_life_secs),
                    hits: s.hits,
                    venues: s.venues.len(),
                    first_seen_secs: s.first_seen_secs,
                    last_seen_secs: s.last_seen_secs,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// Top-`n` hot pairs meeting the `min_venues` pre-filter — the admission
    /// candidate list. The caller cross-checks the registry to keep only pairs
    /// whose pools are not already tracked.
    pub fn candidates(&self, now_secs: u64, n: usize) -> Vec<HotPair> {
        self.ranked(now_secs)
            .into_iter()
            .filter(|p| p.venues >= self.cfg.min_venues)
            .take(n)
            .collect()
    }

    /// Drop pairs whose decayed score has fallen below `prune_below_score`.
    /// Returns the number of pairs removed. Call periodically to bound memory.
    pub fn prune(&self, now_secs: u64) -> usize {
        let mut stats = self.stats.lock().expect("hot-token mutex poisoned");
        let before = stats.len();
        let half_life = self.cfg.half_life_secs;
        let floor = self.cfg.prune_below_score;
        stats.retain(|_, s| {
            let dt = now_secs.saturating_sub(s.last_update_secs);
            s.score * decay_factor(dt, half_life) >= floor
        });
        before - stats.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    #[test]
    fn pair_key_is_order_independent() {
        let a = addr(0x11);
        let b = addr(0x22);
        assert_eq!(pair_key(a, b), pair_key(b, a));
        // Sorted ascending.
        assert_eq!(pair_key(b, a), (a, b));
    }

    #[test]
    fn record_counts_hits_and_score() {
        let t = HotTokenTracker::new(HotTokenConfig::default());
        let (a, b) = (addr(1), addr(2));
        t.record(a, b, Protocol::UniswapV2, None, 0);
        t.record(b, a, Protocol::UniswapV2, None, 0); // reverse order, same pair
        let ranked = t.ranked(0);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].hits, 2);
        // Two same-instant hits with no decay => score 2.0.
        assert!((ranked[0].score - 2.0).abs() < 1e-9);
    }

    #[test]
    fn self_swap_is_ignored() {
        let t = HotTokenTracker::new(HotTokenConfig::default());
        let a = addr(7);
        t.record(a, a, Protocol::UniswapV2, None, 0);
        assert!(t.is_empty());
    }

    #[test]
    fn score_halves_after_one_half_life() {
        let cfg = HotTokenConfig {
            half_life_secs: 100.0,
            ..HotTokenConfig::default()
        };
        let t = HotTokenTracker::new(cfg);
        let (a, b) = (addr(1), addr(2));
        t.record(a, b, Protocol::UniswapV2, None, 0); // score = 1.0 at t=0
        let key = pair_key(a, b);
        // Read 100s later: one half-life => 0.5.
        assert!((t.score(&key, 100) - 0.5).abs() < 1e-9);
        // 200s later => 0.25.
        assert!((t.score(&key, 200) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn decayed_record_accumulates_correctly() {
        let cfg = HotTokenConfig {
            half_life_secs: 100.0,
            ..HotTokenConfig::default()
        };
        let t = HotTokenTracker::new(cfg);
        let (a, b) = (addr(1), addr(2));
        t.record(a, b, Protocol::UniswapV2, None, 0); // 1.0 at t=0
        t.record(a, b, Protocol::UniswapV2, None, 100); // decay 1.0->0.5, +1 => 1.5
        let key = pair_key(a, b);
        assert!((t.score(&key, 100) - 1.5).abs() < 1e-9);
    }

    #[test]
    fn venues_dedupe_by_protocol_and_pool() {
        let t = HotTokenTracker::new(HotTokenConfig::default());
        let (a, b) = (addr(1), addr(2));
        // Same protocol, no pool address -> one venue however many hits.
        t.record(a, b, Protocol::UniswapV2, None, 0);
        t.record(a, b, Protocol::UniswapV2, None, 0);
        assert_eq!(t.ranked(0)[0].venues, 1);
        // A different protocol adds a venue.
        t.record(a, b, Protocol::SushiSwap, None, 0);
        assert_eq!(t.ranked(0)[0].venues, 2);
        // Distinct V3 pools count separately.
        t.record(a, b, Protocol::UniswapV3, Some(addr(0xAA)), 0);
        t.record(a, b, Protocol::UniswapV3, Some(addr(0xBB)), 0);
        assert_eq!(t.ranked(0)[0].venues, 4);
    }

    #[test]
    fn ranked_orders_by_decayed_score() {
        let t = HotTokenTracker::new(HotTokenConfig::default());
        let (a, b, c) = (addr(1), addr(2), addr(3));
        // pair (a,b) hit twice, (a,c) once -> (a,b) ranks first.
        t.record(a, b, Protocol::UniswapV2, None, 0);
        t.record(a, b, Protocol::UniswapV2, None, 0);
        t.record(a, c, Protocol::UniswapV2, None, 0);
        let ranked = t.ranked(0);
        assert_eq!(ranked.len(), 2);
        assert_eq!(pair_key(ranked[0].token_a, ranked[0].token_b), pair_key(a, b));
    }

    #[test]
    fn candidates_filter_by_min_venues_and_take_n() {
        let cfg = HotTokenConfig {
            min_venues: 2,
            ..HotTokenConfig::default()
        };
        let t = HotTokenTracker::new(cfg);
        let (a, b, c) = (addr(1), addr(2), addr(3));
        // (a,b): 2 venues -> candidate. (a,c): 1 venue -> excluded.
        t.record(a, b, Protocol::UniswapV2, None, 0);
        t.record(a, b, Protocol::SushiSwap, None, 0);
        t.record(a, c, Protocol::UniswapV2, None, 0);
        let cands = t.candidates(0, 10);
        assert_eq!(cands.len(), 1);
        assert_eq!(pair_key(cands[0].token_a, cands[0].token_b), pair_key(a, b));
        // take(n) bounds output.
        assert_eq!(t.candidates(0, 0).len(), 0);
    }

    #[test]
    fn prune_drops_cold_pairs() {
        let cfg = HotTokenConfig {
            half_life_secs: 10.0,
            prune_below_score: 0.1,
            ..HotTokenConfig::default()
        };
        let t = HotTokenTracker::new(cfg);
        let (a, b) = (addr(1), addr(2));
        t.record(a, b, Protocol::UniswapV2, None, 0); // score 1.0 at t=0
        // After ~4 half-lives (40s) score ~0.0625 < 0.1 floor -> pruned.
        let removed = t.prune(40);
        assert_eq!(removed, 1);
        assert!(t.is_empty());
    }

    #[test]
    fn clock_regression_does_not_panic_or_inflate() {
        let t = HotTokenTracker::new(HotTokenConfig::default());
        let (a, b) = (addr(1), addr(2));
        t.record(a, b, Protocol::UniswapV2, None, 1000);
        // A later record with an earlier timestamp must not raise score above
        // the no-decay sum (clamped dt = 0).
        t.record(a, b, Protocol::UniswapV2, None, 500);
        let key = pair_key(a, b);
        assert!(t.score(&key, 1000) <= 2.0 + 1e-9);
    }
}
