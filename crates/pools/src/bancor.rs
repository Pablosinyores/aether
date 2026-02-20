use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use crate::Pool;

/// Bancor V3 pool with BNT intermediary
///
/// Uses a bonding curve where the price is determined by the reserve ratio:
///   amount_out = bal_out * amount_in / (bal_in + amount_in)
///
/// This is equivalent to a constant product formula with equal weights,
/// applied between the token and BNT (Bancor Network Token).
#[derive(Debug, Clone)]
pub struct BancorPool {
    pub address: Address,
    pub token: Address,
    pub bnt: Address,
    pub token_balance: U256,
    pub bnt_balance: U256,
    pub fee_bps: u32,
}

impl BancorPool {
    pub fn new(address: Address, token: Address, bnt: Address, fee_bps: u32) -> Self {
        Self {
            address,
            token,
            bnt,
            token_balance: U256::ZERO,
            bnt_balance: U256::ZERO,
            fee_bps,
        }
    }
}

impl Pool for BancorPool {
    fn protocol(&self) -> ProtocolType {
        ProtocolType::BancorV3
    }
    fn address(&self) -> Address {
        self.address
    }
    fn tokens(&self) -> Vec<Address> {
        vec![self.token, self.bnt]
    }
    fn fee_bps(&self) -> u32 {
        self.fee_bps
    }

    fn get_amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256> {
        if amount_in.is_zero() {
            return None;
        }
        let (bal_in, bal_out) = if token_in == self.token {
            (self.token_balance, self.bnt_balance)
        } else if token_in == self.bnt {
            (self.bnt_balance, self.token_balance)
        } else {
            return None;
        };
        if bal_in.is_zero() || bal_out.is_zero() {
            return None;
        }

        // Bancor formula with fee applied to input:
        // amount_out = bal_out * amount_in_after_fee / (bal_in + amount_in_after_fee)
        let fee_complement = U256::from(10000 - self.fee_bps);
        let amount_in_after_fee = amount_in * fee_complement / U256::from(10000);
        let numerator = bal_out * amount_in_after_fee;
        let denominator = bal_in + amount_in_after_fee;
        Some(numerator / denominator)
    }

    fn get_amount_in(&self, token_out: Address, amount_out: U256) -> Option<U256> {
        if amount_out.is_zero() {
            return None;
        }
        let (bal_in, bal_out) = if token_out == self.bnt {
            (self.token_balance, self.bnt_balance)
        } else if token_out == self.token {
            (self.bnt_balance, self.token_balance)
        } else {
            return None;
        };
        if bal_in.is_zero() || bal_out.is_zero() || amount_out >= bal_out {
            return None;
        }

        // Inverse formula: amount_in_before_fee = bal_in * amount_out / (bal_out - amount_out) + 1
        // Then undo the fee: amount_in = amount_in_before_fee * 10000 / (10000 - fee_bps)
        let numerator = bal_in * amount_out;
        let denominator = bal_out - amount_out;
        let amount_before_fee = numerator / denominator + U256::from(1);
        Some(amount_before_fee * U256::from(10000) / U256::from(10000 - self.fee_bps))
    }

    fn update_state(&mut self, reserve0: U256, reserve1: U256) {
        self.token_balance = reserve0;
        self.bnt_balance = reserve1;
    }

    fn encode_swap(&self, _token_in: Address, _amount_in: U256, _min_out: U256) -> Vec<u8> {
        Vec::new() // Placeholder - real encoding in calldata builder
    }

    fn liquidity_depth(&self) -> U256 {
        std::cmp::min(self.token_balance, self.bnt_balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn test_bancor_swap() {
        let mut pool = BancorPool::new(
            Address::ZERO,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C"), // BNT
            30,
        );
        pool.update_state(
            U256::from(1_000_000_000_000_000_000_000u128), // 1000 ETH
            U256::from(2_000_000_000_000_000_000_000u128), // 2000 BNT
        );
        let out = pool
            .get_amount_out(pool.token, U256::from(1_000_000_000_000_000_000u64))
            .unwrap();
        assert!(!out.is_zero());
    }

    #[test]
    fn test_bancor_protocol() {
        let pool = BancorPool::new(
            Address::ZERO,
            Address::ZERO,
            address!("0000000000000000000000000000000000000001"),
            30,
        );
        assert_eq!(pool.protocol(), ProtocolType::BancorV3);
    }
}
