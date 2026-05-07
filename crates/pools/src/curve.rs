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

/// Snapshot of a Curve 2-coin pool *after* a hypothetical victim swap
/// has been applied. Returned by [`CurvePool::predict_post_state`] so the
/// mempool post-state simulator can update its graph-edge cache without
/// round-tripping to RPC.
///
/// The post-balances reflect the pool's view *after* the swap settles:
/// balance_in grew by the full input, balance_out fell by the user-
/// visible output, and the fee is implicitly retained in the pool (which
/// is why `new_balance_out > y_new` in Curve's accounting).
///
/// `analytical` mirrors the V3 `single_tick` flag: `true` when the
/// Newton iteration converged on a valid post-state and the math is
/// trustworthy. `false` triggers the EVM fork-replay fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurvePostState {
    /// Index in `pool.tokens` of the input token.
    pub i: usize,
    /// Index in `pool.tokens` of the output token.
    pub j: usize,
    /// `pool.balances[i]` after the swap settles (= prev + amount_in).
    pub new_balance_in: U256,
    /// `pool.balances[j]` after the swap settles. Equals
    /// `prev - amount_out` (fee retained in pool).
    pub new_balance_out: U256,
    /// Output amount the swapper receives, post-fee.
    pub amount_out: U256,
    /// `true` when Newton converged on a valid post-state and the
    /// analytical answer is trustworthy. `false` signals an EVM fork-
    /// replay fallback to the caller.
    pub analytical: bool,
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

    /// Predict the pool's post-swap state under the same StableSwap
    /// Newton iteration as [`Pool::get_amount_out`]. Used by the mempool
    /// post-state simulator to feed updated balances into the graph edge
    /// cache without an RPC round-trip.
    ///
    /// Returns `None` when the inputs are invalid: zero amount, unknown
    /// token, fewer than 2 coins, swap would drain a balance to zero, or
    /// Newton fails to converge to a smaller `balance_out`. The fee
    /// retention model matches Curve's on-chain `StableSwap` contract:
    /// `new_balance_in = balances[i] + amount_in`,
    /// `new_balance_out = balances[j] - amount_out`. The pool keeps the
    /// fee, which is why the post-balance is *higher* than the Newton-
    /// solved `y`.
    pub fn predict_post_state(
        &self,
        token_in: Address,
        amount_in: U256,
    ) -> Option<CurvePostState> {
        if amount_in.is_zero() {
            return None;
        }
        if self.balances.len() < 2 {
            return None;
        }
        let i = self.tokens.iter().position(|t| *t == token_in)?;
        // 2-coin variant: output is the other index. Generalising to 3+
        // coins requires the caller to pick j explicitly; pinned at
        // 2-coin here to match the existing `Pool::get_amount_out`.
        if self.tokens.len() != 2 {
            return None;
        }
        let j = if i == 0 { 1 } else { 0 };

        let new_balance_in = self.balances[i].checked_add(amount_in)?;
        let y_new = self.get_y(i, j, new_balance_in)?;
        // dy is what the pool owes before fee — Newton can briefly
        // overshoot in the reverse direction so guard with checked_sub.
        let dy = self.balances[j].checked_sub(y_new)?;
        if dy.is_zero() {
            return None;
        }
        // Fee is taken out of the user's payout, retained in the pool.
        let fee = dy * U256::from(self.fee_bps) / U256::from(10_000u64);
        let amount_out = dy.checked_sub(fee)?;
        let new_balance_out = self.balances[j].checked_sub(amount_out)?;

        Some(CurvePostState {
            i,
            j,
            new_balance_in,
            new_balance_out,
            amount_out,
            analytical: true,
        })
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

    // ----- predict_post_state -----

    #[test]
    fn predict_post_state_none_for_zero_amount() {
        let pool = setup_curve_pool();
        assert!(pool.predict_post_state(pool.tokens[0], U256::ZERO).is_none());
    }

    #[test]
    fn predict_post_state_none_for_unknown_token() {
        let pool = setup_curve_pool();
        let bogus = address!("0000000000000000000000000000000000001234");
        assert!(pool.predict_post_state(bogus, U256::from(1u64)).is_none());
    }

    #[test]
    fn predict_post_state_none_for_uninitialised_pool() {
        let token0 = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token1 = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let pool = CurvePool::new(Address::ZERO, vec![token0, token1], 100, 4);
        // balances default to zero — Newton has nothing to work with.
        assert!(pool
            .predict_post_state(token0, U256::from(1_000_000_000u64))
            .is_none());
    }

    #[test]
    fn predict_post_state_balances_shift_correctly() {
        let pool = setup_curve_pool();
        let amount_in = U256::from(1_000_000_000u64); // 1000 USDC
        let post = pool
            .predict_post_state(pool.tokens[0], amount_in)
            .expect("post state");
        // Direction sanity: input balance grew by exactly amount_in;
        // output balance shrank by exactly amount_out.
        assert_eq!(post.i, 0);
        assert_eq!(post.j, 1);
        assert_eq!(post.new_balance_in, pool.balances[0] + amount_in);
        assert_eq!(post.new_balance_out, pool.balances[1] - post.amount_out);
        assert!(post.analytical);
    }

    #[test]
    fn predict_post_state_amount_out_matches_get_amount_out() {
        // Parity guard: the post-state predictor and the legacy
        // `get_amount_out` must return the same `amount_out` for the
        // same input — they share the same StableSwap Newton iteration.
        let pool = setup_curve_pool();
        for amt in [
            U256::from(1_000_000u64),       // 1 USDC
            U256::from(1_000_000_000u64),   // 1k USDC
            U256::from(100_000_000_000u64), // 100k USDC
        ] {
            let legacy = pool
                .get_amount_out(pool.tokens[0], amt)
                .expect("legacy amount_out");
            let post = pool
                .predict_post_state(pool.tokens[0], amt)
                .expect("post state");
            assert_eq!(
                post.amount_out, legacy,
                "predict_post_state diverged from get_amount_out at amt={amt}"
            );
        }
    }

    #[test]
    fn predict_post_state_reverse_direction() {
        let pool = setup_curve_pool();
        let amount_in = U256::from(1_000_000_000u64);
        let post = pool
            .predict_post_state(pool.tokens[1], amount_in)
            .expect("post state");
        assert_eq!(post.i, 1);
        assert_eq!(post.j, 0);
        assert_eq!(post.new_balance_in, pool.balances[1] + amount_in);
        assert_eq!(post.new_balance_out, pool.balances[0] - post.amount_out);
    }

    #[test]
    fn predict_post_state_3coin_pool_unsupported() {
        // 2-coin pinning is intentional. A 3-coin pool requires the
        // caller to pick `j` explicitly; rather than guess, return None
        // and let the caller skip with `protocol_unsupported`.
        let token0 = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token1 = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let token2 = address!("6B175474E89094C44Da98b954EedeAC495271d0F"); // DAI
        let mut pool = CurvePool::new(Address::ZERO, vec![token0, token1, token2], 100, 4);
        pool.balances = vec![
            U256::from(10_000_000_000_000u64),
            U256::from(10_000_000_000_000u64),
            U256::from(10_000_000_000_000u64),
        ];
        assert!(pool
            .predict_post_state(token0, U256::from(1_000_000_000u64))
            .is_none());
    }
}
