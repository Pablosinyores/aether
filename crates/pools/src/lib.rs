pub mod uniswap_v2;
pub mod uniswap_v3;
pub mod sushiswap;
pub mod curve;
pub mod balancer;
pub mod bancor;
pub mod registry;

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use dashmap::DashMap;

use crate::balancer::BalancerPool;
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
}

impl PoolState {
    /// Pool address — convenient for log lines + cache invariants.
    pub fn address(&self) -> Address {
        match self {
            PoolState::UniswapV2(p) | PoolState::SushiSwap(p) => p.address,
            PoolState::UniswapV3(p) => p.address,
            PoolState::Curve(p) => p.address,
            PoolState::Balancer(p) => p.address,
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
    }
}
