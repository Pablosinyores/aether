use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use crate::Pool;

#[derive(Debug, Clone)]
pub struct UniswapV2Pool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    pub fee_bps: u32, // typically 30 (0.3%)
}

impl UniswapV2Pool {
    pub fn new(address: Address, token0: Address, token1: Address, fee_bps: u32) -> Self {
        Self {
            address,
            token0,
            token1,
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            fee_bps,
        }
    }
}

impl Pool for UniswapV2Pool {
    fn protocol(&self) -> ProtocolType { ProtocolType::UniswapV2 }
    fn address(&self) -> Address { self.address }
    fn tokens(&self) -> Vec<Address> { vec![self.token0, self.token1] }
    fn fee_bps(&self) -> u32 { self.fee_bps }

    fn get_amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256> {
        if amount_in.is_zero() { return None; }
        let (reserve_in, reserve_out) = if token_in == self.token0 {
            (self.reserve0, self.reserve1)
        } else if token_in == self.token1 {
            (self.reserve1, self.reserve0)
        } else {
            return None;
        };
        if reserve_in.is_zero() || reserve_out.is_zero() { return None; }

        // Exact Solidity formula from UniswapV2Pair.sol:
        // dy = (dx * 997 * y) / (x * 1000 + dx * 997)
        let amount_in_with_fee = amount_in * U256::from(997);
        let numerator = amount_in_with_fee * reserve_out;
        let denominator = reserve_in * U256::from(1000) + amount_in_with_fee;
        Some(numerator / denominator)
    }

    fn get_amount_in(&self, token_out: Address, amount_out: U256) -> Option<U256> {
        if amount_out.is_zero() { return None; }
        let (reserve_in, reserve_out) = if token_out == self.token1 {
            (self.reserve0, self.reserve1)
        } else if token_out == self.token0 {
            (self.reserve1, self.reserve0)
        } else {
            return None;
        };
        if reserve_in.is_zero() || reserve_out.is_zero() { return None; }
        if amount_out >= reserve_out { return None; }

        // Exact Solidity formula from UniswapV2Library.sol:
        // dx = (x * dy * 1000) / ((y - dy) * 997) + 1
        let numerator = reserve_in * amount_out * U256::from(1000);
        let denominator = (reserve_out - amount_out) * U256::from(997);
        Some(numerator / denominator + U256::from(1))
    }

    fn update_state(&mut self, reserve0: U256, reserve1: U256) {
        self.reserve0 = reserve0;
        self.reserve1 = reserve1;
    }

    fn encode_swap(&self, token_in: Address, _amount_in: U256, min_out: U256) -> Vec<u8> {
        // Encode swap(uint amount0Out, uint amount1Out, address to, bytes data)
        let (_amount0_out, _amount1_out) = if token_in == self.token0 {
            (U256::ZERO, min_out)
        } else {
            (min_out, U256::ZERO)
        };
        Vec::new() // Placeholder - real encoding done in calldata builder
    }

    fn liquidity_depth(&self) -> U256 {
        // Geometric mean of reserves as liquidity proxy
        // Simplified: use min(r0, r1) as depth indicator
        std::cmp::min(self.reserve0, self.reserve1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn setup_pool() -> UniswapV2Pool {
        let mut pool = UniswapV2Pool::new(
            address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // USDC
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
            30,
        );
        // Set realistic reserves: 10M USDC, 5000 ETH
        pool.update_state(
            U256::from(10_000_000_000_000u64),  // 10M USDC (6 decimals)
            U256::from(5_000_000_000_000_000_000_000u128), // 5000 ETH (18 decimals)
        );
        pool
    }

    #[test]
    fn test_get_amount_out() {
        let pool = setup_pool();
        // Swap 1 ETH for USDC
        let eth_amount = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        let usdc_out = pool.get_amount_out(pool.token1, eth_amount).unwrap();
        // Should get roughly 2000 USDC (at 2000 USDC/ETH rate)
        assert!(usdc_out > U256::from(1_990_000_000u64)); // > 1990 USDC
        assert!(usdc_out < U256::from(2_000_000_000u64)); // < 2000 USDC (fee + slippage)
    }

    #[test]
    fn test_get_amount_in() {
        let pool = setup_pool();
        // How much ETH to get 1000 USDC
        let usdc_amount = U256::from(1_000_000_000u64); // 1000 USDC
        let eth_in = pool.get_amount_in(pool.token0, usdc_amount).unwrap();
        // Should need roughly 0.5 ETH
        assert!(eth_in > U256::from(499_000_000_000_000_000u64)); // > 0.499 ETH
        assert!(eth_in < U256::from(502_000_000_000_000_000u64)); // < 0.502 ETH
    }

    #[test]
    fn test_zero_amount_returns_none() {
        let pool = setup_pool();
        assert!(pool.get_amount_out(pool.token0, U256::ZERO).is_none());
        assert!(pool.get_amount_in(pool.token0, U256::ZERO).is_none());
    }

    #[test]
    fn test_invalid_token_returns_none() {
        let pool = setup_pool();
        let random = address!("0000000000000000000000000000000000000001");
        assert!(pool.get_amount_out(random, U256::from(1000u64)).is_none());
    }

    #[test]
    fn test_empty_reserves_returns_none() {
        let pool = UniswapV2Pool::new(
            Address::ZERO, Address::ZERO, address!("0000000000000000000000000000000000000001"), 30,
        );
        assert!(pool.get_amount_out(Address::ZERO, U256::from(1000u64)).is_none());
    }

    #[test]
    fn test_amount_out_exceeds_reserves() {
        let pool = setup_pool();
        let eth_in = pool.get_amount_in(pool.token0, pool.reserve0 + U256::from(1));
        assert!(eth_in.is_none());
    }
}
