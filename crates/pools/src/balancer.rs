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
}
