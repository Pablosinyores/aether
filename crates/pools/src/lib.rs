pub mod balancer;
pub mod bancor;
pub mod curve;
pub mod registry;
pub mod router_decoder;
pub mod sushiswap;
pub mod uniswap_v2;
pub mod uniswap_v3;

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use dashmap::DashMap;

use crate::balancer::BalancerPool;
use crate::bancor::BancorPool;
use crate::curve::CurvePool;
use crate::uniswap_v2::UniswapV2Pool;
use crate::uniswap_v3::UniswapV3Pool;

/// Core Pool trait that all DEX adapters must implement
pub trait Pool: Send + Sync {
    fn protocol(&self) -> ProtocolType;
    fn address(&self) -> Address;
    fn tokens(&self) -> Vec<Address>;
    fn fee_bps(&self) -> u32;
    fn get_amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256>;
    fn get_amount_in(&self, token_out: Address, amount_out: U256) -> Option<U256>;
    fn update_state(&mut self, reserve0: U256, reserve1: U256);
    fn encode_swap(&self, token_in: Address, amount_in: U256, min_out: U256) -> Vec<u8>;
    fn liquidity_depth(&self) -> U256;
}

/// Live state of a single registered pool. Held by [`PoolStateCache`] so
/// the mempool post-state simulator can read accurate per-protocol state
/// (V3 sqrt_price + tick + liquidity, Curve balances + A, Balancer
/// balances + weights) without hitting RPC on every pending tx.
///
/// SushiSwap reuses [`UniswapV2Pool`] under a distinct variant so
/// dispatchers can route to the SushiSwap-specific protocol metadata
/// without a follow-up address lookup.
#[derive(Debug, Clone)]
pub enum PoolState {
    UniswapV2(UniswapV2Pool),
    UniswapV3(UniswapV3Pool),
    SushiSwap(UniswapV2Pool),
    Curve(CurvePool),
    Balancer(BalancerPool),
    Bancor(BancorPool),
}

impl PoolState {
    /// Pool address — convenient for log lines + cache invariants.
    pub fn address(&self) -> Address {
        match self {
            PoolState::UniswapV2(p) | PoolState::SushiSwap(p) => p.address,
            PoolState::UniswapV3(p) => p.address,
            PoolState::Curve(p) => p.address,
            PoolState::Balancer(p) => p.address,
            PoolState::Bancor(p) => p.address,
        }
    }

    /// Protocol family, in workspace `ProtocolType` form.
    pub fn protocol(&self) -> ProtocolType {
        match self {
            PoolState::UniswapV2(_) => ProtocolType::UniswapV2,
            PoolState::UniswapV3(_) => ProtocolType::UniswapV3,
            PoolState::SushiSwap(_) => ProtocolType::SushiSwap,
            PoolState::Curve(_) => ProtocolType::Curve,
            PoolState::Balancer(_) => ProtocolType::BalancerV2,
            PoolState::Bancor(_) => ProtocolType::BancorV3,
        }
    }
}

/// Thread-safe cache of [`PoolState`] values keyed by pool address. Each
/// entry is wrapped in an outer `Arc` so readers (mempool decode pipeline)
/// can clone a snapshot cheaply and run `predict_post_state` against it
/// while a writer (engine event loop) replaces the entry with an updated
/// state for the same address.
///
/// The DashMap shards keys, so a writer updating pool `A` does not block
/// a reader walking pool `B` — important for the hot path where many
/// pending txs are decoded in parallel under a continuous stream of
/// pool-update events.
pub type PoolStateCache = Arc<DashMap<Address, Arc<PoolState>>>;

/// Construct a fresh, empty [`PoolStateCache`]. The engine calls this once
/// at startup and shares the resulting `Arc` with every consumer that
/// wants to observe live pool state (mempool sim, future analytics, etc.).
pub fn new_pool_state_cache() -> PoolStateCache {
    Arc::new(DashMap::new())
}

/// Unified post-swap state across every pool family the analytical
/// predictors handle. Returned by [`predict_post_state_with_fallback`] so
/// the mempool decode pipeline has a single match arm to update its
/// graph-edge cache regardless of protocol.
///
/// Pool-family-specific fields (V3 sqrt_price, Curve / Balancer
/// balances) are kept on their own variants rather than flattened into
/// reserve0 / reserve1 — the graph-edge math reads them differently per
/// protocol and conflating them would lose precision on V3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnifiedPostState {
    UniswapV3(crate::uniswap_v3::V3PostState),
    Curve(crate::curve::CurvePostState),
    Balancer(crate::balancer::BalancerPostState),
    Bancor(crate::bancor::BancorPostState),
}

impl UnifiedPostState {
    /// Output amount the swapper receives, post-fee. Unified across
    /// variants for callers that only care about the user-visible
    /// payout (e.g. the candidate-arb profit estimator).
    pub fn amount_out(&self) -> U256 {
        match self {
            UnifiedPostState::UniswapV3(p) => p.amount_out,
            UnifiedPostState::Curve(p) => p.amount_out,
            UnifiedPostState::Balancer(p) => p.amount_out,
            UnifiedPostState::Bancor(p) => p.amount_out,
        }
    }
}

/// Protocol family the post-state replayer is being asked to handle.
/// Passed by [`predict_post_state_with_replay`] into its replay closure
/// when the analytical predictor's confidence flag is low so the caller
/// can dispatch to the right EVM-fork reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayProtocol {
    UniswapV3,
    Curve,
    Balancer,
    Bancor,
}

/// Run the analytical post-state predictor for the cached pool and
/// escalate to an EVM fork-replay fallback when its confidence flag is
/// low. The caller provides the fallback metric bump as a closure so
/// this function does not need to depend on the engine's metrics crate.
///
/// Returns `Some(UnifiedPostState)` when the analytical answer is
/// trustworthy (`single_tick` / `analytical` flag set). Returns `None`
/// when the predictor itself returned `None` (invalid inputs) OR when
/// the confidence flag was clear and the EVM fallback would be needed
/// — for now the fallback path is a stub that bumps the metric and
/// returns `None`. The replay-aware sibling function
/// [`predict_post_state_with_replay`] does the actual EVM fork-replay
/// when wired by the caller.
///
/// V2 / Sushi pools are NOT handled here — the V2 analytical predictor
/// lives in the mempool decode pipeline (a separate branch) and is
/// trivially exact, so wrapping it through this fallback path adds
/// no value.
pub fn predict_post_state_with_fallback<F>(
    state: &PoolState,
    token_in: alloy::primitives::Address,
    amount_in: U256,
    on_fallback: F,
) -> Option<UnifiedPostState>
where
    F: FnOnce(&str),
{
    match state {
        PoolState::UniswapV3(pool) => {
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.single_tick {
                on_fallback("v3_tick_crossed");
                return None;
            }
            Some(UnifiedPostState::UniswapV3(post))
        }
        PoolState::Curve(pool) => {
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.analytical {
                on_fallback("curve_unconverged");
                return None;
            }
            Some(UnifiedPostState::Curve(post))
        }
        PoolState::Balancer(pool) => {
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.analytical {
                on_fallback("balancer_unequal_weight");
                return None;
            }
            Some(UnifiedPostState::Balancer(post))
        }
        PoolState::Bancor(pool) => {
            // Bancor's bonding curve is closed-form — `analytical` is
            // always `true` on a `Some` return, so the low-confidence
            // branch is dead. Kept for shape symmetry with the other
            // pool families and to keep the metric label space stable
            // if future Bancor pool variants (e.g. V2-style with
            // non-equal weights) ever need to surface the flag.
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.analytical {
                on_fallback("bancor_multihop");
                return None;
            }
            Some(UnifiedPostState::Bancor(post))
        }
        PoolState::UniswapV2(_) | PoolState::SushiSwap(_) => {
            // V2-family analytical predictor lives outside this crate
            // (mempool decode pipeline). Surface the gap so the caller
            // can route V2 through its own path rather than silently
            // returning None and losing the candidate.
            on_fallback("unknown_protocol");
            None
        }
    }
}

/// Replay-aware sibling of [`predict_post_state_with_fallback`]. When
/// the analytical predictor returns a low-confidence flag, invoke the
/// `replay` closure with the protocol family so the caller can dispatch
/// to an EVM fork-replay reader. Returning `None` from the closure
/// preserves the dormant behaviour (skip the candidate); returning
/// `Some` lets the post-state graph update proceed with revm-derived
/// values.
///
/// Inputs that the analytical predictor itself rejects (zero amount,
/// unknown token, uninitialised pool) still short-circuit to `None`
/// without touching the replay closure — there is no useful post-state
/// to reconstruct from a swap the pool itself would not honour.
pub fn predict_post_state_with_replay<F, R>(
    state: &PoolState,
    token_in: alloy::primitives::Address,
    amount_in: U256,
    on_fallback: F,
    replay: R,
) -> Option<UnifiedPostState>
where
    F: FnOnce(&str),
    R: FnOnce(ReplayProtocol) -> Option<UnifiedPostState>,
{
    match state {
        PoolState::UniswapV3(pool) => {
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.single_tick {
                on_fallback("v3_tick_crossed");
                return replay(ReplayProtocol::UniswapV3);
            }
            Some(UnifiedPostState::UniswapV3(post))
        }
        PoolState::Curve(pool) => {
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.analytical {
                on_fallback("curve_unconverged");
                return replay(ReplayProtocol::Curve);
            }
            Some(UnifiedPostState::Curve(post))
        }
        PoolState::Balancer(pool) => {
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.analytical {
                on_fallback("balancer_unequal_weight");
                return replay(ReplayProtocol::Balancer);
            }
            Some(UnifiedPostState::Balancer(post))
        }
        PoolState::Bancor(pool) => {
            let post = pool.predict_post_state(token_in, amount_in)?;
            if !post.analytical {
                on_fallback("bancor_multihop");
                return replay(ReplayProtocol::Bancor);
            }
            Some(UnifiedPostState::Bancor(post))
        }
        PoolState::UniswapV2(_) | PoolState::SushiSwap(_) => {
            on_fallback("unknown_protocol");
            None
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn pool_state_cache_starts_empty() {
        let cache = new_pool_state_cache();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn pool_state_cache_round_trips_v3() {
        let cache = new_pool_state_cache();
        let addr = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let pool = UniswapV3Pool::new(
            addr,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            5,
            10,
        );
        cache.insert(addr, Arc::new(PoolState::UniswapV3(pool)));
        let got = cache.get(&addr).expect("present").clone();
        assert_eq!(got.address(), addr);
        assert_eq!(got.protocol(), ProtocolType::UniswapV3);
    }

    #[test]
    fn predict_with_fallback_v2_routes_to_unknown_protocol() {
        let v2 = UniswapV2Pool::new(
            Address::ZERO,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
        );
        let state = PoolState::UniswapV2(v2);
        let captured = std::cell::RefCell::new(Vec::<String>::new());
        let result = predict_post_state_with_fallback(
            &state,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            U256::from(1u64),
            |reason| captured.borrow_mut().push(reason.to_string()),
        );
        assert!(result.is_none());
        assert_eq!(captured.borrow().as_slice(), &["unknown_protocol".to_string()]);
    }

    #[test]
    fn predict_with_fallback_v3_returns_state_for_single_tick() {
        // Build a V3 pool seated mid-bucket (matches the pattern in
        // uniswap_v3::tests::setup_v3_pool_mid_bucket so a small swap
        // stays single-tick).
        let mut v3 = UniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
            60,
        );
        let two_pow_96_f64: f64 = 79_228_162_514_264_337_593_543_950_336.0;
        let sqrt_norm = 1.0001f64.powi(15);
        let sqrt_x96 = (sqrt_norm * two_pow_96_f64) as u128;
        v3.update_sqrt_price(U256::from(sqrt_x96), 10_000_000_000_000_000u128, 30);

        let state = PoolState::UniswapV3(v3.clone());
        let captured = std::cell::RefCell::new(Vec::<String>::new());
        let result = predict_post_state_with_fallback(
            &state,
            v3.token0,
            U256::from(100_000_000u64),
            |reason| captured.borrow_mut().push(reason.to_string()),
        );
        assert!(result.is_some(), "small mid-bucket swap must stay analytical");
        assert!(captured.borrow().is_empty(), "no fallback expected");
    }

    #[test]
    fn predict_with_fallback_v3_escalates_on_tick_cross() {
        let mut v3 = UniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
            60,
        );
        let two_pow_96_f64: f64 = 79_228_162_514_264_337_593_543_950_336.0;
        let sqrt_norm = 1.0001f64.powi(15);
        let sqrt_x96 = (sqrt_norm * two_pow_96_f64) as u128;
        v3.update_sqrt_price(U256::from(sqrt_x96), 10_000_000_000_000_000u128, 30);

        let state = PoolState::UniswapV3(v3.clone());
        let captured = std::cell::RefCell::new(Vec::<String>::new());
        let result = predict_post_state_with_fallback(
            &state,
            v3.token0,
            U256::from(5_000_000_000_000_000u64), // huge — crosses bucket
            |reason| captured.borrow_mut().push(reason.to_string()),
        );
        assert!(result.is_none());
        assert_eq!(captured.borrow().as_slice(), &["v3_tick_crossed".to_string()]);
    }

    #[test]
    fn predict_with_fallback_balancer_unequal_weight_escalates() {
        // 80/20 weights → analytical=false → escalates with the
        // `balancer_unequal_weight` reason.
        let mut bal = BalancerPool::new(
            Address::ZERO,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            200_000,
            800_000,
            10,
        );
        bal.update_state(
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(10_000_000_000_000u64),
        );
        let state = PoolState::Balancer(bal);
        let captured = std::cell::RefCell::new(Vec::<String>::new());
        let result = predict_post_state_with_fallback(
            &state,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            U256::from(1_000_000_000_000_000_000u64),
            |reason| captured.borrow_mut().push(reason.to_string()),
        );
        assert!(result.is_none());
        assert_eq!(
            captured.borrow().as_slice(),
            &["balancer_unequal_weight".to_string()]
        );
    }

    #[test]
    fn predict_with_fallback_curve_returns_state_for_converged_pool() {
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usdt = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let mut curve = CurvePool::new(Address::ZERO, vec![usdc, usdt], 100, 4);
        curve.balances = vec![
            U256::from(10_000_000_000_000u64),
            U256::from(10_000_000_000_000u64),
        ];
        let state = PoolState::Curve(curve);
        let captured = std::cell::RefCell::new(Vec::<String>::new());
        let result = predict_post_state_with_fallback(
            &state,
            usdc,
            U256::from(1_000_000_000u64),
            |reason| captured.borrow_mut().push(reason.to_string()),
        );
        assert!(result.is_some());
        assert!(captured.borrow().is_empty());
    }

    #[test]
    fn predict_with_fallback_bancor_returns_state_for_single_pool_swap() {
        // Single-pool Bancor swap (token <-> BNT) — predictor returns
        // analytical=true, fallback closure should never fire.
        let token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bnt = address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C");
        let mut bancor = BancorPool::new(Address::ZERO, token, bnt, 30);
        bancor.update_state(
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(2_000_000_000_000_000_000_000u128),
        );
        let state = PoolState::Bancor(bancor);
        let captured = std::cell::RefCell::new(Vec::<String>::new());
        let result = predict_post_state_with_fallback(
            &state,
            token,
            U256::from(1_000_000_000_000_000_000u64),
            |reason| captured.borrow_mut().push(reason.to_string()),
        );
        assert!(matches!(result, Some(UnifiedPostState::Bancor(_))));
        assert!(captured.borrow().is_empty());
    }

    #[test]
    fn predict_with_fallback_bancor_returns_none_for_multihop() {
        // Neither token_in nor token_out is BNT — predictor returns None
        // (single-pool can't predict multi-hop) and the fallback closure
        // does not fire because the rejection happens before the
        // confidence check.
        let token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bnt = address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C");
        let bogus = address!("dddddddddddddddddddddddddddddddddddddddd");
        let mut bancor = BancorPool::new(Address::ZERO, token, bnt, 30);
        bancor.update_state(
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(2_000_000_000_000_000_000_000u128),
        );
        let state = PoolState::Bancor(bancor);
        let captured = std::cell::RefCell::new(Vec::<String>::new());
        let result = predict_post_state_with_fallback(
            &state,
            bogus,
            U256::from(1_000_000_000_000_000_000u64),
            |reason| captured.borrow_mut().push(reason.to_string()),
        );
        assert!(result.is_none());
        assert!(captured.borrow().is_empty(), "no fallback expected on multihop bail");
    }

    #[test]
    fn predict_with_replay_v3_invokes_closure_on_tick_cross() {
        let mut v3 = UniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
            60,
        );
        let two_pow_96_f64: f64 = 79_228_162_514_264_337_593_543_950_336.0;
        let sqrt_norm = 1.0001f64.powi(15);
        let sqrt_x96 = (sqrt_norm * two_pow_96_f64) as u128;
        v3.update_sqrt_price(U256::from(sqrt_x96), 10_000_000_000_000_000u128, 30);

        let state = PoolState::UniswapV3(v3.clone());
        let captured_fallback = std::cell::RefCell::new(Vec::<String>::new());
        let captured_replay = std::cell::RefCell::new(Vec::<ReplayProtocol>::new());
        let result = predict_post_state_with_replay(
            &state,
            v3.token0,
            U256::from(5_000_000_000_000_000u64),
            |reason| captured_fallback.borrow_mut().push(reason.to_string()),
            |proto| {
                captured_replay.borrow_mut().push(proto);
                None
            },
        );
        assert!(result.is_none(), "closure returned None — final result is None");
        assert_eq!(
            captured_fallback.borrow().as_slice(),
            &["v3_tick_crossed".to_string()],
            "fallback metric still bumped"
        );
        assert_eq!(
            captured_replay.borrow().as_slice(),
            &[ReplayProtocol::UniswapV3],
            "replay closure invoked with V3 family"
        );
    }

    #[test]
    fn predict_with_replay_uses_closure_result_when_some() {
        let mut v3 = UniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
            60,
        );
        let two_pow_96_f64: f64 = 79_228_162_514_264_337_593_543_950_336.0;
        let sqrt_norm = 1.0001f64.powi(15);
        let sqrt_x96 = (sqrt_norm * two_pow_96_f64) as u128;
        v3.update_sqrt_price(U256::from(sqrt_x96), 10_000_000_000_000_000u128, 30);

        let state = PoolState::UniswapV3(v3.clone());
        let result = predict_post_state_with_replay(
            &state,
            v3.token0,
            U256::from(5_000_000_000_000_000u64),
            |_reason| {},
            |_proto| {
                Some(UnifiedPostState::UniswapV3(
                    crate::uniswap_v3::V3PostState {
                        new_sqrt_price_x96: U256::from(42u64),
                        new_liquidity: 99,
                        amount_out: U256::ZERO,
                        single_tick: true,
                    },
                ))
            },
        );
        assert!(matches!(
            result,
            Some(UnifiedPostState::UniswapV3(ref p)) if p.new_sqrt_price_x96 == U256::from(42u64)
        ));
    }

    #[test]
    fn predict_with_replay_does_not_call_closure_when_analytical_ok() {
        let mut v3 = UniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
            60,
        );
        let two_pow_96_f64: f64 = 79_228_162_514_264_337_593_543_950_336.0;
        let sqrt_norm = 1.0001f64.powi(15);
        let sqrt_x96 = (sqrt_norm * two_pow_96_f64) as u128;
        v3.update_sqrt_price(U256::from(sqrt_x96), 10_000_000_000_000_000u128, 30);

        let state = PoolState::UniswapV3(v3.clone());
        let called = std::cell::Cell::new(false);
        let result = predict_post_state_with_replay(
            &state,
            v3.token0,
            U256::from(100_000_000u64),
            |_reason| {},
            |_proto| {
                called.set(true);
                None
            },
        );
        assert!(result.is_some(), "analytical predictor succeeded");
        assert!(!called.get(), "replay closure must not run when analytical succeeds");
    }

    #[test]
    fn pool_state_protocol_dispatch_covers_every_variant() {
        let v2 = UniswapV2Pool::new(Address::ZERO, Address::ZERO, Address::ZERO, 30);
        assert_eq!(
            PoolState::UniswapV2(v2.clone()).protocol(),
            ProtocolType::UniswapV2
        );
        assert_eq!(
            PoolState::SushiSwap(v2).protocol(),
            ProtocolType::SushiSwap
        );
        let v3 = UniswapV3Pool::new(Address::ZERO, Address::ZERO, Address::ZERO, 5, 10);
        assert_eq!(
            PoolState::UniswapV3(v3).protocol(),
            ProtocolType::UniswapV3
        );
        let curve = CurvePool::new(
            Address::ZERO,
            vec![Address::ZERO, Address::ZERO],
            100,
            4,
        );
        assert_eq!(PoolState::Curve(curve).protocol(), ProtocolType::Curve);
        let bal = BalancerPool::new(
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
            500_000,
            500_000,
            30,
        );
        assert_eq!(
            PoolState::Balancer(bal).protocol(),
            ProtocolType::BalancerV2
        );
        let bancor = BancorPool::new(
            Address::ZERO,
            Address::ZERO,
            address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C"),
            30,
        );
        assert_eq!(PoolState::Bancor(bancor).protocol(), ProtocolType::BancorV3);
    }
}
