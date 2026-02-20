use crate::opportunity::DetectedCycle;
use aether_state::price_graph::PriceGraph;
use std::collections::VecDeque;

/// Bellman-Ford with SPFA (Shortest Path Faster Algorithm) and SLF (Small Label
/// First) optimization for detecting negative-weight cycles in the price graph.
///
/// # Algorithm overview
///
/// Standard Bellman-Ford relaxes *all* edges in each of N-1 iterations (O(VE)).
/// SPFA improves this by maintaining a queue of vertices whose distances were
/// actually updated — only their outgoing edges are relaxed. In practice this
/// reduces work dramatically on sparse graphs.
///
/// SLF further optimizes by adding a vertex to the *front* of the queue when its
/// new distance is smaller than the current front's distance (similar to a
/// priority deque), which tends to process more promising vertices first.
///
/// A negative cycle is detected when a vertex has been relaxed at least N times,
/// meaning a shorter path keeps being found — only possible with a negative
/// cycle.
pub struct BellmanFord {
    /// Maximum number of hops allowed in an arbitrage path.
    max_hops: usize,
    /// Maximum time budget in microseconds (early exit for latency control).
    max_time_us: u64,
}

impl BellmanFord {
    pub fn new(max_hops: usize, max_time_us: u64) -> Self {
        Self {
            max_hops,
            max_time_us,
        }
    }

    /// Detect all negative cycles reachable from any source vertex.
    ///
    /// Uses a virtual super-source connected to all vertices with weight 0.
    /// Returns detected cycles sorted by `total_weight` ascending (most
    /// profitable first).
    pub fn detect_negative_cycles(&self, graph: &PriceGraph) -> Vec<DetectedCycle> {
        let n = graph.num_vertices();
        if n == 0 {
            return vec![];
        }

        let mut dist = vec![0.0f64; n]; // Virtual source at distance 0
        let mut predecessor: Vec<Option<usize>> = vec![None; n];
        let mut in_queue = vec![false; n];
        let mut relaxation_count = vec![0u32; n];

        let mut queue: VecDeque<usize> = VecDeque::new();

        // Seed all vertices (as if connected to a virtual source with weight 0)
        for (i, flag) in in_queue.iter_mut().enumerate() {
            queue.push_back(i);
            *flag = true;
        }

        let mut cycles = Vec::new();
        let mut visited_cycle_nodes = vec![false; n];
        let start = std::time::Instant::now();

        while let Some(u) = queue.pop_front() {
            in_queue[u] = false;

            // Time budget check
            if start.elapsed().as_micros() as u64 > self.max_time_us {
                break;
            }

            for edge in graph.edges_from(u) {
                let v = edge.to;
                let new_dist = dist[u] + edge.weight;

                if new_dist < dist[v] - 1e-10 {
                    dist[v] = new_dist;
                    predecessor[v] = Some(u);
                    relaxation_count[v] += 1;

                    // Negative cycle detected: vertex relaxed N times
                    if relaxation_count[v] >= n as u32 {
                        if !visited_cycle_nodes[v] {
                            if let Some(cycle) =
                                self.extract_cycle(v, &predecessor, graph, n)
                            {
                                for &node in &cycle.path {
                                    visited_cycle_nodes[node] = true;
                                }
                                cycles.push(cycle);
                            }
                        }
                        continue;
                    }

                    if !in_queue[v] {
                        // SLF: add to front if distance < front's distance
                        if let Some(&front) = queue.front() {
                            if dist[v] < dist[front] {
                                queue.push_front(v);
                            } else {
                                queue.push_back(v);
                            }
                        } else {
                            queue.push_back(v);
                        }
                        in_queue[v] = true;
                    }
                }
            }
        }

        // Sort by total weight (most negative = most profitable first)
        cycles.sort_by(|a, b| {
            a.total_weight
                .partial_cmp(&b.total_weight)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        cycles
    }

    /// Extract the actual cycle from the predecessor array.
    ///
    /// Starting from a vertex known to be in a negative cycle, we walk back N
    /// steps through predecessors to guarantee we land inside the cycle, then
    /// trace until we revisit the same vertex.
    fn extract_cycle(
        &self,
        start: usize,
        predecessor: &[Option<usize>],
        graph: &PriceGraph,
        n: usize,
    ) -> Option<DetectedCycle> {
        // Walk back N steps to ensure we are on the cycle
        let mut v = start;
        for _ in 0..n {
            v = predecessor[v]?;
        }

        // Trace the cycle
        let cycle_start = v;
        let mut path = vec![cycle_start];
        let mut current = predecessor[cycle_start]?;

        let mut safety = 0;
        while current != cycle_start && safety < n {
            path.push(current);
            current = predecessor[current]?;
            safety += 1;
        }

        if current != cycle_start {
            return None;
        }

        path.reverse();
        // Close the cycle: the first element of the reversed path is where the
        // forward traversal starts, so we append it to close the loop.
        let cycle_close = path[0];
        path.push(cycle_close);

        // Filter by max hops
        if path.len() - 1 > self.max_hops {
            return None;
        }

        // Calculate actual total weight by summing edge weights along the cycle
        let mut total_weight = 0.0;
        for i in 0..path.len() - 1 {
            let from = path[i];
            let to = path[i + 1];
            // Find the best (lowest weight) edge between these vertices
            if let Some(best_edge) = graph
                .edges_from(from)
                .iter()
                .filter(|e| e.to == to)
                .min_by(|a, b| {
                    a.weight
                        .partial_cmp(&b.weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                total_weight += best_edge.weight;
            } else {
                return None; // Edge not found — invalid cycle
            }
        }

        Some(DetectedCycle { path, total_weight })
    }

    /// Detect negative cycles only from affected vertices (partial scan).
    ///
    /// After a state update, only the edges around changed pools need
    /// re-evaluation. This seeds the SPFA queue with only the `affected`
    /// vertices rather than the entire graph, significantly reducing work
    /// when few pools update per block.
    pub fn detect_from_affected(
        &self,
        graph: &PriceGraph,
        affected: &[usize],
    ) -> Vec<DetectedCycle> {
        if affected.is_empty() {
            return vec![];
        }

        let n = graph.num_vertices();
        if n == 0 {
            return vec![];
        }

        let mut dist = vec![f64::INFINITY; n];
        let mut predecessor: Vec<Option<usize>> = vec![None; n];
        let mut in_queue = vec![false; n];
        let mut relaxation_count = vec![0u32; n];

        let mut queue: VecDeque<usize> = VecDeque::new();

        // Only seed with affected vertices
        for &v in affected {
            if v < n {
                dist[v] = 0.0;
                queue.push_back(v);
                in_queue[v] = true;
            }
        }

        let mut cycles = Vec::new();
        let mut visited_cycle_nodes = vec![false; n];
        let start = std::time::Instant::now();

        while let Some(u) = queue.pop_front() {
            in_queue[u] = false;

            if start.elapsed().as_micros() as u64 > self.max_time_us {
                break;
            }

            for edge in graph.edges_from(u) {
                let v = edge.to;
                let new_dist = dist[u] + edge.weight;

                if new_dist < dist[v] - 1e-10 {
                    dist[v] = new_dist;
                    predecessor[v] = Some(u);
                    relaxation_count[v] += 1;

                    if relaxation_count[v] >= n as u32 {
                        if !visited_cycle_nodes[v] {
                            if let Some(cycle) =
                                self.extract_cycle(v, &predecessor, graph, n)
                            {
                                for &node in &cycle.path {
                                    visited_cycle_nodes[node] = true;
                                }
                                cycles.push(cycle);
                            }
                        }
                        continue;
                    }

                    if !in_queue[v] {
                        if let Some(&front) = queue.front() {
                            if dist[v] < dist[front] {
                                queue.push_front(v);
                            } else {
                                queue.push_back(v);
                            }
                        } else {
                            queue.push_back(v);
                        }
                        in_queue[v] = true;
                    }
                }
            }
        }

        cycles.sort_by(|a, b| {
            a.total_weight
                .partial_cmp(&b.total_weight)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        cycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_common::types::{PoolId, ProtocolType};
    use alloy::primitives::{Address, U256};

    fn make_pool_id(byte: u8, protocol: ProtocolType) -> PoolId {
        PoolId {
            address: Address::repeat_byte(byte),
            protocol,
        }
    }

    /// Helper: build a graph with no negative cycle (all rates < 1 or just = 1).
    fn build_no_cycle_graph() -> PriceGraph {
        let mut g = PriceGraph::new(3);
        let p01 = make_pool_id(1, ProtocolType::UniswapV2);
        let p12 = make_pool_id(2, ProtocolType::SushiSwap);
        let p20 = make_pool_id(3, ProtocolType::Curve);

        // rates: 0.9, 0.9, 0.9 => product = 0.729 < 1 => positive cycle weight
        g.add_edge(0, 1, 0.9, p01, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(1, 2, 0.9, p12, Address::repeat_byte(2), ProtocolType::SushiSwap, U256::from(1000u64));
        g.add_edge(2, 0, 0.9, p20, Address::repeat_byte(3), ProtocolType::Curve, U256::from(1000u64));
        g
    }

    /// Helper: build a graph with a triangle arbitrage cycle.
    fn build_triangle_arb_graph() -> PriceGraph {
        let mut g = PriceGraph::new(3);
        let p01 = make_pool_id(1, ProtocolType::UniswapV2);
        let p12 = make_pool_id(2, ProtocolType::SushiSwap);
        let p20 = make_pool_id(3, ProtocolType::Curve);

        // rates: 1.1, 1.1, 1.1 => product = 1.331 > 1 => negative cycle
        g.add_edge(0, 1, 1.1, p01, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(1, 2, 1.1, p12, Address::repeat_byte(2), ProtocolType::SushiSwap, U256::from(1000u64));
        g.add_edge(2, 0, 1.1, p20, Address::repeat_byte(3), ProtocolType::Curve, U256::from(1000u64));
        g
    }

    // --------------- No-cycle detection ---------------

    #[test]
    fn test_no_cycles_in_positive_graph() {
        let graph = build_no_cycle_graph();
        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&graph);
        assert!(cycles.is_empty(), "should find no negative cycles in a graph with all rates < 1");
    }

    #[test]
    fn test_empty_graph() {
        let graph = PriceGraph::new(0);
        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&graph);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_single_vertex_no_edges() {
        let graph = PriceGraph::new(1);
        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&graph);
        assert!(cycles.is_empty());
    }

    // --------------- Triangle arb ---------------

    #[test]
    fn test_detect_triangle_arb() {
        let graph = build_triangle_arb_graph();
        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&graph);

        assert!(!cycles.is_empty(), "should detect the triangular arb cycle");

        let cycle = &cycles[0];
        assert!(cycle.is_profitable());
        assert_eq!(cycle.num_hops(), 3);

        // Verify the path forms a valid closed loop
        assert_eq!(cycle.path.first(), cycle.path.last());

        // Verify total weight is negative
        let expected_weight = 3.0 * (-(1.1_f64).ln());
        assert!(
            (cycle.total_weight - expected_weight).abs() < 1e-6,
            "total_weight {} should be close to {}",
            cycle.total_weight,
            expected_weight
        );
    }

    #[test]
    fn test_cycle_path_is_closed() {
        let graph = build_triangle_arb_graph();
        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&graph);

        for cycle in &cycles {
            assert!(
                cycle.path.len() >= 2,
                "cycle path should have at least 2 entries"
            );
            assert_eq!(
                cycle.path.first(),
                cycle.path.last(),
                "cycle must be closed"
            );
        }
    }

    // --------------- Multi-hop cycle ---------------

    #[test]
    fn test_four_hop_cycle() {
        let mut g = PriceGraph::new(4);
        let p01 = make_pool_id(1, ProtocolType::UniswapV2);
        let p12 = make_pool_id(2, ProtocolType::UniswapV3);
        let p23 = make_pool_id(3, ProtocolType::SushiSwap);
        let p30 = make_pool_id(4, ProtocolType::Curve);

        // rates: 1.08 each => product = 1.08^4 ~ 1.3605 > 1
        g.add_edge(0, 1, 1.08, p01, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(1, 2, 1.08, p12, Address::repeat_byte(2), ProtocolType::UniswapV3, U256::from(1000u64));
        g.add_edge(2, 3, 1.08, p23, Address::repeat_byte(3), ProtocolType::SushiSwap, U256::from(1000u64));
        g.add_edge(3, 0, 1.08, p30, Address::repeat_byte(4), ProtocolType::Curve, U256::from(1000u64));

        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&g);

        assert!(!cycles.is_empty(), "should detect 4-hop cycle");
        let cycle = &cycles[0];
        assert!(cycle.is_profitable());
        assert_eq!(cycle.num_hops(), 4);
    }

    #[test]
    fn test_max_hops_filter() {
        // Same 4-hop cycle, but limit max_hops to 3
        let mut g = PriceGraph::new(4);
        let p01 = make_pool_id(1, ProtocolType::UniswapV2);
        let p12 = make_pool_id(2, ProtocolType::UniswapV3);
        let p23 = make_pool_id(3, ProtocolType::SushiSwap);
        let p30 = make_pool_id(4, ProtocolType::Curve);

        g.add_edge(0, 1, 1.08, p01, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(1, 2, 1.08, p12, Address::repeat_byte(2), ProtocolType::UniswapV3, U256::from(1000u64));
        g.add_edge(2, 3, 1.08, p23, Address::repeat_byte(3), ProtocolType::SushiSwap, U256::from(1000u64));
        g.add_edge(3, 0, 1.08, p30, Address::repeat_byte(4), ProtocolType::Curve, U256::from(1000u64));

        let bf = BellmanFord::new(3, 1_000_000); // max 3 hops
        let cycles = bf.detect_negative_cycles(&g);

        // The 4-hop cycle should be filtered out
        for cycle in &cycles {
            assert!(
                cycle.num_hops() <= 3,
                "no cycle should exceed max_hops=3, got {}",
                cycle.num_hops()
            );
        }
    }

    // --------------- Affected vertex scan ---------------

    #[test]
    fn test_detect_from_affected_finds_cycle() {
        let graph = build_triangle_arb_graph();
        let bf = BellmanFord::new(6, 1_000_000);

        // Seed from vertex 0, which is part of the cycle
        let cycles = bf.detect_from_affected(&graph, &[0]);
        assert!(
            !cycles.is_empty(),
            "should detect cycle when seeded from affected vertex in the cycle"
        );
    }

    #[test]
    fn test_detect_from_affected_empty() {
        let graph = build_triangle_arb_graph();
        let bf = BellmanFord::new(6, 1_000_000);

        let cycles = bf.detect_from_affected(&graph, &[]);
        assert!(cycles.is_empty(), "empty affected set should yield no cycles");
    }

    #[test]
    fn test_detect_from_affected_no_cycle() {
        let graph = build_no_cycle_graph();
        let bf = BellmanFord::new(6, 1_000_000);

        let cycles = bf.detect_from_affected(&graph, &[0, 1, 2]);
        assert!(
            cycles.is_empty(),
            "no cycle should be found in positive-weight graph"
        );
    }

    #[test]
    fn test_detect_from_affected_out_of_bounds() {
        let graph = build_triangle_arb_graph();
        let bf = BellmanFord::new(6, 1_000_000);

        // Vertex 999 does not exist — should not panic
        let cycles = bf.detect_from_affected(&graph, &[999]);
        // May or may not find cycles depending on whether any valid vertex
        // gets enqueued. The important thing is no panic.
        let _ = cycles;
    }

    // --------------- Early exit on time budget ---------------

    #[test]
    fn test_early_exit_on_time_budget() {
        // Build a large-ish graph
        let n = 50;
        let mut g = PriceGraph::new(n);
        for i in 0..n {
            let j = (i + 1) % n;
            let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
            g.add_edge(
                i,
                j,
                1.001,
                pid,
                Address::repeat_byte((i % 255) as u8),
                ProtocolType::UniswapV2,
                U256::from(1000u64),
            );
        }

        // Very tight time budget: 1 microsecond
        let bf = BellmanFord::new(6, 1);
        // Should not hang; may or may not find cycles
        let _cycles = bf.detect_negative_cycles(&g);
        // The test passes as long as it completes quickly
    }

    // --------------- Parallel edges (different pools, same pair) ---------------

    #[test]
    fn test_parallel_edges_best_rate_used() {
        let mut g = PriceGraph::new(3);

        // Two pools for edge 0->1: one with bad rate, one with good rate
        let pool_bad = make_pool_id(1, ProtocolType::UniswapV2);
        let pool_good = make_pool_id(2, ProtocolType::SushiSwap);
        let p12 = make_pool_id(3, ProtocolType::Curve);
        let p20 = make_pool_id(4, ProtocolType::BalancerV2);

        g.add_edge(0, 1, 0.5, pool_bad, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(0, 1, 2.0, pool_good, Address::repeat_byte(2), ProtocolType::SushiSwap, U256::from(1000u64));
        g.add_edge(1, 2, 1.5, p12, Address::repeat_byte(3), ProtocolType::Curve, U256::from(1000u64));
        g.add_edge(2, 0, 1.5, p20, Address::repeat_byte(4), ProtocolType::BalancerV2, U256::from(1000u64));

        // Product via good pool: 2.0 * 1.5 * 1.5 = 4.5 > 1 => negative cycle exists
        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&g);
        assert!(!cycles.is_empty(), "should find cycle using the better-rate parallel edge");
    }

    // --------------- Two-hop cycle ---------------

    #[test]
    fn test_two_hop_cycle() {
        let mut g = PriceGraph::new(2);
        let p01 = make_pool_id(1, ProtocolType::UniswapV2);
        let p10 = make_pool_id(2, ProtocolType::SushiSwap);

        // 0->1 rate 1.5, 1->0 rate 1.5 => product 2.25 > 1
        g.add_edge(0, 1, 1.5, p01, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(1, 0, 1.5, p10, Address::repeat_byte(2), ProtocolType::SushiSwap, U256::from(1000u64));

        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&g);
        assert!(!cycles.is_empty(), "should detect 2-hop cycle");
        assert!(cycles[0].is_profitable());
    }

    // --------------- Sorted by profitability ---------------

    #[test]
    fn test_cycles_sorted_most_profitable_first() {
        // Build two separate cycles with different profitability
        let mut g = PriceGraph::new(6);

        // Cycle 1: 0->1->2->0 with moderate profit (rate 1.05)
        let p01 = make_pool_id(1, ProtocolType::UniswapV2);
        let p12 = make_pool_id(2, ProtocolType::SushiSwap);
        let p20 = make_pool_id(3, ProtocolType::Curve);
        g.add_edge(0, 1, 1.05, p01, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(1, 2, 1.05, p12, Address::repeat_byte(2), ProtocolType::SushiSwap, U256::from(1000u64));
        g.add_edge(2, 0, 1.05, p20, Address::repeat_byte(3), ProtocolType::Curve, U256::from(1000u64));

        // Cycle 2: 3->4->5->3 with higher profit (rate 1.2)
        let p34 = make_pool_id(4, ProtocolType::BalancerV2);
        let p45 = make_pool_id(5, ProtocolType::BancorV3);
        let p53 = make_pool_id(6, ProtocolType::UniswapV3);
        g.add_edge(3, 4, 1.2, p34, Address::repeat_byte(4), ProtocolType::BalancerV2, U256::from(1000u64));
        g.add_edge(4, 5, 1.2, p45, Address::repeat_byte(5), ProtocolType::BancorV3, U256::from(1000u64));
        g.add_edge(5, 3, 1.2, p53, Address::repeat_byte(6), ProtocolType::UniswapV3, U256::from(1000u64));

        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&g);

        if cycles.len() >= 2 {
            // Most profitable (most negative weight) should come first
            assert!(
                cycles[0].total_weight <= cycles[1].total_weight,
                "cycles should be sorted with most profitable first"
            );
        }
    }

    // --------------- Graph with no edges ---------------

    #[test]
    fn test_graph_vertices_no_edges() {
        let graph = PriceGraph::new(10);
        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&graph);
        assert!(cycles.is_empty());
    }

    // --------------- Disconnected components ---------------

    #[test]
    fn test_disconnected_components() {
        let mut g = PriceGraph::new(6);

        // Component 1: no cycle (0->1->2, no back edge)
        let p01 = make_pool_id(1, ProtocolType::UniswapV2);
        let p12 = make_pool_id(2, ProtocolType::SushiSwap);
        g.add_edge(0, 1, 1.5, p01, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1000u64));
        g.add_edge(1, 2, 1.5, p12, Address::repeat_byte(2), ProtocolType::SushiSwap, U256::from(1000u64));

        // Component 2: has a cycle (3->4->5->3)
        let p34 = make_pool_id(3, ProtocolType::Curve);
        let p45 = make_pool_id(4, ProtocolType::BalancerV2);
        let p53 = make_pool_id(5, ProtocolType::BancorV3);
        g.add_edge(3, 4, 1.2, p34, Address::repeat_byte(3), ProtocolType::Curve, U256::from(1000u64));
        g.add_edge(4, 5, 1.2, p45, Address::repeat_byte(4), ProtocolType::BalancerV2, U256::from(1000u64));
        g.add_edge(5, 3, 1.2, p53, Address::repeat_byte(5), ProtocolType::BancorV3, U256::from(1000u64));

        let bf = BellmanFord::new(6, 1_000_000);
        let cycles = bf.detect_negative_cycles(&g);

        // Should find the cycle in component 2
        assert!(!cycles.is_empty(), "should detect cycle in disconnected component");
        for cycle in &cycles {
            // All cycle vertices should be in component 2 (vertices 3, 4, 5)
            for &v in &cycle.path {
                assert!(
                    v >= 3 && v <= 5,
                    "cycle vertex {} should be in component 2 (3-5)",
                    v
                );
            }
        }
    }
}
