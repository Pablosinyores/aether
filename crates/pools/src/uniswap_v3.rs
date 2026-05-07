use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use crate::Pool;

/// Tick data for concentrated liquidity
#[derive(Debug, Clone)]
pub struct TickInfo {
    pub index: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
}

/// Uniswap V3 concentrated liquidity pool
#[derive(Debug, Clone)]
pub struct UniswapV3Pool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee_bps: u32,
    pub tick_spacing: i32,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub ticks: Vec<TickInfo>,
}

// Constants for Q96 fixed-point math
const Q96: u128 = 1u128 << 96;

/// Snapshot of a UniswapV3 pool *after* a hypothetical victim swap has been
/// applied. Returned by [`UniswapV3Pool::predict_post_state`] so the mempool
/// post-state simulator can update its graph-edge cache without re-reading
/// chain state.
///
/// `single_tick` is `true` when the analytical math is trustworthy at full
/// numerical precision because the swap stayed within one tick range.
/// Callers MUST escalate to an EVM fork-replay fallback when it is `false`,
/// since the post-state values are then a low-precision estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V3PostState {
    /// New `sqrt(price) * 2^96` after the swap, in the same Q96 units the
    /// pool already uses.
    pub new_sqrt_price_x96: U256,
    /// Liquidity at the new active tick. For single-tick swaps this is
    /// unchanged from the pre-state liquidity. Cross-tick swaps (the
    /// `single_tick = false` path) cannot be settled analytically and the
    /// returned value is a best-effort estimate; the caller is expected to
    /// run an EVM fallback before trusting it.
    pub new_liquidity: u128,
    /// Output amount the swapper receives, after pool fee, in token-out
    /// raw units (no decimals normalisation).
    pub amount_out: U256,
    /// Confidence flag — see struct docs.
    pub single_tick: bool,
}

impl UniswapV3Pool {
    pub fn new(
        address: Address,
        token0: Address,
        token1: Address,
        fee_bps: u32,
        tick_spacing: i32,
    ) -> Self {
        Self {
            address,
            token0,
            token1,
            fee_bps,
            tick_spacing,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            ticks: Vec::new(),
        }
    }

    pub fn update_sqrt_price(&mut self, sqrt_price_x96: U256, liquidity: u128, tick: i32) {
        self.sqrt_price_x96 = sqrt_price_x96;
        self.liquidity = liquidity;
        self.tick = tick;
    }

    pub fn set_ticks(&mut self, mut ticks: Vec<TickInfo>) {
        ticks.sort_by_key(|t| t.index);
        self.ticks = ticks;
    }

    /// Predict the pool's post-swap state under the same single-tick
    /// constant-liquidity assumption as [`Self::compute_swap_within_tick`].
    /// Mempool post-state simulation uses this to update its graph-edge
    /// cache after a decoded victim swap, without round-tripping to RPC.
    ///
    /// Returns `None` when the inputs cannot produce a valid post-state:
    ///   - pool has no liquidity / sqrt_price
    ///   - `amount_in` is zero
    ///   - `token_in` is not one of the pool's two tokens
    ///   - the math underflows / divides by zero (defensive — should not
    ///     happen on real pool state)
    ///
    /// `single_tick` on the returned state is hard-coded to `true` here.
    /// The cross-tick branch lives in a follow-up commit and flips this
    /// flag to signal an EVM fork-replay fallback to the caller.
    pub fn predict_post_state(
        &self,
        token_in: Address,
        amount_in: U256,
    ) -> Option<V3PostState> {
        if self.sqrt_price_x96.is_zero() || self.liquidity == 0 || amount_in.is_zero() {
            return None;
        }
        if token_in != self.token0 && token_in != self.token1 {
            return None;
        }

        // Apply the pool fee. `fee_bps` here is in basis points (1e-4)
        // matching the convention used by `compute_swap_within_tick`
        // above; the bootstrap translation from on-chain
        // hundredths-of-a-bp lives outside this module. So 0.05% pool
        // → fee_bps=5, fee_complement=9995, dx_eff = dx * 0.9995.
        let fee_complement = 10_000u64 - self.fee_bps as u64;
        let amount_in_after_fee =
            amount_in * U256::from(fee_complement) / U256::from(10_000u64);

        let is_token0 = token_in == self.token0;
        let q96 = U256::from(Q96);
        let l = U256::from(self.liquidity);

        if is_token0 {
            // token0 → token1: sqrt_price decreases.
            //   new_sqrt = (L * sqrt * Q96) / (L * Q96 + dx_eff * sqrt)
            //   dy       = L * (sqrt - new_sqrt) / Q96
            let numerator = l * self.sqrt_price_x96;
            let denominator = l * q96 + amount_in_after_fee * self.sqrt_price_x96;
            if denominator.is_zero() {
                return None;
            }
            let new_sqrt_price_x96 = numerator * q96 / denominator;
            if new_sqrt_price_x96 >= self.sqrt_price_x96 {
                // Numerical degenerate case (e.g. amount_in_after_fee
                // rounds to zero against a huge liquidity). Bail rather
                // than emit a zero-output edge.
                return None;
            }
            let delta = self.sqrt_price_x96 - new_sqrt_price_x96;
            let amount_out = l * delta / q96;
            Some(V3PostState {
                new_sqrt_price_x96,
                new_liquidity: self.liquidity,
                amount_out,
                single_tick: true,
            })
        } else {
            // token1 → token0: sqrt_price increases.
            //   new_sqrt = sqrt + dy_eff * Q96 / L
            //   dx       = L * Q96 * (new_sqrt - sqrt) / (sqrt * new_sqrt)
            if l.is_zero() {
                return None;
            }
            let delta_sqrt = amount_in_after_fee * q96 / l;
            let new_sqrt_price_x96 = self.sqrt_price_x96 + delta_sqrt;
            let numerator = l * q96 * delta_sqrt;
            let denominator = self.sqrt_price_x96 * new_sqrt_price_x96;
            if denominator.is_zero() {
                return None;
            }
            let amount_out = numerator / denominator;
            Some(V3PostState {
                new_sqrt_price_x96,
                new_liquidity: self.liquidity,
                amount_out,
                single_tick: true,
            })
        }
    }

    /// Simplified single-tick swap (no tick crossing) for quick estimation.
    /// Full tick-traversal swap is used in revm simulation for exact results.
    fn compute_swap_within_tick(&self, token_in: Address, amount_in: U256) -> Option<U256> {
        if self.sqrt_price_x96.is_zero() || self.liquidity == 0 || amount_in.is_zero() {
            return None;
        }

        // Apply fee
        let fee_complement = 10000u64 - self.fee_bps as u64;
        let amount_in_after_fee =
            amount_in * U256::from(fee_complement) / U256::from(10000u64);

        let is_token0 = token_in == self.token0;
        let q96 = U256::from(Q96);

        if is_token0 {
            // token0 -> token1: price decreases
            // new_sqrt_price = L * sqrt_p / (L + dx * sqrt_p / Q96)
            // Rearranged to avoid precision loss:
            // new_sqrt_price = (L * sqrt_p * Q96) / (L * Q96 + dx * sqrt_p)
            let l = U256::from(self.liquidity);
            let numerator = l * self.sqrt_price_x96;
            let denominator = l * q96 + amount_in_after_fee * self.sqrt_price_x96;
            if denominator.is_zero() {
                return None;
            }
            let new_sqrt_price = numerator * q96 / denominator;

            // dy = L * (sqrt_p - new_sqrt_p) / Q96
            if self.sqrt_price_x96 <= new_sqrt_price {
                return Some(U256::ZERO);
            }
            let delta = self.sqrt_price_x96 - new_sqrt_price;
            Some(l * delta / q96)
        } else {
            // token1 -> token0: price increases
            // new_sqrt_price = sqrt_p + dy * Q96 / L
            let l = U256::from(self.liquidity);
            if l.is_zero() {
                return None;
            }
            let delta_sqrt = amount_in_after_fee * q96 / l;
            let new_sqrt_price = self.sqrt_price_x96 + delta_sqrt;

            // dx = L * Q96 * (new_sqrt_p - sqrt_p) / (sqrt_p * new_sqrt_p)
            let numerator = l * q96 * delta_sqrt;
            let denominator = self.sqrt_price_x96 * new_sqrt_price;
            if denominator.is_zero() {
                return None;
            }
            Some(numerator / denominator)
        }
    }
}

impl Pool for UniswapV3Pool {
    fn protocol(&self) -> ProtocolType {
        ProtocolType::UniswapV3
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
        if token_in != self.token0 && token_in != self.token1 {
            return None;
        }
        self.compute_swap_within_tick(token_in, amount_in)
    }

    fn get_amount_in(&self, token_out: Address, amount_out: U256) -> Option<U256> {
        if amount_out.is_zero() {
            return None;
        }
        if token_out != self.token0 && token_out != self.token1 {
            return None;
        }
        // Binary search for the required input amount
        let token_in = if token_out == self.token0 {
            self.token1
        } else {
            self.token0
        };
        let mut low = U256::from(1u64);
        let mut high = amount_out * U256::from(2u64); // Upper bound estimate
        for _ in 0..256 {
            if low >= high {
                break;
            }
            let mid = (low + high) / U256::from(2u64);
            match self.get_amount_out(token_in, mid) {
                Some(out) if out >= amount_out => high = mid,
                _ => low = mid + U256::from(1u64),
            }
        }
        Some(high)
    }

    fn update_state(&mut self, _reserve0: U256, _reserve1: U256) {
        // V3 doesn't use simple reserves; state is updated via update_sqrt_price()
    }

    fn encode_swap(&self, _token_in: Address, _amount_in: U256, _min_out: U256) -> Vec<u8> {
        Vec::new() // Placeholder - real encoding in calldata builder
    }

    fn liquidity_depth(&self) -> U256 {
        U256::from(self.liquidity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn setup_v3_pool() -> UniswapV3Pool {
        let mut pool = UniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // USDC
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
            5, // 0.05%
            10,
        );
        // sqrt(2000) * 2^96 ~ 3.543 * 10^30
        // 2000 USDC/ETH, but USDC is token0 (6 dec), WETH is token1 (18 dec)
        // price = token1/token0 = 1/2000 * 10^12 = 5 * 10^8
        // sqrt(5 * 10^8) ~ 22360.68
        // sqrt_price_x96 = 22360 * 2^96
        let sqrt_price = U256::from(22360u64) * U256::from(Q96);
        pool.update_sqrt_price(sqrt_price, 10_000_000_000_000u128, 0);
        pool
    }

    #[test]
    fn test_v3_get_amount_out_token0() {
        let pool = setup_v3_pool();
        let usdc_in = U256::from(1_000_000_000u64); // 1000 USDC
        let result = pool.get_amount_out(pool.token0, usdc_in);
        assert!(result.is_some());
        assert!(!result.unwrap().is_zero());
    }

    #[test]
    fn test_v3_get_amount_out_token1() {
        let pool = setup_v3_pool();
        let eth_in = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        let result = pool.get_amount_out(pool.token1, eth_in);
        assert!(result.is_some());
        assert!(!result.unwrap().is_zero());
    }

    #[test]
    fn test_v3_zero_liquidity() {
        let pool = UniswapV3Pool::new(
            Address::ZERO,
            Address::ZERO,
            address!("0000000000000000000000000000000000000001"),
            5,
            10,
        );
        assert!(pool.get_amount_out(Address::ZERO, U256::from(1000u64)).is_none());
    }

    #[test]
    fn test_v3_protocol() {
        let pool = UniswapV3Pool::new(
            Address::ZERO,
            Address::ZERO,
            address!("0000000000000000000000000000000000000001"),
            5,
            10,
        );
        assert_eq!(pool.protocol(), ProtocolType::UniswapV3);
    }

    // ----- predict_post_state -----

    #[test]
    fn predict_post_state_none_for_zero_amount() {
        let pool = setup_v3_pool();
        assert!(pool.predict_post_state(pool.token0, U256::ZERO).is_none());
    }

    #[test]
    fn predict_post_state_none_for_unknown_token() {
        let pool = setup_v3_pool();
        let bogus = address!("0000000000000000000000000000000000001234");
        assert!(pool
            .predict_post_state(bogus, U256::from(1_000_000_000u64))
            .is_none());
    }

    #[test]
    fn predict_post_state_none_for_uninitialised_pool() {
        let pool = UniswapV3Pool::new(
            Address::ZERO,
            Address::ZERO,
            address!("0000000000000000000000000000000000000001"),
            5,
            10,
        );
        // sqrt_price + liquidity both default to zero — nothing to predict.
        assert!(pool
            .predict_post_state(Address::ZERO, U256::from(1u64))
            .is_none());
    }

    #[test]
    fn predict_post_state_token0_to_token1_lowers_sqrt_price() {
        let pool = setup_v3_pool();
        let dx = U256::from(1_000_000_000u64); // 1000 USDC (6dp)
        let post = pool
            .predict_post_state(pool.token0, dx)
            .expect("post state");
        assert!(
            post.new_sqrt_price_x96 < pool.sqrt_price_x96,
            "token0→token1 must lower sqrt_price (got new={}, old={})",
            post.new_sqrt_price_x96,
            pool.sqrt_price_x96
        );
        assert!(!post.amount_out.is_zero(), "non-zero output expected");
        assert_eq!(post.new_liquidity, pool.liquidity);
        assert!(post.single_tick);
    }

    #[test]
    fn predict_post_state_token1_to_token0_raises_sqrt_price() {
        let pool = setup_v3_pool();
        let dy = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        let post = pool
            .predict_post_state(pool.token1, dy)
            .expect("post state");
        assert!(
            post.new_sqrt_price_x96 > pool.sqrt_price_x96,
            "token1→token0 must raise sqrt_price"
        );
        assert!(!post.amount_out.is_zero());
        assert_eq!(post.new_liquidity, pool.liquidity);
        assert!(post.single_tick);
    }

    #[test]
    fn predict_post_state_amount_out_matches_compute_swap_within_tick() {
        // The post-state predictor and the legacy `compute_swap_within_tick`
        // share the same single-tick math; they MUST return the same
        // `amount_out` for the same input. If they ever diverge, one of
        // the two paths is wrong and the mempool sim drifts from the
        // detector's pricing.
        let pool = setup_v3_pool();
        for amt in [
            U256::from(1_000_000u64),
            U256::from(1_000_000_000u64),
            U256::from(1_000_000_000_000u64),
        ] {
            let legacy = pool
                .get_amount_out(pool.token0, amt)
                .expect("legacy amount_out");
            let post = pool
                .predict_post_state(pool.token0, amt)
                .expect("post state");
            assert_eq!(
                post.amount_out, legacy,
                "predict_post_state amount_out diverged from compute_swap_within_tick at amt={amt}"
            );
        }
    }
}
