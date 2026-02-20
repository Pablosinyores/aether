use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use crate::Pool;
use crate::uniswap_v2::UniswapV2Pool;

/// SushiSwap uses the same constant product formula as Uniswap V2
#[derive(Debug, Clone)]
pub struct SushiSwapPool {
    inner: UniswapV2Pool,
}

impl SushiSwapPool {
    pub fn new(address: Address, token0: Address, token1: Address, fee_bps: u32) -> Self {
        Self {
            inner: UniswapV2Pool::new(address, token0, token1, fee_bps),
        }
    }
}

impl Pool for SushiSwapPool {
    fn protocol(&self) -> ProtocolType { ProtocolType::SushiSwap }
    fn address(&self) -> Address { self.inner.address() }
    fn tokens(&self) -> Vec<Address> { self.inner.tokens() }
    fn fee_bps(&self) -> u32 { self.inner.fee_bps() }
    fn get_amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256> { self.inner.get_amount_out(token_in, amount_in) }
    fn get_amount_in(&self, token_out: Address, amount_out: U256) -> Option<U256> { self.inner.get_amount_in(token_out, amount_out) }
    fn update_state(&mut self, reserve0: U256, reserve1: U256) { self.inner.update_state(reserve0, reserve1) }
    fn encode_swap(&self, token_in: Address, amount_in: U256, min_out: U256) -> Vec<u8> { self.inner.encode_swap(token_in, amount_in, min_out) }
    fn liquidity_depth(&self) -> U256 { self.inner.liquidity_depth() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn test_sushi_same_as_univ2() {
        let mut sushi = SushiSwapPool::new(
            Address::ZERO,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
        );
        sushi.update_state(
            U256::from(10_000_000_000_000u64),
            U256::from(5_000_000_000_000_000_000_000u128),
        );

        let mut univ2 = UniswapV2Pool::new(
            Address::ZERO,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
        );
        univ2.update_state(
            U256::from(10_000_000_000_000u64),
            U256::from(5_000_000_000_000_000_000_000u128),
        );

        let amount = U256::from(1_000_000_000_000_000_000u64);
        let token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        assert_eq!(
            sushi.get_amount_out(token, amount),
            univ2.get_amount_out(token, amount),
        );
    }

    #[test]
    fn test_sushi_protocol() {
        let pool = SushiSwapPool::new(
            Address::ZERO,
            Address::ZERO,
            address!("0000000000000000000000000000000000000001"),
            30,
        );
        assert_eq!(pool.protocol(), ProtocolType::SushiSwap);
    }
}
