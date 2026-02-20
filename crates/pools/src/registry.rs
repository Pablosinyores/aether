use alloy::primitives::Address;
use aether_common::types::{PoolId, PoolTier};
use crate::Pool;
use std::collections::HashMap;

/// Qualification criteria for pool admission into the registry.
///
/// Pools must meet all thresholds to be considered for monitoring.
/// See `config/pools.toml` for production values.
#[derive(Debug, Clone)]
pub struct QualificationCriteria {
    pub min_liquidity_usd: f64,
    pub min_volume_24h_usd: f64,
    pub min_age_blocks: u64,
    pub max_rug_score: f64,
}

impl Default for QualificationCriteria {
    fn default() -> Self {
        Self {
            min_liquidity_usd: 10_000.0,
            min_volume_24h_usd: 1_000.0,
            min_age_blocks: 100,
            max_rug_score: 0.3,
        }
    }
}

/// Pool registry managing all discovered and qualified pools.
///
/// Maintains:
/// - A map of `PoolId` -> `Pool` trait objects for all registered pools
/// - A pair index mapping `(tokenA, tokenB)` -> `Vec<PoolId>` for fast cross-DEX lookups
/// - Tier assignments (Hot/Warm/Cold) that control monitoring frequency
pub struct PoolRegistry {
    pools: HashMap<PoolId, Box<dyn Pool>>,
    pair_index: HashMap<(Address, Address), Vec<PoolId>>,
    tiers: HashMap<PoolId, PoolTier>,
    criteria: QualificationCriteria,
}

impl PoolRegistry {
    pub fn new(criteria: QualificationCriteria) -> Self {
        Self {
            pools: HashMap::new(),
            pair_index: HashMap::new(),
            tiers: HashMap::new(),
            criteria,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(QualificationCriteria::default())
    }

    /// Register a pool with a given monitoring tier.
    ///
    /// Automatically builds the pair index for all token pair combinations
    /// in the pool (supports multi-token pools like Curve 3pool).
    pub fn register(&mut self, pool: Box<dyn Pool>, tier: PoolTier) {
        let id = PoolId {
            address: pool.address(),
            protocol: pool.protocol(),
        };
        let tokens = pool.tokens();

        // Build pair index for all token pairs
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                self.pair_index
                    .entry((tokens[i], tokens[j]))
                    .or_default()
                    .push(id);
                self.pair_index
                    .entry((tokens[j], tokens[i]))
                    .or_default()
                    .push(id);
            }
        }

        self.tiers.insert(id, tier);
        self.pools.insert(id, pool);
    }

    /// Get an immutable reference to a pool by its ID.
    pub fn get(&self, id: &PoolId) -> Option<&dyn Pool> {
        self.pools.get(id).map(|p| p.as_ref())
    }

    /// Get a mutable reference to a pool by its ID.
    pub fn get_mut(&mut self, id: &PoolId) -> Option<&mut Box<dyn Pool>> {
        self.pools.get_mut(id)
    }

    /// Find all pools that trade a given token pair (in either direction).
    pub fn pools_for_pair(&self, token_a: Address, token_b: Address) -> Vec<&dyn Pool> {
        self.pair_index
            .get(&(token_a, token_b))
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.pools.get(id).map(|p| p.as_ref()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return all registered pool IDs.
    pub fn all_pool_ids(&self) -> Vec<PoolId> {
        self.pools.keys().copied().collect()
    }

    /// Total number of registered pools.
    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    /// Get the monitoring tier for a pool.
    pub fn tier(&self, id: &PoolId) -> Option<PoolTier> {
        self.tiers.get(id).copied()
    }

    /// Return all pool IDs in the Hot tier (monitored every block).
    pub fn hot_pools(&self) -> Vec<PoolId> {
        self.tiers
            .iter()
            .filter(|(_, t)| **t == PoolTier::Hot)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Check whether a pool meets the qualification criteria for admission.
    pub fn qualifies(
        &self,
        liquidity_usd: f64,
        volume_24h_usd: f64,
        age_blocks: u64,
        rug_score: f64,
    ) -> bool {
        liquidity_usd >= self.criteria.min_liquidity_usd
            && volume_24h_usd >= self.criteria.min_volume_24h_usd
            && age_blocks >= self.criteria.min_age_blocks
            && rug_score <= self.criteria.max_rug_score
    }

    /// Remove a pool from the registry, cleaning up the pair index.
    pub fn remove(&mut self, id: &PoolId) -> Option<Box<dyn Pool>> {
        self.tiers.remove(id);
        if let Some(pool) = self.pools.remove(id) {
            let tokens = pool.tokens();
            for i in 0..tokens.len() {
                for j in (i + 1)..tokens.len() {
                    if let Some(ids) = self.pair_index.get_mut(&(tokens[i], tokens[j])) {
                        ids.retain(|pid| pid != id);
                    }
                    if let Some(ids) = self.pair_index.get_mut(&(tokens[j], tokens[i])) {
                        ids.retain(|pid| pid != id);
                    }
                }
            }
            Some(pool)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_common::types::ProtocolType;
    use crate::sushiswap::SushiSwapPool;
    use crate::uniswap_v2::UniswapV2Pool;
    use alloy::primitives::address;

    fn usdc() -> Address {
        address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
    }
    fn weth() -> Address {
        address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
    }

    #[test]
    fn test_register_and_lookup() {
        let mut registry = PoolRegistry::with_defaults();
        let pool = UniswapV2Pool::new(
            address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            usdc(),
            weth(),
            30,
        );
        registry.register(Box::new(pool), PoolTier::Hot);
        assert_eq!(registry.pool_count(), 1);
    }

    #[test]
    fn test_pair_index() {
        let mut registry = PoolRegistry::with_defaults();
        let pool1 = UniswapV2Pool::new(
            address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            usdc(),
            weth(),
            30,
        );
        let pool2 = SushiSwapPool::new(
            address!("397FF1542f962076d0BFE58eA045FfA2d347ACa0"),
            usdc(),
            weth(),
            30,
        );
        registry.register(Box::new(pool1), PoolTier::Hot);
        registry.register(Box::new(pool2), PoolTier::Hot);

        let pools = registry.pools_for_pair(usdc(), weth());
        assert_eq!(pools.len(), 2);
    }

    #[test]
    fn test_qualification() {
        let registry = PoolRegistry::with_defaults();
        assert!(registry.qualifies(50_000.0, 5_000.0, 200, 0.1));
        assert!(!registry.qualifies(5_000.0, 5_000.0, 200, 0.1)); // low liquidity
        assert!(!registry.qualifies(50_000.0, 500.0, 200, 0.1)); // low volume
        assert!(!registry.qualifies(50_000.0, 5_000.0, 50, 0.1)); // too young
        assert!(!registry.qualifies(50_000.0, 5_000.0, 200, 0.5)); // rug risk
    }

    #[test]
    fn test_hot_pools() {
        let mut registry = PoolRegistry::with_defaults();
        let pool = UniswapV2Pool::new(
            address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            usdc(),
            weth(),
            30,
        );
        registry.register(Box::new(pool), PoolTier::Hot);
        assert_eq!(registry.hot_pools().len(), 1);
    }

    #[test]
    fn test_remove_pool() {
        let mut registry = PoolRegistry::with_defaults();
        let addr = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
        let pool = UniswapV2Pool::new(addr, usdc(), weth(), 30);
        let id = PoolId {
            address: addr,
            protocol: ProtocolType::UniswapV2,
        };
        registry.register(Box::new(pool), PoolTier::Hot);
        assert_eq!(registry.pool_count(), 1);
        registry.remove(&id);
        assert_eq!(registry.pool_count(), 0);
    }
}
