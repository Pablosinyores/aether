use aether_common::types::ArbOpportunity;

/// Detected negative cycle in the price graph.
///
/// A negative-weight cycle in the `-ln(rate)` price graph corresponds to a
/// sequence of swaps whose product of exchange rates exceeds 1.0 — i.e., a
/// profitable arbitrage loop.
#[derive(Debug, Clone)]
pub struct DetectedCycle {
    /// Vertex indices forming the cycle (first == last to close the loop).
    pub path: Vec<usize>,
    /// Total weight of the cycle (negative = profitable).
    pub total_weight: f64,
}

impl DetectedCycle {
    /// Check if this cycle is profitable (negative total weight).
    pub fn is_profitable(&self) -> bool {
        self.total_weight < 0.0
    }

    /// Approximate profit factor: `e^(-total_weight) - 1`.
    ///
    /// A `total_weight` of `-0.01` means roughly 1% profit before gas costs.
    /// This is derived from the fact that `total_weight = -sum(ln(rate_i))`,
    /// so the product of rates = `e^(-total_weight)`.
    pub fn profit_factor(&self) -> f64 {
        (-self.total_weight).exp() - 1.0
    }

    /// Number of hops (edges) in the cycle.
    pub fn num_hops(&self) -> usize {
        if self.path.len() <= 1 {
            0
        } else {
            self.path.len() - 1
        }
    }
}

/// Ranked arbitrage opportunity with a computed score for ordering.
#[derive(Debug, Clone)]
pub struct RankedOpportunity {
    pub opportunity: ArbOpportunity,
    /// Score for ranking (higher = better). Computed as `net_profit / gas_cost`.
    pub score: f64,
}

impl RankedOpportunity {
    pub fn new(opportunity: ArbOpportunity) -> Self {
        let gas_cost = opportunity.gas_cost_wei.to::<u128>() as f64;
        let net_profit = opportunity.net_profit_wei.to::<u128>() as f64;
        let score = if gas_cost > 0.0 {
            net_profit / gas_cost
        } else {
            0.0
        };
        Self { opportunity, score }
    }
}

/// Top-K opportunity collector.
///
/// Maintains a sorted list of the best K opportunities by score. Insertion is
/// O(K log K) due to the sort, which is acceptable for small K (typically 5-20).
pub struct TopKCollector {
    opportunities: Vec<RankedOpportunity>,
    k: usize,
}

impl TopKCollector {
    pub fn new(k: usize) -> Self {
        Self {
            opportunities: Vec::with_capacity(k),
            k,
        }
    }

    /// Insert an opportunity. If the collector is full and the new opportunity's
    /// score exceeds the worst (last) entry, the worst is evicted.
    pub fn insert(&mut self, opp: RankedOpportunity) {
        if self.opportunities.len() < self.k {
            self.opportunities.push(opp);
            self.opportunities.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else if let Some(worst) = self.opportunities.last() {
            if opp.score > worst.score {
                self.opportunities.pop();
                self.opportunities.push(opp);
                self.opportunities.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }
    }

    /// Borrow the current ranked results (sorted best-first).
    pub fn results(&self) -> &[RankedOpportunity] {
        &self.opportunities
    }

    /// Consume the collector and return the ranked results.
    pub fn into_results(self) -> Vec<RankedOpportunity> {
        self.opportunities
    }

    /// Number of opportunities currently held.
    pub fn len(&self) -> usize {
        self.opportunities.len()
    }

    /// Whether the collector is empty.
    pub fn is_empty(&self) -> bool {
        self.opportunities.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    // --------------- DetectedCycle ---------------

    #[test]
    fn test_profitable_cycle() {
        let cycle = DetectedCycle {
            path: vec![0, 1, 2, 0],
            total_weight: -0.05,
        };
        assert!(cycle.is_profitable());
    }

    #[test]
    fn test_unprofitable_cycle() {
        let cycle = DetectedCycle {
            path: vec![0, 1, 2, 0],
            total_weight: 0.05,
        };
        assert!(!cycle.is_profitable());
    }

    #[test]
    fn test_zero_weight_not_profitable() {
        let cycle = DetectedCycle {
            path: vec![0, 1, 0],
            total_weight: 0.0,
        };
        assert!(!cycle.is_profitable());
    }

    #[test]
    fn test_profit_factor_positive() {
        // weight = -0.01 => profit_factor ~ e^0.01 - 1 ~ 0.01005
        let cycle = DetectedCycle {
            path: vec![0, 1, 2, 0],
            total_weight: -0.01,
        };
        let pf = cycle.profit_factor();
        assert!(pf > 0.0);
        assert!((pf - (0.01_f64.exp() - 1.0)).abs() < 1e-10);
    }

    #[test]
    fn test_profit_factor_negative_for_loss() {
        // weight = +0.05 => profit_factor = e^(-0.05) - 1 < 0
        let cycle = DetectedCycle {
            path: vec![0, 1, 2, 0],
            total_weight: 0.05,
        };
        assert!(cycle.profit_factor() < 0.0);
    }

    #[test]
    fn test_num_hops_triangle() {
        let cycle = DetectedCycle {
            path: vec![0, 1, 2, 0],
            total_weight: -0.01,
        };
        assert_eq!(cycle.num_hops(), 3);
    }

    #[test]
    fn test_num_hops_two_hop() {
        let cycle = DetectedCycle {
            path: vec![0, 1, 0],
            total_weight: -0.01,
        };
        assert_eq!(cycle.num_hops(), 2);
    }

    #[test]
    fn test_num_hops_empty() {
        let cycle = DetectedCycle {
            path: vec![],
            total_weight: 0.0,
        };
        assert_eq!(cycle.num_hops(), 0);
    }

    #[test]
    fn test_num_hops_single_vertex() {
        let cycle = DetectedCycle {
            path: vec![0],
            total_weight: 0.0,
        };
        assert_eq!(cycle.num_hops(), 0);
    }

    // --------------- RankedOpportunity ---------------

    fn make_opp(net_profit: u128, gas_cost: u128) -> ArbOpportunity {
        ArbOpportunity {
            id: "test".to_string(),
            hops: vec![],
            total_profit_wei: U256::from(net_profit + gas_cost),
            total_gas: 200_000,
            gas_cost_wei: U256::from(gas_cost),
            net_profit_wei: U256::from(net_profit),
            block_number: 18_000_000,
            timestamp_ns: 0,
        }
    }

    #[test]
    fn test_ranked_opportunity_score() {
        let opp = make_opp(2_000_000, 1_000_000);
        let ranked = RankedOpportunity::new(opp);
        // score = 2_000_000 / 1_000_000 = 2.0
        assert!((ranked.score - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_ranked_opportunity_zero_gas() {
        let opp = ArbOpportunity {
            id: "test".to_string(),
            hops: vec![],
            total_profit_wei: U256::from(1_000_000u64),
            total_gas: 0,
            gas_cost_wei: U256::ZERO,
            net_profit_wei: U256::from(1_000_000u64),
            block_number: 18_000_000,
            timestamp_ns: 0,
        };
        let ranked = RankedOpportunity::new(opp);
        assert_eq!(ranked.score, 0.0);
    }

    // --------------- TopKCollector ---------------

    #[test]
    fn test_top_k_empty() {
        let collector = TopKCollector::new(5);
        assert!(collector.is_empty());
        assert_eq!(collector.len(), 0);
        assert!(collector.results().is_empty());
    }

    #[test]
    fn test_top_k_insert_below_capacity() {
        let mut collector = TopKCollector::new(3);
        collector.insert(RankedOpportunity::new(make_opp(100, 50)));
        collector.insert(RankedOpportunity::new(make_opp(200, 50)));
        assert_eq!(collector.len(), 2);
    }

    #[test]
    fn test_top_k_at_capacity_rejects_worse() {
        let mut collector = TopKCollector::new(2);
        collector.insert(RankedOpportunity::new(make_opp(300, 100))); // score 3.0
        collector.insert(RankedOpportunity::new(make_opp(200, 100))); // score 2.0

        // Attempt to insert worse score
        collector.insert(RankedOpportunity::new(make_opp(100, 100))); // score 1.0
        assert_eq!(collector.len(), 2);
        // Best should still be score 3.0
        assert!((collector.results()[0].score - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_top_k_at_capacity_accepts_better() {
        let mut collector = TopKCollector::new(2);
        collector.insert(RankedOpportunity::new(make_opp(200, 100))); // score 2.0
        collector.insert(RankedOpportunity::new(make_opp(100, 100))); // score 1.0

        // Insert better score, should evict worst
        collector.insert(RankedOpportunity::new(make_opp(500, 100))); // score 5.0
        assert_eq!(collector.len(), 2);
        assert!((collector.results()[0].score - 5.0).abs() < 1e-10);
        assert!((collector.results()[1].score - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_top_k_sorted_descending() {
        let mut collector = TopKCollector::new(5);
        collector.insert(RankedOpportunity::new(make_opp(100, 100))); // 1.0
        collector.insert(RankedOpportunity::new(make_opp(500, 100))); // 5.0
        collector.insert(RankedOpportunity::new(make_opp(300, 100))); // 3.0
        collector.insert(RankedOpportunity::new(make_opp(200, 100))); // 2.0
        collector.insert(RankedOpportunity::new(make_opp(400, 100))); // 4.0

        let results = collector.results();
        for i in 0..results.len() - 1 {
            assert!(results[i].score >= results[i + 1].score);
        }
    }

    #[test]
    fn test_top_k_into_results() {
        let mut collector = TopKCollector::new(3);
        collector.insert(RankedOpportunity::new(make_opp(300, 100)));
        collector.insert(RankedOpportunity::new(make_opp(100, 100)));
        let results = collector.into_results();
        assert_eq!(results.len(), 2);
    }
}
