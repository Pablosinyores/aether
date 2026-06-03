use std::collections::HashMap;

use alloy::primitives::{Address, U256};
use aether_common::types::{PoolId, ProtocolType};

/// Default ERC20 token decimals assumed for any vertex whose decimals have not
/// been explicitly set. Most ERC20 tokens use 18 decimals.
const DEFAULT_TOKEN_DECIMALS: u8 = 18;

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
    /// Pool reserve on the input side (f64 approx). Zero when reserves are unknown
    /// (e.g. V3 edges set via spot price, or placeholder edges).
    pub reserve_in: f64,
    /// Pool reserve on the output side (f64 approx). Zero when reserves are unknown.
    pub reserve_out: f64,
    /// `true` when this edge is excluded from arbitrage detection because the
    /// backing pool's liquidity is below the configured minimum-liquidity floor
    /// (see [`PriceGraph::set_min_liquidity_weth`]) OR it was quarantined as a
    /// never-updated placeholder (see [`PriceGraph::quarantine_stale_edges`]).
    /// Bellman-Ford skips filtered edges during relaxation, so dead/drained
    /// pools never produce phantom cycles.
    pub filtered: bool,
    /// Chain block height at which this edge last received a real reserve
    /// update via [`update_edge_from_reserves`]. `0` = never updated — i.e. a
    /// placeholder edge from [`add_edge`] that carries no real reserves.
    ///
    /// NOTE: for an AMM, an edge with no *recent* update is NOT stale — a quiet
    /// pool's cached reserves are still exact, because reserves only change when
    /// the pool trades (and a trade emits the event that updates this edge). So
    /// raw age is a liveness signal, not a corruption signal;
    /// [`PriceGraph::quarantine_stale_edges`] treats it as an opt-in
    /// missed-event backstop only, never the primary filter.
    pub last_update_block: u64,
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
    /// ERC20 decimals per token vertex. Used to convert raw base-unit reserve
    /// ratios into human-unit exchange rates in [`update_edge_from_reserves`].
    /// Defaults to 18 (the ERC20 default) for any vertex not explicitly set.
    token_decimals: Vec<u8>,
    /// Graph vertex index of WETH, when known. The minimum-liquidity floor uses
    /// the WETH-side reserve of a WETH-paired pool as a no-oracle TVL proxy.
    /// `None` disables the floor entirely (backward-compatible: synthetic graphs
    /// that never register WETH behave exactly as before).
    weth_vertex: Option<usize>,
    /// Minimum WETH-side human reserve (≈ half a constant-product pool's TVL)
    /// for a WETH-paired pool's edges to participate in detection. `0.0` (the
    /// default) disables the floor.
    min_liquidity_weth: f64,
    /// Most recent chain block the writer has applied updates for. Stamped onto
    /// each edge by [`update_edge_from_reserves`] and read by
    /// [`quarantine_stale_edges`]. `0` until the writer calls
    /// [`set_current_block`]; while `0`, edge age is unknown so the age backstop
    /// is inert (placeholder quarantine still works — it is block-independent).
    current_block: u64,
}

impl PriceGraph {
    /// Create a new empty price graph with the given number of token vertices.
    pub fn new(num_vertices: usize) -> Self {
        Self {
            num_vertices,
            edges: vec![Vec::new(); num_vertices],
            all_edges: Vec::new(),
            edge_index: HashMap::with_capacity(num_vertices * 8),
            dirty: Vec::new(),
            token_decimals: vec![DEFAULT_TOKEN_DECIMALS; num_vertices],
            weth_vertex: None,
            min_liquidity_weth: 0.0,
            current_block: 0,
        }
    }

    /// Add or update an edge in the graph.
    ///
    /// `exchange_rate`: how many units of `to` token you receive per unit of
    /// `from` token. The stored weight is `-ln(exchange_rate)`.
    ///
    /// If an edge with the same `(from, to, pool_id)` already exists it is
    /// updated in place; otherwise a new edge is appended.
    ///
    /// NOTE: `add_edge` does **not** apply token-decimal correction. Decimal
    /// normalization of the raw reserve ratio lives in
    /// [`update_edge_from_reserves`]. Callers that want a live, decimal-correct
    /// rate must either pass a neutral `1.0` placeholder here (decimal-neutral,
    /// since `ln(1) = 0`) or follow this call with
    /// [`update_edge_from_reserves`], which is the single source of truth for
    /// decimal-aware rates.
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
            reserve_in: 0.0,
            reserve_out: 0.0,
            // Placeholder edges carry no real reserves and are never floored;
            // the follow-up `update_edge_from_reserves` sets the real flag.
            filtered: false,
            // 0 = never updated. `quarantine_stale_edges` filters such edges
            // until `update_edge_from_reserves` stamps a real block.
            last_update_block: 0,
        };

        // Try to update an existing edge with matching (from, to, pool_id).
        if let Some(existing) = self.edges[from]
            .iter_mut()
            .find(|e| e.to == to && e.pool_id == pool_id)
        {
            existing.weight = weight;
            existing.liquidity = liquidity;
            // Mirror the update in the flat edge list via O(1) index lookup.
            // Direct indexing: panics if the key is missing, which is correct —
            // the adjacency list found the edge, so edge_index must agree.
            let idx = self.edge_index[&(from, to, pool_id)];
            self.all_edges[idx].weight = weight;
            self.all_edges[idx].liquidity = liquidity;
            self.dirty[idx] = true;
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
    /// `reserve_in` / `reserve_out` are expressed in raw on-chain base units, so
    /// the ratio `reserve_out / reserve_in` is in *output base units per input
    /// base unit*. To obtain a human-unit exchange rate the ratio is scaled by
    /// `10^(dec_in - dec_out)`, where `dec_in`/`dec_out` are the ERC20 decimals
    /// of the input/output token vertices. Without this correction a
    /// decimal-mismatched pair (e.g. USDC 6 / WETH 18) would carry an
    /// uncancelled `10^12` factor into the edge weight.
    ///
    /// This single correction is correct for **both** V2 (real reserves) and
    /// V3 (virtual constant-product reserves `(x_v, y_v)` derived from L +
    /// sqrtPrice; see `aether_pools::uniswap_v3::virtual_reserves`), since in
    /// both cases the ratio is output-base-per-input-base. V3 is not
    /// special-cased.
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
        // Capture before the mutable edge borrow below (can't read &self while
        // self.edges is borrowed mutably).
        let cur_block = self.current_block;
        let dec_in = self
            .token_decimals
            .get(from)
            .copied()
            .unwrap_or(DEFAULT_TOKEN_DECIMALS) as i32;
        let dec_out = self
            .token_decimals
            .get(to)
            .copied()
            .unwrap_or(DEFAULT_TOKEN_DECIMALS) as i32;
        let rate = (reserve_out / reserve_in) * fee_factor * 10f64.powi(dec_in - dec_out);

        // Minimum-liquidity floor: exclude WETH-paired pools whose WETH-side
        // human reserve is below the configured floor. For a constant-product
        // pool both sides hold equal value, so the WETH-side reserve is a clean,
        // oracle-free TVL proxy. Pools where neither endpoint is WETH are NOT
        // subject to this floor (left to existing qualification gates). The
        // floor is disabled when `weth_vertex` is unknown or the floor is 0.0.
        //
        // UniswapV3 is excluded from this floor. V3 edges now carry virtual
        // constant-product reserves `(x_v, y_v)` derived from L + sqrtPrice
        // (`aether_pools::uniswap_v3::virtual_reserves`), but those virtual
        // reserves *overstate* true depth for concentrated liquidity (they
        // extrapolate the active-tick L across the whole price range), so the
        // WETH-side virtual reserve is not a conservative TVL proxy the way a
        // V2 pool's real WETH balance is. Flooring on it would let shallow
        // pools pass while risking false-filtering of legitimately deep ones;
        // V3 depth is instead validated downstream by the revm fork simulator
        // and the protocol-aware cycle-gating liquidity check.
        let filtered = match self.weth_vertex {
            Some(weth)
                if self.min_liquidity_weth > 0.0
                    && pool_id.protocol != ProtocolType::UniswapV3 =>
            {
                let weth_human = if from == weth {
                    Some(reserve_in / 10f64.powi(self.token_decimals(from) as i32))
                } else if to == weth {
                    Some(reserve_out / 10f64.powi(self.token_decimals(to) as i32))
                } else {
                    // Pool not WETH-paired → not subject to this floor.
                    None
                };
                match weth_human {
                    Some(human) => human < self.min_liquidity_weth,
                    None => false,
                }
            }
            _ => false,
        };

        if let Some(existing) = self.edges[from]
            .iter_mut()
            .find(|e| e.to == to && e.pool_id == pool_id)
        {
            existing.weight = -rate.ln();
            existing.reserve_in = reserve_in;
            existing.reserve_out = reserve_out;
            existing.filtered = filtered;
            existing.last_update_block = cur_block;
            // Mirror the update in the flat edge list via O(1) index lookup.
            // Direct indexing: panics if the key is missing, which is correct —
            // the adjacency list found the edge, so edge_index must agree.
            let idx = self.edge_index[&(from, to, pool_id)];
            self.all_edges[idx].weight = existing.weight;
            self.all_edges[idx].reserve_in = reserve_in;
            self.all_edges[idx].reserve_out = reserve_out;
            self.all_edges[idx].filtered = filtered;
            self.all_edges[idx].last_update_block = cur_block;
            self.dirty[idx] = true;
        }
    }

    /// Mark an edge's `filtered` flag without touching its weight or reserves.
    ///
    /// Used by the boot-time reserve fetcher to disable graph edges for pools
    /// whose RPC fetch failed (`getReserves`/`slot0` returned empty bytes, etc).
    /// Without this Bellman-Ford keeps traversing the placeholder rate=1.0 edge
    /// from [`Self::add_edge`] and synthesises phantom cycles by chaining the
    /// dead edge against any real edge between the same two vertices.
    ///
    /// No-op when no edge matches `(from, to, pool_id)`. Safe to call before or
    /// after [`Self::update_edge_from_reserves`]; the next reserve refresh will
    /// overwrite the flag with the min-liquidity-floor verdict.
    pub fn set_edge_filtered(
        &mut self,
        from: usize,
        to: usize,
        pool_id: PoolId,
        filtered: bool,
    ) {
        if let Some(existing) = self.edges[from]
            .iter_mut()
            .find(|e| e.to == to && e.pool_id == pool_id)
        {
            existing.filtered = filtered;
            let idx = self.edge_index[&(from, to, pool_id)];
            self.all_edges[idx].filtered = filtered;
            self.dirty[idx] = true;
        }
    }

    /// Record the chain block the writer is currently applying updates for.
    /// Subsequent [`update_edge_from_reserves`] calls stamp this onto each
    /// edge's `last_update_block`, and [`quarantine_stale_edges`] uses it to
    /// compute edge age. Set it once per block before applying that block's
    /// reserve updates.
    pub fn set_current_block(&mut self, block: u64) {
        self.current_block = block;
    }

    /// The most recent block recorded via [`set_current_block`] (`0` if unset).
    #[inline]
    pub fn current_block(&self) -> u64 {
        self.current_block
    }

    /// Quarantine stale / corrupt edges by setting their `filtered` flag so
    /// Bellman-Ford skips them — preventing the placeholder rate=1.0 edge from
    /// [`add_edge`] from chaining into phantom cycles (the same failure
    /// [`set_edge_filtered`] guards at boot).
    ///
    /// Two signals, deliberately asymmetric:
    ///
    /// * **Never-updated placeholders** (`reserve_in <= 0 || reserve_out <= 0`)
    ///   are ALWAYS quarantined. These carry no real reserves — they are the
    ///   genuine corruption source. This signal is block-independent and safe.
    ///
    /// * **Raw age** (`current_block - last_update_block > max_age_blocks`) is
    ///   an OPT-IN backstop, applied ONLY when `max_age_blocks > 0` and ONLY to
    ///   edges that already hold real reserves. It is OFF by default
    ///   (`max_age_blocks == 0`) because for an AMM a quiet pool's reserves stay
    ///   exact between trades — age means "no recent trade", not "wrong data".
    ///   Enable it only as a missed-event / lost-subscription safety net, where
    ///   dropping coverage on genuinely quiet pools is an acceptable trade.
    ///
    /// Quarantine is additive: it only ever SETS `filtered = true`; it never
    /// clears it. A subsequent [`update_edge_from_reserves`] re-derives
    /// `filtered` from the liquidity floor, so a quarantined edge that receives
    /// fresh reserves automatically heals. Returns the number of edges newly
    /// quarantined by this call.
    pub fn quarantine_stale_edges(&mut self, max_age_blocks: u64) -> usize {
        let cur = self.current_block;
        let mut count = 0usize;
        for idx in 0..self.all_edges.len() {
            let (from, to, pool_id, already, reserve_in, reserve_out, last_update) = {
                let e = &self.all_edges[idx];
                (
                    e.from,
                    e.to,
                    e.pool_id,
                    e.filtered,
                    e.reserve_in,
                    e.reserve_out,
                    e.last_update_block,
                )
            };
            if already {
                continue;
            }
            let is_placeholder = reserve_in <= 0.0 || reserve_out <= 0.0;
            let is_too_old = max_age_blocks > 0
                && cur > 0
                && last_update > 0
                && cur.saturating_sub(last_update) > max_age_blocks;
            if is_placeholder || is_too_old {
                self.all_edges[idx].filtered = true;
                self.dirty[idx] = true;
                if let Some(adj) = self.edges[from]
                    .iter_mut()
                    .find(|x| x.to == to && x.pool_id == pool_id)
                {
                    adj.filtered = true;
                }
                count += 1;
            }
        }
        count
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
        // Preserve dirty flags for surviving edges: carry each edge's dirty
        // state through the rebuild so that pending reserve updates from other
        // pools are not silently discarded.
        let old_dirty = std::mem::take(&mut self.dirty);
        let surviving: Vec<(PriceEdge, bool)> = self
            .all_edges
            .iter()
            .zip(old_dirty.iter())
            .filter(|(e, _)| &e.pool_id != pool_id)
            .map(|(e, &d)| (e.clone(), d))
            .collect();

        self.all_edges.clear();
        self.edge_index.clear();

        for (idx, (edge, was_dirty)) in surviving.into_iter().enumerate() {
            self.edge_index.insert((edge.from, edge.to, edge.pool_id), idx);
            self.all_edges.push(edge);
            self.dirty.push(was_dirty);
        }
    }

    /// Grow the graph to accommodate at least `new_size` vertices.
    /// Existing edges are preserved.
    pub fn resize(&mut self, new_size: usize) {
        if new_size > self.num_vertices {
            self.edges.resize(new_size, Vec::new());
            // Keep the per-vertex decimals table in lockstep with the vertex
            // count, defaulting new slots to the ERC20 default.
            self.token_decimals.resize(new_size, DEFAULT_TOKEN_DECIMALS);
            self.num_vertices = new_size;
        }
    }

    /// Set the ERC20 decimals for a token vertex. If `vertex` is beyond the
    /// current table length the table is grown (new slots default to 18) so the
    /// assignment always succeeds.
    pub fn set_token_decimals(&mut self, vertex: usize, decimals: u8) {
        if vertex >= self.token_decimals.len() {
            self.token_decimals
                .resize(vertex + 1, DEFAULT_TOKEN_DECIMALS);
        }
        self.token_decimals[vertex] = decimals;
    }

    /// Get the ERC20 decimals for a token vertex, or the ERC20 default (18) if
    /// the vertex has no explicit entry.
    #[inline]
    pub fn token_decimals(&self, vertex: usize) -> u8 {
        self.token_decimals
            .get(vertex)
            .copied()
            .unwrap_or(DEFAULT_TOKEN_DECIMALS)
    }

    /// Register the graph vertex index of WETH. Enables the WETH-denominated
    /// minimum-liquidity floor for WETH-paired pools (see
    /// [`Self::set_min_liquidity_weth`]). Must be set before reserves are seeded
    /// for the resulting `filtered` flags to be correct.
    pub fn set_weth_vertex(&mut self, vertex: usize) {
        self.weth_vertex = Some(vertex);
    }

    /// Set the minimum WETH-side human reserve a WETH-paired pool must hold for
    /// its edges to participate in detection. `0.0` disables the floor. The
    /// floor is only applied when [`Self::set_weth_vertex`] has also been set.
    pub fn set_min_liquidity_weth(&mut self, floor: f64) {
        self.min_liquidity_weth = floor;
    }

    /// The configured minimum WETH-side reserve floor (`0.0` when disabled).
    #[inline]
    pub fn min_liquidity_weth(&self) -> f64 {
        self.min_liquidity_weth
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
        // Clear dirty so pool B's edge starts clean.
        g.clear_dirty();

        // Mark pool B's edge dirty via a reserve update before removing pool A.
        g.update_edge_from_reserves(2, 3, pool_b, 1000.0, 2000.0, 0.997);
        assert!(g.has_dirty_edges(), "pool B edge should be dirty before removal");

        g.remove_pool_edges(&pool_a);

        assert_eq!(g.num_edges(), 1);
        assert!(g.edges_from(0).is_empty());
        assert!(g.edges_from(1).is_empty());
        assert_eq!(g.edges_from(2).len(), 1);
        assert_eq!(g.all_edges()[0].pool_id, pool_b);
        assert_eq!(g.dirty.len(), 1);

        // Dirty flag for pool B's surviving edge must be preserved.
        assert!(
            g.has_dirty_edges(),
            "pool B edge dirty flag must survive removal of pool A"
        );
        assert_eq!(g.dirty_edge_indices(), vec![0]);
    }

    #[test]
    fn test_remove_pool_edges_dirty_preserved_clean() {
        // When the surviving edge is clean before the removal it should remain clean.
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
            2,
            3,
            1.5,
            pool_b,
            Address::repeat_byte(2),
            ProtocolType::SushiSwap,
            U256::from(500u64),
        );
        g.clear_dirty();

        // Remove pool A while pool B's edge is clean.
        g.remove_pool_edges(&pool_a);

        assert_eq!(g.num_edges(), 1);
        assert!(!g.has_dirty_edges(), "clean edge should remain clean after removal");
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

    #[test]
    fn test_set_and_get_token_decimals() {
        let mut g = PriceGraph::new(3);
        // Defaults to 18 before any explicit set.
        assert_eq!(g.token_decimals(0), 18);
        assert_eq!(g.token_decimals(2), 18);
        // Out-of-range vertex also defaults to 18.
        assert_eq!(g.token_decimals(99), 18);

        g.set_token_decimals(0, 6);
        assert_eq!(g.token_decimals(0), 6);

        // Setting beyond current length grows the table with 18-fill.
        g.set_token_decimals(50, 8);
        assert_eq!(g.token_decimals(50), 8);
        assert_eq!(g.token_decimals(49), 18);
    }

    #[test]
    fn test_resize_extends_decimals_with_default() {
        let mut g = PriceGraph::new(3);
        g.set_token_decimals(1, 6);
        g.resize(10);
        // Existing decimals preserved across resize.
        assert_eq!(g.token_decimals(1), 6);
        // New slots default to 18.
        assert_eq!(g.token_decimals(7), 18);
    }

    #[test]
    fn test_decimal_correction_usdc_weth_v2() {
        // Realistic balanced mainnet-ish USDC/WETH V2 pool at ~3000 USDC/WETH.
        // vertex 0 = USDC (6 decimals), vertex 1 = WETH (18 decimals).
        // Reserves in raw base units:
        //   USDC: 3,000,000 * 1e6  = 3_000_000_000_000
        //   WETH: 1,000     * 1e18 = 1_000_000_000_000_000_000_000
        // Human price = 3,000,000 USDC / 1,000 WETH = 3000 USDC per WETH.
        let mut g = PriceGraph::new(2);
        g.set_token_decimals(0, 6); // USDC
        g.set_token_decimals(1, 18); // WETH

        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        // USDC -> WETH edge and WETH -> USDC edge.
        g.add_edge(
            0,
            1,
            1.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1u64),
        );
        g.add_edge(
            1,
            0,
            1.0,
            pool_id,
            Address::repeat_byte(1),
            ProtocolType::UniswapV2,
            U256::from(1u64),
        );

        let usdc_reserve = 3_000_000_000_000.0_f64; // 3e6 * 1e6
        let weth_reserve = 1_000_000_000_000_000_000_000.0_f64; // 1e3 * 1e18
        let fee = 0.997_f64;

        // USDC(0) -> WETH(1): rate ~ (1/3000) * fee in human units.
        g.update_edge_from_reserves(0, 1, pool_id, usdc_reserve, weth_reserve, fee);
        // WETH(1) -> USDC(0): rate ~ 3000 * fee in human units.
        g.update_edge_from_reserves(1, 0, pool_id, weth_reserve, usdc_reserve, fee);

        // Recover human rates from stored weights (-ln(rate)).
        let usdc_to_weth_rate = (-g.edges_from(0)[0].weight).exp();
        let weth_to_usdc_rate = (-g.edges_from(1)[0].weight).exp();

        // Each rate must be in a sane band, NOT off by ~1e12.
        let expected_usdc_to_weth = (1.0 / 3000.0) * fee;
        let expected_weth_to_usdc = 3000.0 * fee;
        assert!(
            (usdc_to_weth_rate - expected_usdc_to_weth).abs() / expected_usdc_to_weth < 1e-9,
            "USDC->WETH human rate off: got {usdc_to_weth_rate}, want ~{expected_usdc_to_weth}"
        );
        assert!(
            (weth_to_usdc_rate - expected_weth_to_usdc).abs() / expected_weth_to_usdc < 1e-9,
            "WETH->USDC human rate off: got {weth_to_usdc_rate}, want ~{expected_weth_to_usdc}"
        );

        // Round-trip product must be ~fee^2 (≈ 0.997^2 ≈ 0.994), NOT ~1e±12.
        let product = usdc_to_weth_rate * weth_to_usdc_rate;
        assert!(
            (product - fee * fee).abs() < 1e-9,
            "round-trip product should be ~fee^2 ({}), got {product}",
            fee * fee
        );
    }

    #[test]
    fn test_decimal_correction_v3_synthetic_mapping() {
        // Regression for the V3 synthetic mapping that triggered the live
        // phantom-profit bug: (reserve_in, reserve_out) = (1.0, raw_spot).
        // raw_spot = (sqrt_price / 2^96)^2 ~ token1_base_per_token0_base.
        //
        // For a USDC(token0, 6 dec) / WETH(token1, 18 dec) V3 pool priced at
        // ~3000 USDC/WETH, raw_spot (WETH-base per USDC-base) ~ token1/token0.
        // Human price WETH per USDC = (1/3000); in base units that is
        //   raw_spot = (1/3000) * 10^(dec1 - dec0) = (1/3000) * 1e12.
        // The edge direction here is USDC(0) -> WETH(1), so the corrected human
        // rate must be ~ (1/3000) * fee, NOT off by 1e12.
        let mut g = PriceGraph::new(2);
        g.set_token_decimals(0, 6); // USDC = token0
        g.set_token_decimals(1, 18); // WETH = token1

        let pool_id = make_pool_id(2, ProtocolType::UniswapV3);
        g.add_edge(
            0,
            1,
            1.0,
            pool_id,
            Address::repeat_byte(2),
            ProtocolType::UniswapV3,
            U256::from(1u64),
        );

        let fee = 0.997_f64;
        // raw_spot in base units: (1/3000) human * 10^(18-6) = (1/3000) * 1e12.
        let raw_spot = (1.0 / 3000.0) * 1e12;

        // V3 synthetic: reserve_in = 1.0 (dimensionless), reserve_out = raw_spot.
        g.update_edge_from_reserves(0, 1, pool_id, 1.0, raw_spot, fee);

        let human_rate = (-g.edges_from(0)[0].weight).exp();
        let expected = (1.0 / 3000.0) * fee;
        assert!(
            (human_rate - expected).abs() / expected < 1e-9,
            "V3 synthetic USDC->WETH human rate off: got {human_rate}, want ~{expected} (must NOT carry 1e12)"
        );
        // Sanity: a 1e12-off bug would put human_rate near 3.3e8; assert nowhere close.
        assert!(human_rate < 1.0, "human rate must be sub-1, got {human_rate}");
    }

    // --------------- Minimum-liquidity floor ---------------

    /// Build a single WETH-paired V2 pool (vertex 0 = some token, vertex 1 =
    /// WETH) with both directional placeholder edges in place, ready for a
    /// `update_edge_from_reserves` call. Returns the graph and the pool_id.
    fn build_weth_paired_pool(weth_decimals: u8) -> (PriceGraph, PoolId) {
        let mut g = PriceGraph::new(2);
        g.set_token_decimals(0, 18); // arbitrary token
        g.set_token_decimals(1, weth_decimals); // WETH
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1u64));
        g.add_edge(1, 0, 1.0, pool_id, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1u64));
        (g, pool_id)
    }

    #[test]
    fn test_floor_filters_low_weth_reserve() {
        // WETH-side human reserve = 0.5 WETH < floor 1.0 → filtered.
        let (mut g, pool_id) = build_weth_paired_pool(18);
        g.set_weth_vertex(1);
        g.set_min_liquidity_weth(1.0);

        // 0.5 WETH raw = 5e17 wei on the WETH side (vertex 1 = `to` for 0->1).
        let token_reserve = 1_000_000_000_000_000_000.0_f64; // 1.0 token
        let weth_reserve = 500_000_000_000_000_000.0_f64; // 0.5 WETH
        g.update_edge_from_reserves(0, 1, pool_id, token_reserve, weth_reserve, 0.997);
        g.update_edge_from_reserves(1, 0, pool_id, weth_reserve, token_reserve, 0.997);

        // Both directions must be filtered (each carries the same dead pool).
        assert!(g.edges_from(0)[0].filtered, "0->1 edge should be filtered");
        assert!(g.edges_from(1)[0].filtered, "1->0 edge should be filtered");
        // Mirror in all_edges must agree.
        assert!(g.all_edges().iter().all(|e| e.filtered));
    }

    #[test]
    fn test_floor_passes_high_weth_reserve() {
        // WETH-side human reserve = 10 WETH > floor 1.0 → not filtered.
        let (mut g, pool_id) = build_weth_paired_pool(18);
        g.set_weth_vertex(1);
        g.set_min_liquidity_weth(1.0);

        let token_reserve = 30_000_000_000_000_000_000_000.0_f64; // 30000 token
        let weth_reserve = 10_000_000_000_000_000_000.0_f64; // 10 WETH
        g.update_edge_from_reserves(0, 1, pool_id, token_reserve, weth_reserve, 0.997);
        g.update_edge_from_reserves(1, 0, pool_id, weth_reserve, token_reserve, 0.997);

        assert!(!g.edges_from(0)[0].filtered, "0->1 edge should not be filtered");
        assert!(!g.edges_from(1)[0].filtered, "1->0 edge should not be filtered");
        assert!(g.all_edges().iter().all(|e| !e.filtered));
    }

    #[test]
    fn test_floor_ignores_non_weth_pool() {
        // Neither vertex is WETH (weth_vertex points at a third vertex) → never
        // filtered regardless of how tiny the reserves are.
        let mut g = PriceGraph::new(3);
        g.set_token_decimals(0, 18);
        g.set_token_decimals(1, 18);
        g.set_weth_vertex(2); // WETH is vertex 2, not part of this pool
        g.set_min_liquidity_weth(1.0);

        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1u64));
        // Tiny reserves on both sides — would be filtered if this were WETH-paired.
        g.update_edge_from_reserves(0, 1, pool_id, 1.0, 1.0, 0.997);

        assert!(!g.edges_from(0)[0].filtered, "non-WETH pool must never be floored");
    }

    #[test]
    fn test_floor_disabled_when_weth_vertex_unset() {
        // weth_vertex None → backward-compatible, never filtered.
        let (mut g, pool_id) = build_weth_paired_pool(18);
        g.set_min_liquidity_weth(1.0); // floor set, but no weth_vertex
        g.update_edge_from_reserves(0, 1, pool_id, 1.0, 1.0, 0.997);
        assert!(!g.edges_from(0)[0].filtered);
    }

    #[test]
    fn test_floor_disabled_when_floor_zero() {
        // min_liquidity_weth 0.0 (default) → never filtered even with weth_vertex.
        let (mut g, pool_id) = build_weth_paired_pool(18);
        g.set_weth_vertex(1);
        // Leave floor at its 0.0 default.
        assert_eq!(g.min_liquidity_weth(), 0.0);
        g.update_edge_from_reserves(0, 1, pool_id, 1.0, 1.0, 0.997);
        assert!(!g.edges_from(0)[0].filtered);
    }

    #[test]
    fn test_floor_decimal_aware_conversion() {
        // WETH is 18-dec: raw 5e17 wei = 0.5 WETH < floor 1.0 → filtered.
        // Verifies the human conversion divides by 10^decimals correctly.
        let (mut g, pool_id) = build_weth_paired_pool(18);
        g.set_weth_vertex(1);
        g.set_min_liquidity_weth(1.0);

        // WETH side (vertex 1) is `to` for the 0->1 direction.
        let weth_raw = 500_000_000_000_000_000.0_f64; // 5e17 wei = 0.5 WETH
        g.update_edge_from_reserves(0, 1, pool_id, 1.0e18, weth_raw, 0.997);
        assert!(
            g.edges_from(0)[0].filtered,
            "0.5 WETH (raw 5e17) must be below floor 1.0 after decimal conversion"
        );

        // Bump just above the floor: 1.5 WETH = 1.5e18 wei → not filtered.
        let weth_raw_ok = 1_500_000_000_000_000_000.0_f64;
        g.update_edge_from_reserves(0, 1, pool_id, 1.0e18, weth_raw_ok, 0.997);
        assert!(
            !g.edges_from(0)[0].filtered,
            "1.5 WETH must be above floor 1.0 after decimal conversion"
        );
    }

    #[test]
    fn test_add_edge_initializes_filtered_false() {
        let mut g = PriceGraph::new(2);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(0, 1, 2.0, pool_id, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1u64));
        assert!(!g.all_edges()[0].filtered, "placeholder edge must start unfiltered");
    }

    #[test]
    fn test_floor_skips_v3_when_weth_is_to_side() {
        // Regression: WETH-paired V3 pools carry the synthetic
        // `(reserve_in, reserve_out) = (1.0, raw_spot)` seed. When WETH
        // is the `to` side and the pair has decimal mismatch (e.g.
        // USDC(6) / WETH(18)), `raw_spot / 1e18` is ≈ 0, which the
        // legacy floor wrongly interpreted as "below the minimum WETH
        // reserve" and filtered the edge. The V3 carve-out skips that
        // computation entirely.
        let mut g = PriceGraph::new(2);
        g.set_token_decimals(0, 6); // USDC = token0
        g.set_token_decimals(1, 18); // WETH = token1
        g.set_weth_vertex(1);
        g.set_min_liquidity_weth(1.0);

        let pool_id = PoolId {
            address: Address::repeat_byte(2),
            protocol: ProtocolType::UniswapV3,
        };
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(2), ProtocolType::UniswapV3, U256::ZERO);

        // Realistic V3 USDC->WETH synthetic seed: spot in raw base units
        // = (1/3000) human * 10^(18-6) = (1/3000) * 1e12 ≈ 3.33e8.
        let raw_spot = (1.0 / 3000.0) * 1e12;
        g.update_edge_from_reserves(0, 1, pool_id, 1.0, raw_spot, 0.997);

        assert!(
            !g.edges_from(0)[0].filtered,
            "deep V3 WETH-paired pool must not be filtered by the min_liquidity_weth floor"
        );
    }

    #[test]
    fn test_floor_skips_v3_when_weth_is_from_side() {
        // Symmetric direction: WETH on the `from` side. The synthetic
        // seed makes `reserve_in == 1.0`, which divided by 1e18 is
        // dust — would wrongly trip the floor without the V3 carve-out.
        let mut g = PriceGraph::new(2);
        g.set_token_decimals(0, 18); // WETH = token0
        g.set_token_decimals(1, 6); // USDC = token1
        g.set_weth_vertex(0);
        g.set_min_liquidity_weth(1.0);

        let pool_id = PoolId {
            address: Address::repeat_byte(3),
            protocol: ProtocolType::UniswapV3,
        };
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(3), ProtocolType::UniswapV3, U256::ZERO);

        // WETH->USDC: synthetic reserve_in = 1.0 (the WETH side), real
        // spot on the output side. The exact spot value is irrelevant
        // for the floor; what matters is the V3 carve-out skipping the
        // human conversion altogether.
        g.update_edge_from_reserves(0, 1, pool_id, 1.0, 3000.0 * 1e12, 0.997);

        assert!(
            !g.edges_from(0)[0].filtered,
            "deep V3 WETH-paired pool (WETH on from side) must not be filtered"
        );
    }

    #[test]
    fn test_floor_still_applies_to_v2_weth_pool() {
        // Negative regression: the V3 carve-out must not weaken the floor
        // for V2/Sushi/Curve/Balancer/Bancor — those pools carry real
        // base-unit reserves and the floor remains the no-oracle TVL
        // proxy that catches drained or stub WETH pools.
        let (mut g, _v2_pool) = build_weth_paired_pool(18);
        g.set_weth_vertex(1);
        g.set_min_liquidity_weth(1.0);

        let v2_pool_id = PoolId {
            address: Address::repeat_byte(4),
            protocol: ProtocolType::UniswapV2,
        };
        // Overwrite with our explicit V2 pool to be unambiguous about protocol.
        g.add_edge(0, 1, 1.0, v2_pool_id, Address::repeat_byte(4), ProtocolType::UniswapV2, U256::ZERO);

        // 0.5 WETH on the WETH side (raw 5e17), one token unit on the
        // other side. Below the 1.0 WETH floor → must still be filtered.
        let weth_raw = 500_000_000_000_000_000.0_f64;
        g.update_edge_from_reserves(0, 1, v2_pool_id, 1.0e18, weth_raw, 0.997);

        assert!(
            g.edges_from(0)
                .iter()
                .find(|e| e.pool_id == v2_pool_id)
                .expect("V2 edge")
                .filtered,
            "V2 dust WETH pool must still be filtered after the V3 carve-out"
        );
    }

    #[test]
    fn test_decimal_neutral_18_18_unchanged() {
        // Two 18-decimal tokens: 10^(18-18) = 1, so the corrected rate must
        // exactly equal the pre-change behavior (no regression).
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
        // Vertices default to 18 decimals; do not set anything.
        g.update_edge_from_reserves(0, 1, pool_id, 1000.0, 2000.0, 0.997);

        // Same expectation as the legacy test_update_edge_from_reserves.
        let expected_weight = -(2.0 * 0.997_f64).ln();
        let edge = &g.all_edges()[0];
        assert!(
            (edge.weight - expected_weight).abs() < 1e-12,
            "18/18 decimal pair must match pre-change weight"
        );
    }

    // --------------- Stale / placeholder quarantine ---------------

    #[test]
    fn quarantine_filters_never_updated_placeholder() {
        // add_edge alone leaves reserves at 0 (placeholder). Quarantine with the
        // age backstop OFF must still filter it on the placeholder signal.
        let mut g = PriceGraph::new(2);
        let pool_id = make_pool_id(1, ProtocolType::UniswapV2);
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(1), ProtocolType::UniswapV2, U256::from(1u64));
        assert!(!g.edges_from(0)[0].filtered, "placeholder starts unfiltered");

        let n = g.quarantine_stale_edges(0);
        assert_eq!(n, 1, "the one placeholder edge should be quarantined");
        assert!(g.edges_from(0)[0].filtered, "placeholder must be filtered");
        assert!(g.all_edges()[0].filtered, "mirror in all_edges must agree");
    }

    #[test]
    fn quarantine_keeps_updated_edge_regardless_of_age_when_backstop_off() {
        // A real, updated edge that has not traded for many blocks is NOT stale
        // for an AMM — its reserves are still exact. With the age backstop off
        // (max_age_blocks = 0) it must be kept.
        let mut g = PriceGraph::new(2);
        let pool_id = make_pool_id(2, ProtocolType::UniswapV2);
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(2), ProtocolType::UniswapV2, U256::from(1u64));
        g.set_current_block(100);
        g.update_edge_from_reserves(0, 1, pool_id, 1_000.0, 2_000.0, 0.997);
        // Chain advanced far past the last update.
        g.set_current_block(1_000_000);

        let n = g.quarantine_stale_edges(0);
        assert_eq!(n, 0, "quiet-but-valid edge must NOT be filtered when age backstop is off");
        assert!(!g.edges_from(0)[0].filtered);
        assert_eq!(g.all_edges()[0].last_update_block, 100, "stamp records the update block");
    }

    #[test]
    fn quarantine_age_backstop_is_opt_in() {
        let mut g = PriceGraph::new(2);
        let pool_id = make_pool_id(3, ProtocolType::UniswapV2);
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(3), ProtocolType::UniswapV2, U256::from(1u64));
        g.set_current_block(100);
        g.update_edge_from_reserves(0, 1, pool_id, 1_000.0, 2_000.0, 0.997);
        g.set_current_block(100 + 51); // 51 blocks since update

        // max_age 50 → 51 > 50 → filtered (opt-in backstop enabled).
        let n = g.quarantine_stale_edges(50);
        assert_eq!(n, 1, "updated edge older than max_age must be filtered when backstop enabled");
        assert!(g.edges_from(0)[0].filtered);
    }

    #[test]
    fn quarantine_self_heals_on_fresh_update() {
        // A quarantined placeholder that later receives real reserves must
        // un-filter (update_edge_from_reserves re-derives filtered from the
        // liquidity floor, which is disabled here → false).
        let mut g = PriceGraph::new(2);
        let pool_id = make_pool_id(4, ProtocolType::UniswapV2);
        g.add_edge(0, 1, 1.0, pool_id, Address::repeat_byte(4), ProtocolType::UniswapV2, U256::from(1u64));
        g.quarantine_stale_edges(0);
        assert!(g.edges_from(0)[0].filtered, "placeholder quarantined");

        g.set_current_block(500);
        g.update_edge_from_reserves(0, 1, pool_id, 1_000.0, 2_000.0, 0.997);
        assert!(!g.edges_from(0)[0].filtered, "fresh reserves must clear the quarantine");
        assert_eq!(g.edges_from(0)[0].last_update_block, 500);
    }
}
