use alloy::primitives::{Address, U256};
use aether_common::types::ProtocolType;
use crate::Pool;

/// Curve StableSwap pool (2-token variant)
///
/// Implements the StableSwap invariant:
///   A * n^n * sum(x_i) + D = A * n^n * D + D^(n+1) / (n^n * prod(x_i))
///
/// Newton's method is used to solve for D and y, exactly matching the
/// on-chain Solidity implementation in Curve's StableSwap contracts.
#[derive(Debug, Clone)]
pub struct CurvePool {
    pub address: Address,
    pub tokens: Vec<Address>,
    pub balances: Vec<U256>,
    pub amplification: U256, // A coefficient
    pub fee_bps: u32,        // typically 4 (0.04%)
}

impl CurvePool {
    pub fn new(address: Address, tokens: Vec<Address>, amplification: u64, fee_bps: u32) -> Self {
        let n = tokens.len();
        Self {
            address,
            tokens,
            balances: vec![U256::ZERO; n],
            amplification: U256::from(amplification),
            fee_bps,
        }
    }

    /// Compute StableSwap invariant D using Newton's method.
    ///
    /// The invariant is: A * n^n * S + D = A * n^n * D + D^(n+1) / (n^n * prod(x_i))
    /// where S = sum(x_i).
    ///
    /// Newton iteration:
    ///   d_new = (A*n^n*S + n*D_P) * D / ((A*n^n - 1)*D + (n+1)*D_P)
    ///   where D_P = D^(n+1) / (n^n * prod(x_i))
    fn get_d(&self) -> U256 {
        let n = U256::from(self.balances.len());
        let mut s = U256::ZERO;
        for b in &self.balances {
            s += *b;
        }
        if s.is_zero() {
            return U256::ZERO;
        }

        let ann = self.amplification * n;
        let mut d = s;

        for _ in 0..256 {
            let mut d_p = d;
            for b in &self.balances {
                if b.is_zero() {
                    return U256::ZERO;
                }
                // d_p = d_p * d / (b * n)
                d_p = d_p * d / (*b * n);
            }
            let d_prev = d;
            // d = (ann * s + d_p * n) * d / ((ann - 1) * d + (n + 1) * d_p)
            let numerator = (ann * s + d_p * n) * d;
            let denominator = (ann - U256::from(1)) * d + (n + U256::from(1)) * d_p;
            if denominator.is_zero() {
                return d;
            }
            d = numerator / denominator;

            // Convergence check (within 1 wei)
            if d > d_prev {
                if d - d_prev <= U256::from(1) {
                    break;
                }
            } else if d_prev - d <= U256::from(1) {
                break;
            }
        }
        d
    }

    /// Get y given x for the StableSwap invariant.
    ///
    /// Solves for y in the invariant equation, holding all other balances constant
    /// except for x_i (which is set to `x`) and x_j (which we solve for).
    fn get_y(&self, i: usize, j: usize, x: U256) -> Option<U256> {
        let n = self.balances.len();
        if i >= n || j >= n || i == j {
            return None;
        }

        let d = self.get_d();
        if d.is_zero() {
            return None;
        }

        let n_u256 = U256::from(n);
        let ann = self.amplification * n_u256;

        let mut s = x;
        let mut c = d * d / (x * n_u256);
        for k in 0..n {
            if k == i || k == j {
                continue;
            }
            s += self.balances[k];
            c = c * d / (self.balances[k] * n_u256);
        }
        c = c * d / (ann * n_u256);
        let b = s + d / ann;

        let mut y = d;
        for _ in 0..256 {
            let y_prev = y;
            // y = (y^2 + c) / (2*y + b - d)
            y = (y * y + c) / (U256::from(2) * y + b - d);
            if y > y_prev {
                if y - y_prev <= U256::from(1) {
                    break;
                }
            } else if y_prev - y <= U256::from(1) {
                break;
            }
        }
        Some(y)
    }
}

impl Pool for CurvePool {
    fn protocol(&self) -> ProtocolType {
        ProtocolType::Curve
    }
    fn address(&self) -> Address {
        self.address
    }
    fn tokens(&self) -> Vec<Address> {
        self.tokens.clone()
    }
    fn fee_bps(&self) -> u32 {
        self.fee_bps
    }

    fn get_amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256> {
        if amount_in.is_zero() {
            return None;
        }
        let i = self.tokens.iter().position(|t| *t == token_in)?;
        // For 2-token pools, output token is the other one
        let j = if i == 0 { 1 } else { 0 };

        let x = self.balances[i] + amount_in;
        let y = self.get_y(i, j, x)?;
        let dy = self.balances[j].checked_sub(y)?;
        if dy.is_zero() {
            return None;
        }

        // Apply fee: fee is taken from the output amount
        let fee = dy * U256::from(self.fee_bps) / U256::from(10000);
        Some(dy - fee)
    }

    fn get_amount_in(&self, token_out: Address, amount_out: U256) -> Option<U256> {
        if amount_out.is_zero() {
            return None;
        }
        let j = self.tokens.iter().position(|t| *t == token_out)?;
        let i = if j == 0 { 1 } else { 0 };

        // Reverse the fee to get the pre-fee output amount
        let amount_out_before_fee =
            amount_out * U256::from(10000) / U256::from(10000 - self.fee_bps);
        let y_new = self.balances[j].checked_sub(amount_out_before_fee)?;

        // Solve for x given y (swap i and j roles in get_y)
        let x = self.get_y(j, i, y_new)?;
        let dx = x.checked_sub(self.balances[i])?;
        Some(dx + U256::from(1))
    }

    fn update_state(&mut self, reserve0: U256, reserve1: U256) {
        if self.balances.len() >= 2 {
            self.balances[0] = reserve0;
            self.balances[1] = reserve1;
        }
    }

    fn encode_swap(&self, _token_in: Address, _amount_in: U256, _min_out: U256) -> Vec<u8> {
        Vec::new() // Placeholder - real encoding in calldata builder
    }

    fn liquidity_depth(&self) -> U256 {
        self.balances.iter().fold(U256::ZERO, |acc, b| acc + *b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn setup_curve_pool() -> CurvePool {
        let token0 = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC
        let token1 = address!("dAC17F958D2ee523a2206206994597C13D831ec7"); // USDT
        let mut pool = CurvePool::new(Address::ZERO, vec![token0, token1], 100, 4);
        // 10M USDC and 10M USDT (both 6 decimals)
        pool.update_state(
            U256::from(10_000_000_000_000u64),
            U256::from(10_000_000_000_000u64),
        );
        pool
    }

    #[test]
    fn test_curve_stableswap() {
        let pool = setup_curve_pool();
        let amount_in = U256::from(1_000_000_000u64); // 1000 USDC
        let out = pool.get_amount_out(pool.tokens[0], amount_in).unwrap();
        // For stableswap with high A, output should be very close to input minus fee
        // Fee is 0.04%, so ~999.96 USDT expected
        assert!(out > U256::from(999_000_000u64)); // > 999 USDT
        assert!(out < U256::from(1_000_000_000u64)); // < 1000 USDT
    }

    #[test]
    fn test_curve_protocol() {
        let pool = setup_curve_pool();
        assert_eq!(pool.protocol(), ProtocolType::Curve);
    }

    #[test]
    fn test_curve_zero_amount() {
        let pool = setup_curve_pool();
        assert!(pool.get_amount_out(pool.tokens[0], U256::ZERO).is_none());
    }
}
