use alloy::primitives::{address, Address, U256};
use aether_common::types::ProtocolType;
use crate::Pool;

/// Canonical Bancor Network Token (BNT) address on Ethereum mainnet.
///
/// Every Bancor V3 pool is a token/BNT pair; the multi-hop case where a
/// victim swap routes `tokenA → BNT → tokenB` hits two such pools, and the
/// dispatch logic in the mempool pipeline needs a constant handle on BNT
/// to look up the two affected pools in the registry without an extra
/// per-event allocation.
pub const BNT_ADDRESS: Address = address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C");

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

/// Snapshot of a [`BancorPool`] *after* a hypothetical victim swap has
/// been applied. Returned by [`BancorPool::predict_post_state`] so the
/// mempool post-state simulator can update its graph-edge cache
/// without round-tripping to RPC.
///
/// `new_balance_in` and `new_balance_out` are aligned with the swap
/// direction (not the pool's `token` / `bnt` fields), mirroring the
/// `CurvePostState` shape — the caller's `unified_to_post_reserves`
/// helper then trusts them directly without re-deriving the direction.
///
/// `analytical` is `true` whenever the predictor returns a value; the
/// Bancor bonding curve is closed-form and always trustworthy when both
/// reserves are non-zero. The flag is kept on the struct so callers can
/// drive the same `predict_post_state_with_replay` dispatch they use
/// for the other pool families.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BancorPostState {
    /// Post-swap balance on the input side (the side `token_in` belongs
    /// to). `bnt_balance` when `token_in == bnt`, else `token_balance`.
    pub new_balance_in: U256,
    /// Post-swap balance on the output side.
    pub new_balance_out: U256,
    /// Output amount the swapper receives, post-fee, in raw token
    /// units (no decimals normalisation).
    pub amount_out: U256,
    /// Confidence flag — see struct docs.
    pub analytical: bool,
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

    /// Predict the pool's post-swap state under the Bancor bonding curve
    /// assumption. Mempool post-state simulation uses this to update its
    /// graph-edge cache after a decoded victim swap, without round-tripping
    /// to RPC.
    ///
    /// Returns `None` when the inputs cannot produce a valid post-state:
    ///   - pool has no liquidity on either side
    ///   - `amount_in` is zero
    ///   - `token_in` is neither the pool's `token` nor BNT — this is the
    ///     multi-hop case where the victim swaps two non-BNT tokens, the
    ///     trade hits two pools, and a single `BancorPool` cannot predict
    ///     the full path. Callers must detect that case upstream and
    ///     either bail or look up both affected pools.
    ///
    /// `analytical = true` on every `Some` return because the Bancor
    /// bonding curve is closed-form. The flag is carried on the struct
    /// to keep the public shape uniform with `V3PostState`,
    /// `CurvePostState`, and `BalancerPostState`.
    pub fn predict_post_state(
        &self,
        token_in: Address,
        amount_in: U256,
    ) -> Option<BancorPostState> {
        if self.token_balance.is_zero() || self.bnt_balance.is_zero() || amount_in.is_zero() {
            return None;
        }
        if token_in != self.token && token_in != self.bnt {
            // Multi-hop: caller must split into two pool predictions.
            return None;
        }
        let (bal_in, bal_out) = if token_in == self.token {
            (self.token_balance, self.bnt_balance)
        } else {
            (self.bnt_balance, self.token_balance)
        };

        // Fee applied to input — matches `get_amount_out` above.
        let fee_complement = U256::from(10_000u64 - self.fee_bps as u64);
        let amount_in_after_fee = amount_in * fee_complement / U256::from(10_000u64);
        let numerator = bal_out * amount_in_after_fee;
        let denominator = bal_in + amount_in_after_fee;
        if denominator.is_zero() {
            return None;
        }
        let amount_out = numerator / denominator;
        if amount_out.is_zero() || amount_out >= bal_out {
            // Degenerate: amount_in_after_fee rounds to zero against a
            // huge reserve, or the swap would drain the output side. Bail
            // rather than emit a zero-output edge.
            return None;
        }

        // Pool side accounting: the swapper deposits `amount_in` (gross,
        // not net-of-fee) into the input reserve and withdraws
        // `amount_out` from the output reserve. The fee is implicitly
        // retained in the pool, which is why `new_balance_in` grows by
        // the full `amount_in` rather than the post-fee net.
        let new_balance_in = bal_in + amount_in;
        let new_balance_out = bal_out - amount_out;

        Some(BancorPostState {
            new_balance_in,
            new_balance_out,
            amount_out,
            analytical: true,
        })
    }

    /// Predict the joint post-state of a Bancor V3 multi-hop swap
    /// `token_in → BNT → token_out` that touches two pools (`self` for the
    /// `token_in` leg, `second_pool` for the `token_out` leg). The mempool
    /// pipeline calls this when the decoded swap's `token_in` / `token_out`
    /// pair has neither side equal to BNT — the underlying trade still
    /// settles atomically on-chain through the BNT intermediary, but the
    /// pre-state predictor on a single pool cannot model both legs.
    ///
    /// Returns `Some((leg_a_post, leg_b_post))` where:
    ///   * `leg_a_post` is `self`'s post-state with `token_in` going in
    ///     and BNT coming out (`new_balance_in` is the new token_in
    ///     reserve, `new_balance_out` is the new BNT reserve on `self`);
    ///   * `leg_b_post` is `second_pool`'s post-state with the
    ///     intermediate BNT amount going in and `token_out` coming out
    ///     (`new_balance_in` is the new BNT reserve on `second_pool`,
    ///     `new_balance_out` is the new token_out reserve).
    ///
    /// Returns `None` when:
    ///   * `self` does not pair `token_in` against BNT (i.e. neither
    ///     `self.token == token_in` nor `self.bnt == token_in`, or
    ///     `self.bnt != BNT_ADDRESS`);
    ///   * `second_pool` does not pair BNT against `token_out`;
    ///   * either leg's `predict_post_state` returns `None` (empty
    ///     reserves, degenerate amount, etc.);
    ///   * the intermediate BNT amount produced by leg A is zero.
    ///
    /// The caller is responsible for confirming that the two pools come
    /// from the live registry and that their `self.bnt` fields match
    /// `BNT_ADDRESS` — this function checks both invariants defensively
    /// because the multi-hop dispatch path is a structural change to the
    /// pipeline and a silent registry / config mistake would produce
    /// confidently-wrong graph edges instead of a clean `None`.
    pub fn predict_post_state_multihop(
        &self,
        token_in: Address,
        amount_in: U256,
        token_out: Address,
        second_pool: &BancorPool,
    ) -> Option<(BancorPostState, BancorPostState)> {
        // Both pools must pair against the canonical BNT address. A pool
        // whose `bnt` field drifts (e.g. test fixture, off-by-one config)
        // would otherwise produce an "intermediate" amount denominated in
        // an entirely different token without surfacing the mistake.
        if self.bnt != BNT_ADDRESS || second_pool.bnt != BNT_ADDRESS {
            return None;
        }
        // Multi-hop only — by construction the caller routes single-leg
        // swaps through `predict_post_state` directly. If `token_in`
        // happens to be BNT (or `token_out` is BNT) this is not a real
        // multi-hop, and trying to model it through two pools would
        // double-count the BNT leg.
        if token_in == BNT_ADDRESS || token_out == BNT_ADDRESS {
            return None;
        }
        // Leg A pool must be the (token_in, BNT) pair.
        if self.token != token_in {
            return None;
        }
        // Leg B pool must be the (BNT, token_out) pair.
        if second_pool.token != token_out {
            return None;
        }

        let leg_a = self.predict_post_state(token_in, amount_in)?;
        if leg_a.amount_out.is_zero() {
            return None;
        }
        let leg_b = second_pool.predict_post_state(BNT_ADDRESS, leg_a.amount_out)?;
        Some((leg_a, leg_b))
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

    // ----- predict_post_state -----

    fn seeded_pool() -> BancorPool {
        let mut pool = BancorPool::new(
            Address::ZERO,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
            address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C"), // BNT
            30,
        );
        pool.update_state(
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(2_000_000_000_000_000_000_000u128),
        );
        pool
    }

    #[test]
    fn predict_post_state_none_for_zero_amount() {
        let pool = seeded_pool();
        assert!(pool.predict_post_state(pool.token, U256::ZERO).is_none());
    }

    #[test]
    fn predict_post_state_none_for_uninitialised_pool() {
        let pool = BancorPool::new(
            Address::ZERO,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C"),
            30,
        );
        assert!(pool.predict_post_state(pool.token, U256::from(1u64)).is_none());
    }

    #[test]
    fn predict_post_state_none_for_unknown_token() {
        let pool = seeded_pool();
        let bogus = address!("dddddddddddddddddddddddddddddddddddddddd");
        assert!(pool.predict_post_state(bogus, U256::from(1u64)).is_none());
    }

    #[test]
    fn predict_post_state_token_in_to_bnt_balances_shift_correctly() {
        let pool = seeded_pool();
        let amount_in = U256::from(10_000_000_000_000_000_000u128); // 10 ETH
        let post = pool
            .predict_post_state(pool.token, amount_in)
            .expect("predictor returns Some");
        assert!(post.analytical);
        assert!(post.amount_out > U256::ZERO);
        // new_balance_in is on the token side (deposited gross amount_in).
        assert_eq!(post.new_balance_in, pool.token_balance + amount_in);
        // new_balance_out is on the BNT side (withdrew amount_out).
        assert_eq!(post.new_balance_out, pool.bnt_balance - post.amount_out);
    }

    #[test]
    fn predict_post_state_bnt_to_token_balances_shift_correctly() {
        let pool = seeded_pool();
        let amount_in = U256::from(50_000_000_000_000_000_000u128); // 50 BNT
        let post = pool
            .predict_post_state(pool.bnt, amount_in)
            .expect("predictor returns Some");
        assert!(post.analytical);
        // Reverse direction: new_balance_in = BNT side; new_balance_out = token side.
        assert_eq!(post.new_balance_in, pool.bnt_balance + amount_in);
        assert_eq!(post.new_balance_out, pool.token_balance - post.amount_out);
    }

    #[test]
    fn predict_post_state_amount_out_matches_get_amount_out() {
        let pool = seeded_pool();
        // Skip degenerate sizes (amount_in_after_fee rounds to zero) — the
        // predictor bails on those by design while `get_amount_out` returns
        // a literal zero.
        for amt in [
            U256::from(1_000_000_000_000_000_000u128),  // 1 ETH
            U256::from(100_000_000_000_000_000_000u128), // 100 ETH
        ] {
            let analytical_out = pool.get_amount_out(pool.token, amt).expect("get_amount_out");
            let predicted = pool
                .predict_post_state(pool.token, amt)
                .expect("predict_post_state");
            assert_eq!(
                predicted.amount_out, analytical_out,
                "predict_post_state diverged from get_amount_out at amt={amt}"
            );
        }
    }

    #[test]
    fn predict_post_state_none_for_degenerate_small_amount() {
        // `amount_in_after_fee` rounds to zero against a large reserve —
        // the predictor bails so the caller doesn't emit a zero-output
        // graph edge that would corrupt Bellman-Ford weights.
        let pool = seeded_pool();
        assert!(pool.predict_post_state(pool.token, U256::from(1u64)).is_none());
    }

    // ----- predict_post_state_multihop -----

    fn weth_address() -> Address {
        address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
    }

    fn link_address() -> Address {
        address!("514910771AF9Ca656af840dff83E8264EcF986CA")
    }

    fn weth_bnt_pool() -> BancorPool {
        let mut pool = BancorPool::new(
            address!("aaaa000000000000000000000000000000000001"),
            weth_address(),
            BNT_ADDRESS,
            30,
        );
        pool.update_state(
            U256::from(1_000_000_000_000_000_000_000u128), // 1000 WETH
            U256::from(2_000_000_000_000_000_000_000u128), // 2000 BNT
        );
        pool
    }

    fn link_bnt_pool() -> BancorPool {
        let mut pool = BancorPool::new(
            address!("aaaa000000000000000000000000000000000002"),
            link_address(),
            BNT_ADDRESS,
            30,
        );
        pool.update_state(
            U256::from(500_000_000_000_000_000_000u128), // 500 LINK
            U256::from(1_500_000_000_000_000_000_000u128), // 1500 BNT
        );
        pool
    }

    #[test]
    fn multihop_happy_path_returns_two_aligned_post_states() {
        let leg_a_pool = weth_bnt_pool();
        let leg_b_pool = link_bnt_pool();
        let amount_in = U256::from(10_000_000_000_000_000_000u128); // 10 WETH

        let (leg_a, leg_b) = leg_a_pool
            .predict_post_state_multihop(weth_address(), amount_in, link_address(), &leg_b_pool)
            .expect("multihop predictor returns Some");

        // Leg A: WETH-in, BNT-out → new_balance_in tracks WETH side,
        // new_balance_out tracks BNT side.
        assert!(leg_a.analytical);
        assert_eq!(leg_a.new_balance_in, leg_a_pool.token_balance + amount_in);
        assert_eq!(leg_a.new_balance_out, leg_a_pool.bnt_balance - leg_a.amount_out);

        // Leg B: BNT-in, LINK-out → new_balance_in tracks BNT side,
        // new_balance_out tracks LINK side. Intermediate amount equals
        // leg A's amount_out.
        assert!(leg_b.analytical);
        assert_eq!(leg_b.new_balance_in, leg_b_pool.bnt_balance + leg_a.amount_out);
        assert_eq!(leg_b.new_balance_out, leg_b_pool.token_balance - leg_b.amount_out);
        assert!(leg_b.amount_out > U256::ZERO);
    }

    #[test]
    fn multihop_returns_none_when_token_in_is_bnt() {
        // If `token_in == BNT` this is a single-leg swap that belongs on
        // `predict_post_state`, not on the multi-hop helper.
        let leg_a_pool = weth_bnt_pool();
        let leg_b_pool = link_bnt_pool();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        assert!(leg_a_pool
            .predict_post_state_multihop(BNT_ADDRESS, amount_in, link_address(), &leg_b_pool)
            .is_none());
    }

    #[test]
    fn multihop_returns_none_when_token_out_is_bnt() {
        // Mirror of the above: BNT on the output side is single-leg.
        let leg_a_pool = weth_bnt_pool();
        let leg_b_pool = link_bnt_pool();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        assert!(leg_a_pool
            .predict_post_state_multihop(weth_address(), amount_in, BNT_ADDRESS, &leg_b_pool)
            .is_none());
    }

    #[test]
    fn multihop_returns_none_when_leg_a_pool_mismatches_token_in() {
        // `self` must pair against `token_in`. Passing the LINK/BNT pool
        // as leg A while asking for a WETH-in swap must bail rather than
        // silently emit a wrong post-state.
        let leg_a_pool = link_bnt_pool();
        let leg_b_pool = link_bnt_pool();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        assert!(leg_a_pool
            .predict_post_state_multihop(weth_address(), amount_in, link_address(), &leg_b_pool)
            .is_none());
    }

    #[test]
    fn multihop_returns_none_when_leg_b_pool_mismatches_token_out() {
        let leg_a_pool = weth_bnt_pool();
        // Second pool also says "WETH" on its non-BNT side → wrong pool
        // for a swap whose `token_out` is LINK.
        let leg_b_pool = weth_bnt_pool();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        assert!(leg_a_pool
            .predict_post_state_multihop(weth_address(), amount_in, link_address(), &leg_b_pool)
            .is_none());
    }

    #[test]
    fn multihop_returns_none_when_either_pool_has_non_canonical_bnt() {
        // Defensive check: pool whose `bnt` field drifted from the canonical
        // BNT address. Without this guard the predictor would happily run
        // the math and produce "BNT-denominated" intermediate amounts in a
        // different token, corrupting downstream graph edges.
        let mut leg_a_pool = weth_bnt_pool();
        leg_a_pool.bnt = address!("dddddddddddddddddddddddddddddddddddddddd");
        let leg_b_pool = link_bnt_pool();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        assert!(leg_a_pool
            .predict_post_state_multihop(weth_address(), amount_in, link_address(), &leg_b_pool)
            .is_none());

        let leg_a_pool = weth_bnt_pool();
        let mut leg_b_pool = link_bnt_pool();
        leg_b_pool.bnt = address!("dddddddddddddddddddddddddddddddddddddddd");
        assert!(leg_a_pool
            .predict_post_state_multihop(weth_address(), amount_in, link_address(), &leg_b_pool)
            .is_none());
    }

    #[test]
    fn multihop_returns_none_when_leg_a_predictor_bails() {
        // Empty reserves on leg A → first `predict_post_state` returns
        // None → no second-leg call.
        let leg_a_pool = BancorPool::new(
            Address::ZERO,
            weth_address(),
            BNT_ADDRESS,
            30,
        );
        let leg_b_pool = link_bnt_pool();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        assert!(leg_a_pool
            .predict_post_state_multihop(weth_address(), amount_in, link_address(), &leg_b_pool)
            .is_none());
    }
}
