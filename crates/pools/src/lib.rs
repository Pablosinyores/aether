pub mod balancer;
pub mod bancor;
pub mod curve;
pub mod registry;
pub mod router_decoder;
pub mod sushiswap;
pub mod uniswap_v2;
pub mod uniswap_v3;

use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;

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
