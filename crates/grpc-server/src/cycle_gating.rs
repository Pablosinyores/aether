//! Multi-signal candidate gating for detected arb cycles.
//!
//! Bellman-Ford produces cycles from a directed price graph. When the
//! graph contains a corrupted edge (stale or zero reserves on a pool that
//! has not been refreshed from chain yet), the `-ln(rate)` weight on that
//! edge collapses toward `-Infinity`, every cycle traversing it reports
//! a `profit_factor` measured in billions of percent, and the downstream
//! simulator wastes 100-500 ms per cycle on a calldata that can never
//! settle profitably on-chain.
//!
//! This module sits between cycle detection and EVM simulation. It
//! applies three cheap, independent gates that each catch a different
//! corruption signature, plus a post-simulation cross-check that catches
//! anything the pre-sim gates missed:
//!
//! 1. **TVL** — every edge in the cycle has at least
//!    `MIN_RESERVE_F64` worth of input-side liquidity. An empty pool
//!    cannot produce a real arb regardless of the rate the graph thinks
//!    it has.
//!
//! 2. **Multi-cycle fingerprint** — when five or more sibling cycles in
//!    the same detection pass share the same `profit_factor` (quantised
//!    to 1e-6), one corrupt edge is being walked through many path
//!    permutations. Drop the whole cluster, not just one of its members.
//!
//! 3. **Hard sanity cap** — `profit_factor > 100.0` (10000%) is f64
//!    overflow territory and never a real opportunity even during stable-
//!    coin depegs (UST's peak was 90%). Drop immediately without further
//!    work.
//!
//! 4. **Post-sim revm cross-check** — after EVM fork simulation, compare
//!    `sim_result.profit_wei` against the detector's `expected_net_wei`.
//!    If the two differ by more than 50% the local graph snapshot the
//!    detector ran against is out of sync with the chain; trust revm and
//!    drop the candidate before it reaches the executor.
//!
//! Every drop bumps `aether_cycle_gate_dropped_total{reason="..."}` with
//! a label drawn from a fixed set so dashboards can pre-render every panel
//! without churn.

use aether_detector::opportunity::DetectedCycle;
use aether_state::price_graph::PriceGraph;
use tracing::{info, warn};

use crate::EngineMetrics;

/// Tunable thresholds for the four gates. Defaults are calibrated for the
/// blue-chip pool registry that ships in `config/pools.toml`; deployments
/// monitoring long-tail or freshly-launched pools may want to relax the
/// `min_reserve_f64` and `profit_factor_impossible` bounds.
#[derive(Debug, Clone, Copy)]
pub struct GatingConfig {
    /// Gate 1: minimum `reserve_in` value (f64 wei) on every edge in the
    /// cycle. Below this the pool is empty enough that the rate is
    /// noise rather than signal.
    pub min_reserve_f64: f64,
    /// Gate 2: number of sibling cycles that must share the same
    /// `profit_factor` (quantised to `fingerprint_quantum`) before the
    /// cluster is judged a corruption signature.
    pub fingerprint_min_cluster: usize,
    /// Gate 2: quantum used to bucket `profit_factor` values into
    /// fingerprint groups. `1e-6` collapses values within 1e-4 % of each
    /// other into a single bucket, sufficient to detect the
    /// "every cycle reports identical profit" overflow pattern.
    pub fingerprint_quantum: f64,
    /// Gate 3 / hard cap: `profit_factor` above this is always dropped.
    /// `100.0` = `10000%` profit, well above any historical depeg or
    /// liquidity event.
    pub profit_factor_impossible: f64,
    /// Gate 3 / soft warn: `profit_factor` above this but below
    /// `profit_factor_impossible` is logged at `warn` with the full cycle
    /// path so an operator can audit whether the graph snapshot was
    /// stale. The cycle still proceeds; this gate never drops on its own.
    pub profit_factor_suspicious: f64,
    /// Gate 4: maximum tolerated fractional disagreement between the
    /// detector's predicted profit and revm's measured profit. Above
    /// this the candidate is dropped after simulation, before it is
    /// handed to the executor.
    pub revm_profit_mismatch_threshold: f64,
}

impl Default for GatingConfig {
    fn default() -> Self {
        Self {
            // 1e6 wei = 0.000001 ETH on the input side. Below this any
            // real swap is dust; the f64 rate is dominated by rounding.
            min_reserve_f64: 1.0e6,
            fingerprint_min_cluster: 5,
            fingerprint_quantum: 1.0e-6,
            // 100.0 = 10000%. UST's peak depeg was 90% — anything beyond
            // that is unequivocally math broken.
            profit_factor_impossible: 100.0,
            // 0.5 = 50%. Real arbs on blue-chip pools live well below
            // this; depegs and launch events live above. Worth a warn,
            // not a drop.
            profit_factor_suspicious: 0.5,
            // 0.5 = revm's profit must be within 50% of the detector's
            // estimate. Outside that window the graph snapshot was stale.
            revm_profit_mismatch_threshold: 0.5,
        }
    }
}

impl GatingConfig {
    /// Permissive config that disables every gate. Used by unit tests
    /// whose synthetic graphs populate edge weights via
    /// `PriceGraph::add_edge` (which leaves `reserve_in = 0.0`) and would
    /// otherwise be unconditionally dropped by the TVL gate. Never set
    /// this on a production engine.
    ///
    /// `dead_code` allowed because the release binary never calls this
    /// directly — only the test harness does — but it has to live in
    /// non-test code so `EngineConfig` (also non-test) can name the type
    /// returned here when tests construct a custom `EngineConfig`.
    #[allow(dead_code)]
    pub fn permissive() -> Self {
        Self {
            min_reserve_f64: 0.0,
            fingerprint_min_cluster: usize::MAX,
            fingerprint_quantum: 1.0e-6,
            profit_factor_impossible: f64::INFINITY,
            profit_factor_suspicious: f64::INFINITY,
            revm_profit_mismatch_threshold: f64::INFINITY,
        }
    }
}

/// Verdict of the pre-simulation gating layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreSimGateVerdict {
    /// Cycle passes all gates. May still log a `warn` if the
    /// `Suspicious` branch fired — that is observability only, not a drop.
    Pass,
    /// Cycle dropped before simulation. The reason corresponds to a
    /// label in `aether_cycle_gate_dropped_total`.
    Drop(GateDropReason),
}

/// Stable label set for `aether_cycle_gate_dropped_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDropReason {
    ProfitFactorImpossible,
    ReservesTooLow,
    FingerprintCluster,
    RevmContradicts,
}

impl GateDropReason {
    pub fn as_label(&self) -> &'static str {
        match self {
            GateDropReason::ProfitFactorImpossible => "profit_factor_impossible",
            GateDropReason::ReservesTooLow => "reserves_too_low",
            GateDropReason::FingerprintCluster => "fingerprint_cluster",
            GateDropReason::RevmContradicts => "revm_contradicts",
        }
    }
}

/// Quantise a `profit_factor` into a fingerprint bucket id. Values within
/// `quantum` of each other map to the same id, so the bucket counts in
/// [`build_fingerprint_index`] reflect "cycles that produced the same
/// profit number" rather than "cycles whose profits happen to round the
/// same way".
fn fingerprint_bucket(profit_factor: f64, quantum: f64) -> i64 {
    if !profit_factor.is_finite() {
        // NaN / +-inf collapse into the same bucket so the fingerprint
        // gate also catches the f64-overflow signature even when the
        // hard cap below would otherwise not fire (e.g. NaN < 100.0
        // returns false in Rust, so a NaN profit_factor would survive
        // the impossible gate without this branch).
        return i64::MAX;
    }
    (profit_factor / quantum).round() as i64
}

/// Build a per-bucket count of `profit_factor` fingerprints across an
/// entire detection pass. The result is consumed by the multi-cycle
/// fingerprint gate (gate 2): any cycle whose bucket count exceeds
/// `config.fingerprint_min_cluster` is treated as belonging to a
/// corruption cluster.
pub fn build_fingerprint_index(
    cycles: &[DetectedCycle],
    config: &GatingConfig,
) -> std::collections::HashMap<i64, usize> {
    let mut counts = std::collections::HashMap::new();
    for cycle in cycles {
        if !cycle.is_profitable() {
            continue;
        }
        let bucket = fingerprint_bucket(cycle.profit_factor(), config.fingerprint_quantum);
        *counts.entry(bucket).or_insert(0) += 1;
    }
    counts
}

/// Run the pre-simulation gates on a single cycle.
///
/// `fingerprint_index` must be the output of [`build_fingerprint_index`]
/// over the full cycle batch returned by Bellman-Ford in this detection
/// pass — building it inside this function would be O(N) per cycle and
/// quadratic across the batch.
pub fn gate_pre_sim(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    fingerprint_index: &std::collections::HashMap<i64, usize>,
    config: &GatingConfig,
    metrics: &EngineMetrics,
) -> PreSimGateVerdict {
    let pf = cycle.profit_factor();

    // ── Gate 3: hard sanity cap ──────────────────────────────────────
    // Checked first because it is the cheapest signal and catches the
    // f64-overflow signature directly without needing graph traversal.
    if !pf.is_finite() || pf > config.profit_factor_impossible {
        metrics.inc_cycle_gate_dropped(GateDropReason::ProfitFactorImpossible.as_label());
        return PreSimGateVerdict::Drop(GateDropReason::ProfitFactorImpossible);
    }

    // ── Gate 1: TVL on every edge ────────────────────────────────────
    // Walk the cycle path and confirm each edge has at least
    // `min_reserve_f64` of input-side reserves. Corrupt edges typically
    // have `reserve_in = 0.0` (placeholder seed never refreshed by chain
    // events); without this check Bellman-Ford treats them as infinite-
    // rate arbitrage sources.
    for i in 0..cycle.path.len().saturating_sub(1) {
        let from = cycle.path[i];
        let to = cycle.path[i + 1];
        // Find the edge that BF picked for this hop. There may be
        // multiple parallel pools backing the same `(from, to)` pair, so
        // we accept the cycle if at least one of them passes the TVL
        // check — BF will route through the best one downstream anyway.
        let any_edge_passes = graph
            .edges_from(from)
            .iter()
            .filter(|e| e.to == to)
            .any(|e| e.reserve_in >= config.min_reserve_f64);
        if !any_edge_passes {
            metrics.inc_cycle_gate_dropped(GateDropReason::ReservesTooLow.as_label());
            return PreSimGateVerdict::Drop(GateDropReason::ReservesTooLow);
        }
    }

    // ── Gate 2: multi-cycle fingerprint ──────────────────────────────
    let bucket = fingerprint_bucket(pf, config.fingerprint_quantum);
    let cluster_size = fingerprint_index.get(&bucket).copied().unwrap_or(0);
    if cluster_size >= config.fingerprint_min_cluster {
        metrics.inc_cycle_gate_dropped(GateDropReason::FingerprintCluster.as_label());
        return PreSimGateVerdict::Drop(GateDropReason::FingerprintCluster);
    }

    // ── Suspicious-but-pass warn ─────────────────────────────────────
    // Anything above the soft threshold is unusual enough to surface so
    // an operator can decide whether to widen the registry, tighten the
    // hard cap, or accept the risk; not a drop on its own.
    if pf > config.profit_factor_suspicious {
        warn!(
            profit_factor = pf,
            hops = cycle.num_hops(),
            cluster_size,
            "suspicious profit factor — audit the graph snapshot before trusting downstream sim"
        );
    }

    PreSimGateVerdict::Pass
}

/// Verdict of the post-simulation revm cross-check (gate 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostSimGateVerdict {
    Pass,
    Drop(GateDropReason),
}

/// Compare the detector's profit estimate against revm's measured profit.
///
/// If the two differ by more than `config.revm_profit_mismatch_threshold`
/// fractional distance the detector's graph snapshot was stale relative
/// to chain — trust revm and drop the candidate before it is published
/// to the executor. The gate fires on either direction: a wildly
/// optimistic detector (the 21B ETH bug) AND a detector that under-
/// counted profit. Both indicate a graph-state mismatch worth flagging.
///
/// Special cases:
/// - `expected == 0`: never gate (nothing to compare against).
/// - `actual == 0` while `expected > 0`: always drop. revm says no profit
///   at all but detector predicted some — definitive disagreement.
pub fn gate_post_sim(
    expected_net_wei: u128,
    actual_profit_wei: u128,
    config: &GatingConfig,
    metrics: &EngineMetrics,
) -> PostSimGateVerdict {
    // Permissive bypass: when the operator (or a unit test) has set
    // `revm_profit_mismatch_threshold` to infinity, treat the gate as
    // disabled wholesale. This avoids the awkward case where the gate
    // is "configured off" by a giant threshold yet still fires its
    // zero-actual special case and drops candidates a permissive caller
    // explicitly does not want dropped.
    if !config.revm_profit_mismatch_threshold.is_finite() {
        return PostSimGateVerdict::Pass;
    }
    if expected_net_wei == 0 {
        return PostSimGateVerdict::Pass;
    }
    if actual_profit_wei == 0 {
        info!(
            expected_net_wei,
            actual_profit_wei,
            "REVM CONTRADICTS DETECTOR: zero actual vs nonzero expected"
        );
        metrics.inc_cycle_gate_dropped(GateDropReason::RevmContradicts.as_label());
        return PostSimGateVerdict::Drop(GateDropReason::RevmContradicts);
    }
    let expected_f = expected_net_wei as f64;
    let actual_f = actual_profit_wei as f64;
    let frac_diff = (expected_f - actual_f).abs() / expected_f.max(actual_f);
    if frac_diff > config.revm_profit_mismatch_threshold {
        info!(
            expected_net_wei,
            actual_profit_wei,
            frac_diff,
            "REVM CONTRADICTS DETECTOR: profit mismatch beyond threshold"
        );
        metrics.inc_cycle_gate_dropped(GateDropReason::RevmContradicts.as_label());
        return PostSimGateVerdict::Drop(GateDropReason::RevmContradicts);
    }
    PostSimGateVerdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_common::types::{PoolId, ProtocolType};
    use alloy::primitives::{Address, U256};

    fn make_cycle(weight: f64, hops: usize) -> DetectedCycle {
        // Build a closed cycle: [0, 1, ..., hops-1, 0]. The trailing 0
        // is what makes it a *cycle* (first vertex == last vertex);
        // earlier iterations of this helper produced an open path which
        // tripped the TVL gate because graph edges were only seeded for
        // the closed-cycle directions.
        let mut path: Vec<usize> = (0..hops).collect();
        path.push(0);
        DetectedCycle {
            path,
            total_weight: weight,
        }
    }

    fn empty_graph() -> PriceGraph {
        PriceGraph::new(8)
    }

    fn add_test_edge(
        g: &mut PriceGraph,
        from: usize,
        to: usize,
        reserve_in: f64,
        reserve_out: f64,
    ) {
        // Use the f64 ratio as the exchange rate; the test only cares
        // that the edge exists with the supplied reserves so the gate
        // logic can read them back.
        let rate = if reserve_in > 0.0 {
            reserve_out / reserve_in
        } else {
            1.0
        };
        let pool_id = PoolId {
            address: Address::ZERO,
            protocol: ProtocolType::UniswapV2,
        };
        g.add_edge(
            from,
            to,
            rate.max(1e-30),
            pool_id,
            Address::ZERO,
            ProtocolType::UniswapV2,
            U256::ZERO,
        );
        g.update_edge_from_reserves(from, to, pool_id, reserve_in, reserve_out, 0.997);
    }

    #[test]
    fn fingerprint_bucket_collapses_identical_profits() {
        let q = 1.0e-6;
        // The two values differ by ~1e-9, well inside the quantum.
        assert_eq!(fingerprint_bucket(0.025_000_001, q), fingerprint_bucket(0.025_000_002, q));
        // Values an order of magnitude apart land in different buckets.
        assert_ne!(fingerprint_bucket(0.025, q), fingerprint_bucket(0.030, q));
    }

    #[test]
    fn fingerprint_bucket_handles_nan_and_infinity() {
        let q = 1.0e-6;
        let nan_bucket = fingerprint_bucket(f64::NAN, q);
        let inf_bucket = fingerprint_bucket(f64::INFINITY, q);
        let neg_inf_bucket = fingerprint_bucket(f64::NEG_INFINITY, q);
        assert_eq!(nan_bucket, i64::MAX);
        assert_eq!(inf_bucket, i64::MAX);
        assert_eq!(neg_inf_bucket, i64::MAX);
    }

    #[test]
    fn build_fingerprint_index_counts_profitable_only() {
        let cycles = vec![
            make_cycle(-0.01, 2),
            make_cycle(-0.01, 2),
            make_cycle(-0.01, 2),
            make_cycle(0.01, 2), // unprofitable
        ];
        let config = GatingConfig::default();
        let index = build_fingerprint_index(&cycles, &config);
        let bucket = fingerprint_bucket((0.01f64).exp() - 1.0, config.fingerprint_quantum);
        assert_eq!(index.get(&bucket), Some(&3));
    }

    #[test]
    fn gate_pre_sim_drops_impossible_profit() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        let graph = empty_graph();
        let index = std::collections::HashMap::new();
        // total_weight = -ln(profit_factor + 1). For profit_factor = 1000
        // (= 100000%) we need total_weight = -ln(1001) ≈ -6.91.
        let cycle = make_cycle(-6.91, 2);
        let verdict = gate_pre_sim(&cycle, &graph, &index, &config, &metrics);
        assert_eq!(
            verdict,
            PreSimGateVerdict::Drop(GateDropReason::ProfitFactorImpossible)
        );
        assert_eq!(metrics.cycle_gate_dropped_count("profit_factor_impossible"), 1);
    }

    #[test]
    fn gate_pre_sim_drops_nan_profit() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        let graph = empty_graph();
        let index = std::collections::HashMap::new();
        // total_weight = +inf -> profit_factor = e^-inf - 1 = -1, which
        // is finite and negative (unprofitable). To force NaN we use
        // total_weight = NaN directly; profit_factor() = (-NaN).exp() - 1
        // = NaN.
        let cycle = DetectedCycle {
            path: vec![0, 1, 0],
            total_weight: f64::NAN,
        };
        let verdict = gate_pre_sim(&cycle, &graph, &index, &config, &metrics);
        assert!(matches!(
            verdict,
            PreSimGateVerdict::Drop(GateDropReason::ProfitFactorImpossible)
        ));
    }

    #[test]
    fn gate_pre_sim_drops_low_reserve_edge() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        let mut graph = empty_graph();
        // Cycle 0 -> 1 -> 0 with the edge 0->1 holding less than
        // min_reserve_f64 worth of input.
        add_test_edge(&mut graph, 0, 1, 1.0, 100.0);
        add_test_edge(&mut graph, 1, 0, 1.0e9, 1.0e9);
        let index = std::collections::HashMap::new();
        let cycle = DetectedCycle {
            path: vec![0, 1, 0],
            total_weight: -0.05, // realistic 5% profit
        };
        let verdict = gate_pre_sim(&cycle, &graph, &index, &config, &metrics);
        assert_eq!(verdict, PreSimGateVerdict::Drop(GateDropReason::ReservesTooLow));
        assert_eq!(metrics.cycle_gate_dropped_count("reserves_too_low"), 1);
    }

    #[test]
    fn gate_pre_sim_drops_fingerprint_cluster() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        let mut graph = empty_graph();
        add_test_edge(&mut graph, 0, 1, 1.0e9, 1.0e9);
        add_test_edge(&mut graph, 1, 0, 1.0e9, 1.0e9);
        // Build an index that already contains 6 sibling cycles at the
        // same bucket as our test cycle. Derive the bucket from the
        // cycle's own `profit_factor` rather than re-computing the f64
        // expression by hand — the two paths must agree exactly because
        // `fingerprint_bucket` rounds, and any sign error in the manual
        // computation would land in a different bucket and silently
        // pass the gate.
        let cycle = make_cycle(-0.05, 2);
        let bucket = fingerprint_bucket(cycle.profit_factor(), config.fingerprint_quantum);
        let mut index = std::collections::HashMap::new();
        index.insert(bucket, 6);
        let verdict = gate_pre_sim(&cycle, &graph, &index, &config, &metrics);
        assert_eq!(verdict, PreSimGateVerdict::Drop(GateDropReason::FingerprintCluster));
        assert_eq!(metrics.cycle_gate_dropped_count("fingerprint_cluster"), 1);
    }

    #[test]
    fn gate_pre_sim_passes_normal_arb() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        let mut graph = empty_graph();
        add_test_edge(&mut graph, 0, 1, 1.0e9, 1.0e9);
        add_test_edge(&mut graph, 1, 0, 1.0e9, 1.0e9);
        let index = std::collections::HashMap::new();
        let cycle = make_cycle(-0.001, 2); // 0.1% profit, normal
        let verdict = gate_pre_sim(&cycle, &graph, &index, &config, &metrics);
        assert_eq!(verdict, PreSimGateVerdict::Pass);
    }

    #[test]
    fn gate_post_sim_passes_when_profits_match() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        // expected and actual within 10% — should pass.
        let verdict = gate_post_sim(1_000_000, 950_000, &config, &metrics);
        assert_eq!(verdict, PostSimGateVerdict::Pass);
    }

    #[test]
    fn gate_post_sim_drops_on_zero_actual() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        let verdict = gate_post_sim(1_000_000, 0, &config, &metrics);
        assert_eq!(verdict, PostSimGateVerdict::Drop(GateDropReason::RevmContradicts));
        assert_eq!(metrics.cycle_gate_dropped_count("revm_contradicts"), 1);
    }

    #[test]
    fn gate_post_sim_drops_on_large_mismatch() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        // expected = 21B ETH, actual = trivial. Detector lied; trust revm.
        let expected: u128 = 21_000_000_000_000_000_000_000_000_000;
        let actual: u128 = 1_000_000_000_000_000;
        let verdict = gate_post_sim(expected, actual, &config, &metrics);
        assert_eq!(verdict, PostSimGateVerdict::Drop(GateDropReason::RevmContradicts));
    }

    #[test]
    fn gate_post_sim_passes_when_expected_zero() {
        let metrics = EngineMetrics::new();
        let config = GatingConfig::default();
        // expected = 0 means we have nothing to compare against; gate
        // is a no-op rather than a divide-by-zero panic.
        let verdict = gate_post_sim(0, 0, &config, &metrics);
        assert_eq!(verdict, PostSimGateVerdict::Pass);
    }
}
