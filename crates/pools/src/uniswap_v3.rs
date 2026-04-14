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
}
