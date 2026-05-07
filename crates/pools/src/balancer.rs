use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use crate::Pool;

/// Balancer V2 weighted pool
///
/// Implements the weighted constant product formula:
///   prod(B_i ^ W_i) = k
///
/// For swaps between two tokens:
///   amount_out = B_out * (1 - (B_in / (B_in + amount_in))^(W_in / W_out))
///
/// Equal-weight (50/50) pools simplify to the standard constant product formula.
/// For unequal weights, a first-order approximation is used for gas-efficient estimation.
#[derive(Debug, Clone)]
pub struct BalancerPool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub balance0: U256,
    pub balance1: U256,
    pub weight0: U256, // Normalized weight (e.g., 500000 for 50%)
    pub weight1: U256,
    pub fee_bps: u32,
}

/// Snapshot of a Balancer weighted-2-token pool *after* a hypothetical
/// victim swap has been applied. Returned by
/// [`BalancerPool::predict_post_state`] so the mempool post-state simulator
/// can update its graph-edge cache without an RPC round-trip.
///
/// `analytical` is `false` for the unequal-weight branch since
/// `get_amount_out` uses a first-order Taylor approximation there; the
/// caller should escalate those to an EVM fork-replay fallback. The
/// equal-weight branch (50/50, the dominant on-chain shape) is exact and
/// keeps `analytical = true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalancerPostState {
    /// `balance0` after the swap settles.
    pub new_balance0: U256,
    /// `balance1` after the swap settles.
    pub new_balance1: U256,
    /// Output amount the swapper receives, post-fee.
    pub amount_out: U256,
    /// `true` for equal-weight pools where the math is exact;
    /// `false` for the first-order approximation used on unequal weights.
    pub analytical: bool,
}

impl BalancerPool {
    pub fn new(
        address: Address,
        token0: Address,
        token1: Address,
        weight0: u64,
        weight1: u64,
        fee_bps: u32,
    ) -> Self {
        Self {
            address,
            token0,
            token1,
            balance0: U256::ZERO,
            balance1: U256::ZERO,
            weight0: U256::from(weight0),
            weight1: U256::from(weight1),
            fee_bps,
        }
    }

    /// Predict the pool's post-swap state under the same weighted-product
    /// math as [`Pool::get_amount_out`]. Used by the mempool post-state
    /// simulator to feed updated balances into the graph-edge cache
    /// without an RPC round-trip.
    ///
    /// Returns `None` when the inputs are invalid: zero amount, unknown
    /// token, or either balance starts at zero. The fee model matches
    /// Balancer's on-chain `Vault`: the fee portion of `amount_in` stays
    /// in the pool, so `new_balance_in = balance_in + amount_in` (the
    /// full input is credited), `new_balance_out = balance_out - amount_out`.
    ///
    /// `analytical` is `true` only on the equal-weight branch where the
    /// math reduces to the exact constant-product formula; the unequal-
    /// weight branch is a first-order Taylor approximation and signals
    /// `false` so the caller can fall back to an EVM fork-replay.
    pub fn predict_post_state(
        &self,
        token_in: Address,
        amount_in: U256,
    ) -> Option<BalancerPostState> {
        if amount_in.is_zero() {
            return None;
        }
        let amount_out = self.get_amount_out(token_in, amount_in)?;
        let analytical = self.weight0 == self.weight1;

        let (new_balance0, new_balance1) = if token_in == self.token0 {
            (
                self.balance0.checked_add(amount_in)?,
                self.balance1.checked_sub(amount_out)?,
            )
        } else if token_in == self.token1 {
            (
                self.balance0.checked_sub(amount_out)?,
                self.balance1.checked_add(amount_in)?,
            )
        } else {
            return None;
        };

        Some(BalancerPostState {
            new_balance0,
            new_balance1,
            amount_out,
            analytical,
        })
    }
}

impl Pool for BalancerPool {
    fn protocol(&self) -> ProtocolType {
        ProtocolType::BalancerV2
    }
    fn address(&self) -> Address {
        self.address
    }
    fn tokens(&self) -> Vec<Address> {
        vec![self.token0, self.token1]
    }
    fn fee_bps(&self) -> u32 {
        self.fee_bps
    }

    fn get_amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256> {
        if amount_in.is_zero() {
            return None;
        }
        let (bal_in, bal_out, w_in, w_out) = if token_in == self.token0 {
            (self.balance0, self.balance1, self.weight0, self.weight1)
        } else if token_in == self.token1 {
            (self.balance1, self.balance0, self.weight1, self.weight0)
        } else {
            return None;
        };
        if bal_in.is_zero() || bal_out.is_zero() {
            return None;
        }

        // Apply fee to input
        let fee_complement = U256::from(10000 - self.fee_bps);
        let amount_in_after_fee = amount_in * fee_complement / U256::from(10000);

        if w_in == w_out {
            // Equal weights: simplifies to constant product
            // amount_out = bal_out * amount_in_after_fee / (bal_in + amount_in_after_fee)
            let numerator = bal_out * amount_in_after_fee;
            let denominator = bal_in + amount_in_after_fee;
            Some(numerator / denominator)
        } else {
            // Weighted formula approximation using first-order Taylor expansion
            // amount_out ~ bal_out * (amount_in_after_fee * w_in) / (bal_in * w_out + amount_in_after_fee * w_in)
            let numerator = bal_out * amount_in_after_fee * w_in;
            let denominator = bal_in * w_out + amount_in_after_fee * w_in;
            if denominator.is_zero() {
                return None;
            }
            Some(numerator / denominator)
        }
    }

    fn get_amount_in(&self, token_out: Address, amount_out: U256) -> Option<U256> {
        if amount_out.is_zero() {
            return None;
        }
        let (bal_in, bal_out, w_in, w_out) = if token_out == self.token1 {
            (self.balance0, self.balance1, self.weight0, self.weight1)
        } else if token_out == self.token0 {
            (self.balance1, self.balance0, self.weight1, self.weight0)
        } else {
            return None;
        };
        if bal_in.is_zero() || bal_out.is_zero() || amount_out >= bal_out {
            return None;
        }

        if w_in == w_out {
            // Equal weights: constant product inverse
            let numerator = bal_in * amount_out;
            let denominator = bal_out - amount_out;
            let amount_in_before_fee = numerator / denominator + U256::from(1);
            Some(amount_in_before_fee * U256::from(10000) / U256::from(10000 - self.fee_bps))
        } else {
            // Weighted inverse approximation
            let numerator = bal_in * amount_out * w_out;
            let denominator = (bal_out - amount_out) * w_in;
            if denominator.is_zero() {
                return None;
            }
            let amount_in_before_fee = numerator / denominator + U256::from(1);
            Some(amount_in_before_fee * U256::from(10000) / U256::from(10000 - self.fee_bps))
        }
    }

    fn update_state(&mut self, reserve0: U256, reserve1: U256) {
        self.balance0 = reserve0;
        self.balance1 = reserve1;
    }

    fn encode_swap(&self, _token_in: Address, _amount_in: U256, _min_out: U256) -> Vec<u8> {
        Vec::new() // Placeholder - real encoding in calldata builder
    }

    fn liquidity_depth(&self) -> U256 {
        std::cmp::min(self.balance0, self.balance1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn setup_balancer_pool() -> BalancerPool {
        let mut pool = BalancerPool::new(
            Address::ZERO,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // USDC
            500000,
            500000, // 50/50 weights
            30,     // 0.3% fee
        );
        pool.update_state(
            U256::from(5_000_000_000_000_000_000_000u128), // 5000 ETH
            U256::from(10_000_000_000_000u64),             // 10M USDC
        );
        pool
    }

    #[test]
    fn test_balancer_equal_weight() {
        let pool = setup_balancer_pool();
        let eth_in = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        let out = pool.get_amount_out(pool.token0, eth_in).unwrap();
        assert!(!out.is_zero());
    }

    #[test]
    fn test_balancer_protocol() {
        let pool = setup_balancer_pool();
        assert_eq!(pool.protocol(), ProtocolType::BalancerV2);
    }

    fn setup_balancer_80_20_pool() -> BalancerPool {
        let mut pool = BalancerPool::new(
            Address::ZERO,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // USDC
            200000, 800000, // 20/80 weights — unequal, exercises approx path
            10,             // 0.1%
        );
        pool.update_state(
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(10_000_000_000_000u64),
        );
        pool
    }

    // ----- predict_post_state -----

    #[test]
    fn predict_post_state_none_for_zero_amount() {
        let pool = setup_balancer_pool();
        assert!(pool.predict_post_state(pool.token0, U256::ZERO).is_none());
    }

    #[test]
    fn predict_post_state_none_for_unknown_token() {
        let pool = setup_balancer_pool();
        let bogus = address!("0000000000000000000000000000000000001234");
        assert!(pool.predict_post_state(bogus, U256::from(1u64)).is_none());
    }

    #[test]
    fn predict_post_state_none_for_uninitialised_pool() {
        let pool = BalancerPool::new(
            Address::ZERO,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            500000, 500000, 30,
        );
        assert!(pool
            .predict_post_state(pool.token0, U256::from(1u64))
            .is_none());
    }

    #[test]
    fn predict_post_state_balances_shift_correctly_equal_weight() {
        let pool = setup_balancer_pool();
        let dx = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        let post = pool
            .predict_post_state(pool.token0, dx)
            .expect("post state");
        assert_eq!(post.new_balance0, pool.balance0 + dx);
        assert_eq!(post.new_balance1, pool.balance1 - post.amount_out);
        assert!(post.analytical, "50/50 pool is exact, analytical=true");
    }

    #[test]
    fn predict_post_state_unequal_weight_signals_approximation() {
        let pool = setup_balancer_80_20_pool();
        let dx = U256::from(1_000_000_000_000_000_000u64);
        let post = pool
            .predict_post_state(pool.token0, dx)
            .expect("post state");
        assert!(
            !post.analytical,
            "20/80 pool uses first-order approximation, analytical=false"
        );
    }

    #[test]
    fn predict_post_state_amount_out_matches_get_amount_out() {
        let pool = setup_balancer_pool();
        for amt in [
            U256::from(1_000_000_000u64),
            U256::from(1_000_000_000_000_000_000u64),
            U256::from(10_000_000_000_000_000_000u64),
        ] {
            let legacy = pool
                .get_amount_out(pool.token0, amt)
                .expect("legacy amount_out");
            let post = pool
                .predict_post_state(pool.token0, amt)
                .expect("post state");
            assert_eq!(
                post.amount_out, legacy,
                "predict_post_state diverged from get_amount_out at amt={amt}"
            );
        }
    }

    #[test]
    fn predict_post_state_reverse_direction() {
        let pool = setup_balancer_pool();
        let dx = U256::from(1_000_000_000u64);
        let post = pool
            .predict_post_state(pool.token1, dx)
            .expect("post state");
        assert_eq!(post.new_balance0, pool.balance0 - post.amount_out);
        assert_eq!(post.new_balance1, pool.balance1 + dx);
    }
}
