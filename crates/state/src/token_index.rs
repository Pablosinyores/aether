use alloy::primitives::Address;
use std::collections::HashMap;

/// Bidirectional mapping between token addresses and graph vertex indices.
///
/// Every unique token that appears in the monitored pool set is assigned a
/// stable integer index. This index is used as the vertex identifier in the
/// [`PriceGraph`](crate::price_graph::PriceGraph), avoiding the overhead of
/// hashing `Address` values on every edge lookup.
#[derive(Debug, Clone)]
pub struct TokenIndex {
    /// Forward map: address -> index.
    addr_to_idx: HashMap<Address, usize>,
    /// Reverse map: index -> address (position = index).
    idx_to_addr: Vec<Address>,
}

impl TokenIndex {
    /// Create an empty token index.
    pub fn new() -> Self {
        Self {
            addr_to_idx: HashMap::new(),
            idx_to_addr: Vec::new(),
        }
    }

    /// Get the index for a token address, inserting it if it does not yet
    /// exist. The returned index is stable for the lifetime of this
    /// `TokenIndex`.
    pub fn get_or_insert(&mut self, address: Address) -> usize {
        if let Some(&idx) = self.addr_to_idx.get(&address) {
            idx
        } else {
            let idx = self.idx_to_addr.len();
            self.addr_to_idx.insert(address, idx);
            self.idx_to_addr.push(address);
            idx
        }
    }

    /// Look up the index for a token address. Returns `None` if the token
    /// has not been indexed.
    #[inline]
    pub fn get_index(&self, address: &Address) -> Option<usize> {
        self.addr_to_idx.get(address).copied()
    }

    /// Look up the address for a given index. Returns `None` if the index
    /// is out of bounds.
    #[inline]
    pub fn get_address(&self, index: usize) -> Option<&Address> {
        self.idx_to_addr.get(index)
    }

    /// Total number of tokens indexed.
    #[inline]
    pub fn len(&self) -> usize {
        self.idx_to_addr.len()
    }

    /// Returns `true` if no tokens have been indexed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.idx_to_addr.is_empty()
    }

    /// Get a slice of all indexed addresses in index order.
    #[inline]
    pub fn all_addresses(&self) -> &[Address] {
        &self.idx_to_addr
    }

    /// Check whether a token address has been indexed.
    #[inline]
    pub fn contains(&self, address: &Address) -> bool {
        self.addr_to_idx.contains_key(address)
    }
}

impl Default for TokenIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn test_new_index_is_empty() {
        let idx = TokenIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.all_addresses().is_empty());
    }

    #[test]
    fn test_default_is_empty() {
        let idx = TokenIndex::default();
        assert!(idx.is_empty());
    }

    #[test]
    fn test_insert_single() {
        let mut idx = TokenIndex::new();
        let addr = Address::repeat_byte(0x01);
        let i = idx.get_or_insert(addr);
        assert_eq!(i, 0);
        assert_eq!(idx.len(), 1);
        assert!(!idx.is_empty());
    }

    #[test]
    fn test_insert_multiple() {
        let mut idx = TokenIndex::new();
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        let c = Address::repeat_byte(0x03);

        assert_eq!(idx.get_or_insert(a), 0);
        assert_eq!(idx.get_or_insert(b), 1);
        assert_eq!(idx.get_or_insert(c), 2);
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn test_insert_duplicate_returns_same_index() {
        let mut idx = TokenIndex::new();
        let addr = Address::repeat_byte(0xAA);

        let first = idx.get_or_insert(addr);
        let second = idx.get_or_insert(addr);
        assert_eq!(first, second);
        assert_eq!(idx.len(), 1, "duplicate insert should not increase size");
    }

    #[test]
    fn test_get_index_existing() {
        let mut idx = TokenIndex::new();
        let addr = Address::repeat_byte(0x05);
        let i = idx.get_or_insert(addr);
        assert_eq!(idx.get_index(&addr), Some(i));
    }

    #[test]
    fn test_get_index_nonexistent() {
        let idx = TokenIndex::new();
        let addr = Address::repeat_byte(0x05);
        assert_eq!(idx.get_index(&addr), None);
    }

    #[test]
    fn test_get_address_existing() {
        let mut idx = TokenIndex::new();
        let addr = Address::repeat_byte(0x07);
        let i = idx.get_or_insert(addr);
        assert_eq!(idx.get_address(i), Some(&addr));
    }

    #[test]
    fn test_get_address_out_of_bounds() {
        let idx = TokenIndex::new();
        assert_eq!(idx.get_address(0), None);
        assert_eq!(idx.get_address(999), None);
    }

    #[test]
    fn test_bidirectional_consistency() {
        let mut idx = TokenIndex::new();
        let addresses: Vec<Address> = (1..=10).map(|i| Address::repeat_byte(i)).collect();

        for &addr in &addresses {
            idx.get_or_insert(addr);
        }

        // Forward then reverse.
        for &addr in &addresses {
            let i = idx.get_index(&addr).unwrap();
            assert_eq!(idx.get_address(i), Some(&addr));
        }

        // Reverse then forward.
        for i in 0..idx.len() {
            let addr = idx.get_address(i).unwrap();
            assert_eq!(idx.get_index(addr), Some(i));
        }
    }

    #[test]
    fn test_contains() {
        let mut idx = TokenIndex::new();
        let addr = Address::repeat_byte(0x10);

        assert!(!idx.contains(&addr));
        idx.get_or_insert(addr);
        assert!(idx.contains(&addr));
    }

    #[test]
    fn test_all_addresses() {
        let mut idx = TokenIndex::new();
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        let c = Address::repeat_byte(0x03);

        idx.get_or_insert(a);
        idx.get_or_insert(b);
        idx.get_or_insert(c);

        let all = idx.all_addresses();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], a);
        assert_eq!(all[1], b);
        assert_eq!(all[2], c);
    }

    #[test]
    fn test_clone() {
        let mut idx = TokenIndex::new();
        let addr = Address::repeat_byte(0x42);
        idx.get_or_insert(addr);

        let cloned = idx.clone();
        assert_eq!(cloned.len(), 1);
        assert_eq!(cloned.get_index(&addr), Some(0));
        assert_eq!(cloned.get_address(0), Some(&addr));
    }

    #[test]
    fn test_well_known_tokens() {
        use aether_common::types::addresses;
        let mut idx = TokenIndex::new();

        let weth_idx = idx.get_or_insert(addresses::WETH);
        let usdc_idx = idx.get_or_insert(addresses::USDC);
        let dai_idx = idx.get_or_insert(addresses::DAI);

        assert_ne!(weth_idx, usdc_idx);
        assert_ne!(usdc_idx, dai_idx);
        assert_eq!(idx.get_index(&addresses::WETH), Some(weth_idx));
        assert_eq!(idx.get_address(dai_idx), Some(&addresses::DAI));
    }
}
