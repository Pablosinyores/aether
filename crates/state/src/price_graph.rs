use std::collections::HashMap;

use alloy::primitives::{Address, U256};
use aether_common::types::{PoolId, ProtocolType};

/// Edge in the price graph representing a swap between two tokens via a pool.
#[derive(Debug, Clone)]
pub struct PriceEdge {
    /// Source token vertex index.
    pub from: usize,
    /// Destination token vertex index.
    pub to: usize,
    /// Edge weight: `-ln(exchange_rate)`. Negative weight means profitable direction.
    pub weight: f64,
    /// Unique identifier of the pool backing this edge.
    pub pool_id: PoolId,
    /// On-chain address of the pool contract.
    pub pool_address: Address,
    /// DEX protocol type for gas estimation and swap encoding.
    pub protocol: ProtocolType,
    /// Available liquidity in the pool (for filtering low-liq edges).
    pub liquidity: U256,
}

/// Directed price graph for arbitrage detection.
///
/// Uses negative log-transformed exchange rates as edge weights.
/// A negative-weight cycle in this graph corresponds to a profitable arbitrage
/// opportunity, detectable via Bellman-Ford / SPFA.
#[derive(Debug, Clone)]
pub struct PriceGraph {
    /// Number of token vertices.
    num_vertices: usize,
    /// Adjacency list: `edges[from]` = vec of outgoing edges from vertex `from`.
    edges: Vec<Vec<PriceEdge>>,
    /// All edges in a flat list (used by Bellman-Ford which iterates all edges).
    all_edges: Vec<PriceEdge>,
    /// O(1) lookup: `(from, to, pool_id)` -> index in `all_edges`.
    edge_index: HashMap<(usize, usize, PoolId), usize>,
    /// Dirty flags per edge index -- only dirty edges need recomputation in
    /// partial Bellman-Ford scans.
    dirty: Vec<bool>,
}

impl PriceGraph {
    /// Create a new empty price graph with the given number of token vertices.
    pub fn new(num_vertices: usize) -> Self {
        Self {
            num_vertices,
            edges: vec![Vec::new(); num_vertices],
            all_edges: Vec::new(),
            edge_index: HashMap::new(),
            dirty: Vec::new(),
        }
    }

    /// Add or update an edge in the graph.
    ///
    /// `exchange_rate`: how many units of `to` token you receive per unit of
    /// `from` token. The stored weight is `-ln(exchange_rate)`.
    ///
    /// If an edge with the same `(from, to, pool_id)` already exists it is
    /// updated in place; otherwise a new edge is appended.
    #[allow(clippy::too_many_arguments)]
    pub fn add_edge(
        &mut self,
        from: usize,
        to: usize,
        exchange_rate: f64,
        pool_id: PoolId,
        pool_address: Address,
        protocol: ProtocolType,
        liquidity: U256,
    ) {
        let weight = -exchange_rate.ln();

        let edge = PriceEdge {
            from,
            to,
            weight,
            pool_id,
            pool_address,
            protocol,
            liquidity,
        };

        // Try to update an existing edge with matching (from, to, pool_id).
        if let Some(existing) = self.edges[from]
            .iter_mut()
            .find(|e| e.to == to && e.pool_id == pool_id)
        {
            existing.weight = weight;
            existing.liquidity = liquidity;
            // Mirror the update in the flat edge list via O(1) index lookup.
            if let Some(&idx) = self.edge_index.get(&(from, to, pool_id)) {
                self.all_edges[idx].weight = weight;
                self.all_edges[idx].liquidity = liquidity;
                if idx < self.dirty.len() {
                    self.dirty[idx] = true;
                }
            }
        } else {
            self.edges[from].push(edge.clone());
            let idx = self.all_edges.len();
            self.all_edges.push(edge);
            self.edge_index.insert((from, to, pool_id), idx);
            self.dirty.push(true);
        }
    }

    /// Update an edge's weight from raw reserve values.
    ///
    /// For a constant-product AMM (UniV2-style) the marginal rate is:
    /// `rate = (reserve_out / reserve_in) * fee_factor`
    /// where `fee_factor` accounts for the swap fee (e.g. 0.997 for 0.3% fee).
    ///
    /// This only updates an *existing* edge. If no matching edge is found the
    /// call is a no-op.
    pub fn update_edge_from_reserves(
        &mut self,
        from: usize,
        to: usize,
        pool_id: PoolId,
        reserve_in: f64,
        reserve_out: f64,
        fee_factor: f64,
    ) {
        if reserve_in <= 0.0 || reserve_out <= 0.0 {
            return;
        }
        let rate = (reserve_out / reserve_in) * fee_factor;

        if let Some(existing) = self.edges[from]
            .iter_mut()
            .find(|e| e.to == to && e.pool_id == pool_id)
        {
            existing.weight = -rate.ln();
            // Mirror the update in the flat edge list via O(1) index lookup.
            if let Some(&idx) = self.edge_index.get(&(from, to, pool_id)) {
                self.all_edges[idx].weight = existing.weight;
                if idx < self.dirty.len() {
                    self.dirty[idx] = true;
                }
            }
        }
    }

    /// Get all outgoing edges from a vertex.
    #[inline]
    pub fn edges_from(&self, vertex: usize) -> &[PriceEdge] {
        if vertex < self.edges.len() {
            &self.edges[vertex]
        } else {
            &[]
        }
    }

    /// Get the flat list of all edges (used by Bellman-Ford).
    #[inline]
    pub fn all_edges(&self) -> &[PriceEdge] {
        &self.all_edges
    }

    /// Number of token vertices in the graph.
    #[inline]
    pub fn num_vertices(&self) -> usize {
        self.num_vertices
    }

    /// Total number of edges in the graph.
    #[inline]
    pub fn num_edges(&self) -> usize {
        self.all_edges.len()
    }

    /// Returns `true` if any edge has been modified since the last
    /// [`clear_dirty`](Self::clear_dirty) call.
    pub fn has_dirty_edges(&self) -> bool {
        self.dirty.iter().any(|&d| d)
    }

    /// Return the indices (into `all_edges`) of all dirty edges.
    pub fn dirty_edge_indices(&self) -> Vec<usize> {
        self.dirty
            .iter()
            .enumerate()
            .filter(|(_, &d)| d)
            .map(|(i, _)| i)
            .collect()
    }

    /// Clear all dirty flags after a detection pass has processed them.
    pub fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }

    /// Get vertices affected by dirty edges (useful for partial Bellman-Ford
    /// that only re-relaxes the subgraph around changed edges).
    pub fn affected_vertices(&self) -> Vec<usize> {
        let mut affected = std::collections::HashSet::new();
        for (i, &is_dirty) in self.dirty.iter().enumerate() {
            if is_dirty {
                if let Some(edge) = self.all_edges.get(i) {
                    affected.insert(edge.from);
                    affected.insert(edge.to);
                }
            }
        }
        let mut result: Vec<usize> = affected.into_iter().collect();
        result.sort_unstable();
        result
    }

    /// Remove all edges belonging to the given pool (e.g. when a pool is
    /// deregistered or fails qualification).
    pub fn remove_pool_edges(&mut self, pool_id: &PoolId) {
        for adj in &mut self.edges {
            adj.retain(|e| &e.pool_id != pool_id);
        }
        self.all_edges.retain(|e| &e.pool_id != pool_id);
        // Rebuild edge_index from scratch since indices shifted after retain.
        self.edge_index.clear();
        for (idx, edge) in self.all_edges.iter().enumerate() {
            self.edge_index
                .insert((edge.from, edge.to, edge.pool_id), idx);
        }
        // Rebuild dirty flags to match the new all_edges length.
        self.dirty = vec![false; self.all_edges.len()];
    }

    /// Grow the graph to accommodate at least `new_size` vertices.
    /// Existing edges are preserved.
    pub fn resize(&mut self, new_size: usize) {
        if new_size > self.num_vertices {
            self.edges.resize(new_size, Vec::new());
            self.num_vertices = new_size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    fn make_pool_id(byte: u8, protocol: ProtocolType) -> PoolId {
        PoolId {
            address: Address::repeat_byte(byte),
            protocol,
        }
    }

    #[test]
    fn test_new_graph_is_empty() {
        let g = PriceGraph::new(5);
        assert_eq!(g.num_vertices(), 5);
        assert_eq!(g.num_edges(), 0);
        assert!(!g.has_dirty_edges());
        assert!(g.all_edges().is_empty());
    }

    #[test]
    fn test_add_single_edge() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        // rate = 2.0  =>  weight = -ln(2) ~ -0.693
        g.add_edge(
            0,
            1,
            2.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000_000u64),
        );

        assert_eq!(g.num_edges(), 1);
        assert_eq!(g.edges_from(0).len(), 1);
        assert_eq!(g.edges_from(1).len(), 0);

        let edge = &g.all_edges()[0];
        assert_eq!(edge.from, 0);
        assert_eq!(edge.to, 1);
        assert!((edge.weight - (-2.0_f64.ln())).abs() < 1e-12);
        assert_eq!(edge.pool_id, pool_id);
    }

    #[test]
    fn test_add_edge_marks_dirty() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            2.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );

        assert!(g.has_dirty_edges());
        assert_eq!(g.dirty_edge_indices(), vec![0]);
    }

    #[test]
    fn test_clear_dirty() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            2.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );

        g.clear_dirty();
        assert!(!g.has_dirty_edges());
        assert!(g.dirty_edge_indices().is_empty());
    }

    #[test]
    fn test_update_existing_edge() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            2.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.clear_dirty();

        // Update the same edge with a new rate.
        g.add_edge(
            0,
            1,
            3.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(2_000u64),
        );

        // Should still be 1 edge, not 2.
        assert_eq!(g.num_edges(), 1);
        assert_eq!(g.edges_from(0).len(), 1);

        let edge = &g.all_edges()[0];
        assert!((edge.weight - (-3.0_f64.ln())).abs() < 1e-12);
        assert_eq!(edge.liquidity, U256::from(2_000u64));

        // Should be dirty again.
        assert!(g.has_dirty_edges());
    }

    #[test]
    fn test_multiple_edges_same_from() {
        let mut g = PriceGraph::new(4);
        let pool_a = make_pool_id(1, ProtocolType::UniswapV2);
        let pool_b = make_pool_id(2, ProtocolType::SushiSwap);

        g.add_edge(
            0,
            1,
            2.0,
            pool_a,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.add_edge(
            0,
            2,
            1.5,
            pool_b,
            Address::repeat_byte(2),
            ProtocolType::SushiSwap,
            U256::from(500u64),
        );

        assert_eq!(g.num_edges(), 2);
        assert_eq!(g.edges_from(0).len(), 2);
        assert_eq!(g.edges_from(1).len(), 0);
        assert_eq!(g.edges_from(2).len(), 0);
    }

    #[test]
    fn test_parallel_edges_different_pools() {
        // Two different pools connecting the same pair (0 -> 1).
        let mut g = PriceGraph::new(3);
        let pool_a = make_pool_id(1, ProtocolType::UniswapV2);
        let pool_b = make_pool_id(2, ProtocolType::SushiSwap);

        g.add_edge(
            0,
            1,
            2.0,
            pool_a,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.add_edge(
            0,
            1,
            2.1,
            pool_b,
            Address::repeat_byte(2),
            ProtocolType::SushiSwap,
            U256::from(500u64),
        );

        assert_eq!(g.num_edges(), 2);
        assert_eq!(g.edges_from(0).len(), 2);
    }

    #[test]
    fn test_update_edge_from_reserves() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            2.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.clear_dirty();

        // reserve_out=2000, reserve_in=1000, fee=0.997 => rate = 2.0 * 0.997 = 1.994
        g.update_edge_from_reserves(0, 1, pool_id, 1000.0, 2000.0, 0.997);

        let expected_weight = -(2.0 * 0.997_f64).ln();
        let edge = &g.all_edges()[0];
        assert!((edge.weight - expected_weight).abs() < 1e-12);
        assert!(g.has_dirty_edges());
    }

    #[test]
    fn test_update_edge_from_reserves_zero_reserves() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            2.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.clear_dirty();

        let original_weight = g.all_edges()[0].weight;
        // Zero reserves should be a no-op.
        g.update_edge_from_reserves(0, 1, pool_id, 0.0, 2000.0, 0.997);
        assert!((g.all_edges()[0].weight - original_weight).abs() < 1e-12);
        assert!(!g.has_dirty_edges());

        g.update_edge_from_reserves(0, 1, pool_id, 1000.0, 0.0, 0.997);
        assert!((g.all_edges()[0].weight - original_weight).abs() < 1e-12);
    }

    #[test]
    fn test_update_edge_from_reserves_nonexistent() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        // No edges added. Should be a no-op (no panic).
        g.update_edge_from_reserves(0, 1, pool_id, 1000.0, 2000.0, 0.997);
        assert_eq!(g.num_edges(), 0);
    }

    #[test]
    fn test_affected_vertices() {
        let mut g = PriceGraph::new(5);
        let pool_a = make_pool_id(1, ProtocolType::UniswapV2);
        let pool_b = make_pool_id(2, ProtocolType::SushiSwap);

        g.add_edge(
            0,
            1,
            2.0,
            pool_a,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.add_edge(
            2,
            3,
            1.5,
            pool_b,
            Address::repeat_byte(2),
            ProtocolType::SushiSwap,
            U256::from(500u64),
        );

        let mut affected = g.affected_vertices();
        affected.sort_unstable();
        assert_eq!(affected, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_affected_vertices_after_clear() {
        let mut g = PriceGraph::new(5);
        let pool_a = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            2.0,
            pool_a,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.clear_dirty();
        assert!(g.affected_vertices().is_empty());
    }

    #[test]
    fn test_remove_pool_edges() {
        let mut g = PriceGraph::new(4);
        let pool_a = make_pool_id(1, ProtocolType::UniswapV2);
        let pool_b = make_pool_id(2, ProtocolType::SushiSwap);

        // Pool A has two edges: 0->1 and 1->0.
        g.add_edge(
            0,
            1,
            2.0,
            pool_a,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.add_edge(
            1,
            0,
            0.5,
            pool_a,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        // Pool B has one edge: 2->3.
        g.add_edge(
            2,
            3,
            1.5,
            pool_b,
            Address::repeat_byte(2),
            ProtocolType::SushiSwap,
            U256::from(500u64),
        );

        assert_eq!(g.num_edges(), 3);

        g.remove_pool_edges(&pool_a);

        assert_eq!(g.num_edges(), 1);
        assert!(g.edges_from(0).is_empty());
        assert!(g.edges_from(1).is_empty());
        assert_eq!(g.edges_from(2).len(), 1);
        assert_eq!(g.all_edges()[0].pool_id, pool_b);

        // Dirty flags should be rebuilt (all false).
        assert!(!g.has_dirty_edges());
        assert_eq!(g.dirty.len(), 1);
    }

    #[test]
    fn test_resize_grow() {
        let mut g = PriceGraph::new(3);
        assert_eq!(g.num_vertices(), 3);

        g.resize(10);
        assert_eq!(g.num_vertices(), 10);
        // Should be able to add edges to new vertices.
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            7,
            8,
            1.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(100u64),
        );
        assert_eq!(g.edges_from(7).len(), 1);
    }

    #[test]
    fn test_resize_no_shrink() {
        let mut g = PriceGraph::new(10);
        g.resize(5); // Should be a no-op.
        assert_eq!(g.num_vertices(), 10);
    }

    #[test]
    fn test_edges_from_out_of_bounds() {
        let g = PriceGraph::new(3);
        assert!(g.edges_from(100).is_empty());
    }

    #[test]
    fn test_negative_weight_for_profitable_rate() {
        // rate > 1.0 => -ln(rate) < 0 => negative weight => profitable
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            1.5,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );

        let edge = &g.all_edges()[0];
        assert!(edge.weight < 0.0, "rate > 1 should yield negative weight");
    }

    #[test]
    fn test_positive_weight_for_unfavorable_rate() {
        // rate < 1.0 => -ln(rate) > 0 => positive weight => not immediately profitable
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            0.5,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );

        let edge = &g.all_edges()[0];
        assert!(edge.weight > 0.0, "rate < 1 should yield positive weight");
    }

    #[test]
    fn test_negative_cycle_detection_setup() {
        // Set up a triangular arbitrage: A->B->C->A where the product of
        // rates > 1.0, meaning sum of weights < 0 (negative cycle).
        let mut g = PriceGraph::new(3);
        let pool_ab = make_pool_id(1, ProtocolType::UniswapV2);
        let pool_bc = make_pool_id(2, ProtocolType::SushiSwap);
        let pool_ca = make_pool_id(3, ProtocolType::Curve);

        // A->B: rate=1.1, B->C: rate=1.1, C->A: rate=1.1
        // Product = 1.331 > 1 => sum of -ln weights < 0
        g.add_edge(
            0,
            1,
            1.1,
            pool_ab,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );
        g.add_edge(
            1,
            2,
            1.1,
            pool_bc,
            Address::repeat_byte(2),
            ProtocolType::SushiSwap,
            U256::from(1_000u64),
        );
        g.add_edge(
            2,
            0,
            1.1,
            pool_ca,
            Address::repeat_byte(3),
            ProtocolType::Curve,
            U256::from(1_000u64),
        );

        let total_weight: f64 = g.all_edges().iter().map(|e| e.weight).sum();
        assert!(
            total_weight < 0.0,
            "triangular arb with all rates > 1 should have negative cycle weight: {total_weight}"
        );
    }

    #[test]
    fn test_edge_index_consistency() {
        let mut g = PriceGraph::new(10);
        let protocols = [
            ProtocolType::UniswapV2,
            ProtocolType::SushiSwap,
            ProtocolType::Curve,
        ];

        // Add many edges across different pools and vertices.
        for i in 0..8 {
            for (k, &proto) in protocols.iter().enumerate() {
                let j = (i + k + 1) % 10;
                let pid = make_pool_id(((i * 3 + k) % 255) as u8, proto);
                g.add_edge(
                    i,
                    j,
                    1.05 + (k as f64) * 0.01,
                    pid,
                    Address::repeat_byte(((i * 3 + k) % 255) as u8),
                    proto,
                    U256::from(1_000u64),
                );
            }
        }

        // Verify every edge in all_edges is correctly indexed.
        for (idx, edge) in g.all_edges().iter().enumerate() {
            let key = (edge.from, edge.to, edge.pool_id);
            assert_eq!(
                g.edge_index.get(&key).copied(),
                Some(idx),
                "edge_index mismatch at all_edges[{idx}]"
            );
        }

        // After removing a pool, the index must still be consistent.
        let removed_pool = make_pool_id(0, ProtocolType::UniswapV2);
        g.remove_pool_edges(&removed_pool);

        for (idx, edge) in g.all_edges().iter().enumerate() {
            let key = (edge.from, edge.to, edge.pool_id);
            assert_eq!(
                g.edge_index.get(&key).copied(),
                Some(idx),
                "edge_index mismatch after remove at all_edges[{idx}]"
            );
        }
        // Removed pool should not appear in the index.
        assert!(!g
            .edge_index
            .keys()
            .any(|(_, _, pid)| pid == &removed_pool));
    }

    #[test]
    fn test_edge_index_update_preserves_index() {
        let mut g = PriceGraph::new(3);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(
            0,
            1,
            2.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1_000u64),
        );

        let idx_before = g.edge_index[&(0, 1, pool_id)];

        // Update the same edge -- index should not change.
        g.add_edge(
            0,
            1,
            3.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(2_000u64),
        );

        assert_eq!(g.edge_index[&(0, 1, pool_id)], idx_before);
        assert_eq!(g.num_edges(), 1);
    }

    #[test]
    fn test_dirty_edge_indices_multiple() {
        let mut g = PriceGraph::new(5);
        let p1 = make_pool_id(1, ProtocolType::UniswapV2);
        let p2 = make_pool_id(2, ProtocolType::SushiSwap);
        let p3 = make_pool_id(3, ProtocolType::Curve);

        g.add_edge(0, 1, 2.0, p1, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(100u64));
        g.add_edge(1, 2, 1.5, p2, Address::repeat_byte(2), ProtocolType::SushiSwap, U256::from(200u64));
        g.add_edge(2, 3, 1.2, p3, Address::repeat_byte(3), ProtocolType::Curve, U256::from(300u64));

        // All three should be dirty.
        let mut indices = g.dirty_edge_indices();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2]);

        g.clear_dirty();
        assert!(g.dirty_edge_indices().is_empty());

        // Update only the second edge.
        g.update_edge_from_reserves(1, 2, p2, 500.0, 800.0, 0.997);
        assert_eq!(g.dirty_edge_indices(), vec![1]);
    }
}
