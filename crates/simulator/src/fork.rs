use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider};
use futures::future::join_all;
use revm::bytecode::Bytecode;

use crate::bytecode_cache::BytecodeCache;
use crate::v2_reserves_cache::V2ReservesCache;
use revm::database::CacheDB;

/// How many blocks of WS lag the pre-warm reads-from-cache path tolerates
/// before falling back to RPC. `Sync` events arrive within ~50 ms of a
/// block landing on the WS subscription; one block of slack (~12 s) is
/// more than enough headroom under normal latency while still rejecting
/// clearly stale snapshots after a reconnect or reorg.
pub const V2_RESERVES_MAX_LAG_BLOCKS: u64 = 1;
use revm::database_interface::EmptyDB;
use revm::state::AccountInfo;
use revm_database::{AlloyDB, BlockId, WrapDatabaseAsync};
use tracing::{debug, warn};

// ── CacheDB pre-warming ────────────────────────────────────────────

/// Pre-fetched contract code and storage for the simulation hot path.
///
/// Built once per block cycle before parallel simulation is dispatched.
/// Injected into each task's `RpcForkedState` cache so that per-task cold
/// RPC fetches are eliminated — all simulations sharing the same block see
/// the same contract state without redundant network round-trips.
#[derive(Default)]
pub struct PrewarmedState {
    /// Bytecode cache: (code_hash, bytecode) pairs for pre-fetched contracts.
    ///
    /// Stored by code hash (not address) and injected directly into
    /// `CacheDB::cache.contracts` — this warms the bytecode cache without
    /// touching account balance or nonce, so pools that hold ETH (V3, Curve,
    /// Balancer) are not incorrectly zeroed before simulation.
    code_cache: Vec<(B256, Bytecode)>,
    /// (address, slot, value) — pre-fetched storage slots (e.g. V2 reserves).
    storage: Vec<(Address, U256, U256)>,
}

impl PrewarmedState {
    /// Inject pre-fetched bytecode and storage into an `RpcForkedState` cache.
    ///
    /// Bytecode is inserted by code hash only — balance and nonce are left for
    /// lazy RPC fetch so on-chain ETH holdings are never clobbered with zero.
    pub fn inject_into(&self, state: &mut RpcForkedState) {
        for (code_hash, bytecode) in &self.code_cache {
            state.db.cache.contracts.insert(*code_hash, bytecode.clone());
        }
        for &(addr, slot, value) in &self.storage {
            if let Err(e) = state.db.insert_account_storage(addr, slot, value) {
                warn!(%addr, %slot, error = %e, "pre-warm: failed to insert storage slot");
            }
        }
    }
}

/// Fetch contract code and known storage slots for `code_addresses` and
/// `v2_pool_addresses` at `block_number`, returning a `PrewarmedState` ready
/// to be injected into parallel simulation tasks.
///
/// All RPC calls are issued concurrently via `join_all`. Errors on individual
/// addresses are logged and skipped — pre-warming is best-effort; missing
/// entries simply result in a per-task cache miss (lazy RPC fetch) rather
/// than a hard failure.
///
/// **`v2_pool_addresses`**: UniswapV2 / SushiSwap pools whose packed-reserve
/// slot (slot 8) is pre-fetched. This is the single most impactful storage
/// slot to warm — `getReserves()` reads it on every V2 swap path.
///
/// **`bytecode_cache`**: when supplied, addresses already resident in the
/// cache short-circuit the `eth_getCode` call entirely; freshly fetched
/// bytecode is persisted back so subsequent block cycles serve from the
/// cache. Pass `None` to retain the historical RPC-every-time behaviour.
///
/// **`v2_reserves_cache`**: when supplied, V2 pool addresses whose latest
/// `Sync` event landed within [`V2_RESERVES_MAX_LAG_BLOCKS`] of
/// `block_number` synthesise the slot-8 storage value locally and skip the
/// `eth_getStorageAt` round-trip. Stale pools fall back to RPC.
pub async fn prewarm_state(
    provider: &DynProvider<Ethereum>,
    block_number: u64,
    code_addresses: &[Address],
    v2_pool_addresses: &[Address],
    bytecode_cache: Option<&BytecodeCache>,
    v2_reserves_cache: Option<&V2ReservesCache>,
) -> PrewarmedState {
    let block_id = BlockId::from(block_number);

    // Partition addresses into cache hits (served locally) and misses (must
    // RPC). Hits bypass the entire RPC fan-out so they don't even contribute
    // to the in-flight burst that drives free-tier 429s.
    let mut cached: Vec<(B256, Bytecode)> = Vec::new();
    let mut to_fetch: Vec<Address> = Vec::with_capacity(code_addresses.len());
    if let Some(cache) = bytecode_cache {
        for &addr in code_addresses {
            match cache.get(addr) {
                Some(hit) => cached.push(hit),
                None => to_fetch.push(addr),
            }
        }
    } else {
        to_fetch.extend_from_slice(code_addresses);
    }

    // Fetch contract code for every cache-miss in parallel. Returns
    // (code_hash, bytecode) pairs — we warm the bytecode cache only,
    // leaving account balance/nonce for lazy RPC fetch.
    let code_futs = to_fetch.into_iter().map(|addr| {
        let p = provider.clone();
        let cache = bytecode_cache.cloned();
        async move {
            match p.get_code_at(addr).block_id(block_id).await {
                Ok(code) if !code.is_empty() => {
                    let code_hash = alloy::primitives::keccak256(&code);
                    let bytecode = Bytecode::new_raw(
                        revm::primitives::Bytes::copy_from_slice(&code),
                    );
                    if let Some(c) = cache.as_ref() {
                        if let Err(e) = c.put(addr, code_hash, &code) {
                            warn!(%addr, error = %e, "pre-warm: bytecode cache persist failed");
                        }
                    }
                    Some((code_hash, bytecode))
                }
                Ok(_) => None, // empty bytecode (EOA)
                Err(e) => {
                    warn!(%addr, error = %e, "pre-warm: failed to fetch contract code");
                    None
                }
            }
        }
    });

    // Slot 8 of every UniV2 pair packs `(blockTimestampLast << 224) |
    // (reserve1 << 112) | reserve0`. Two paths feed it:
    //
    // 1. WS-fed `V2ReservesCache`: synthesise the slot locally for any pool
    //    whose latest `Sync` event landed within `V2_RESERVES_MAX_LAG_BLOCKS`
    //    of the target block. Stale or missing entries fall through.
    // 2. RPC `eth_getStorageAt` for the remaining addresses.
    const V2_RESERVES_SLOT: u64 = 8;
    let mut ws_storage: Vec<(Address, U256, U256)> = Vec::new();
    let mut storage_to_fetch: Vec<Address> = Vec::with_capacity(v2_pool_addresses.len());
    if let Some(rcache) = v2_reserves_cache {
        for &addr in v2_pool_addresses {
            match rcache.get_fresh(addr, block_number, V2_RESERVES_MAX_LAG_BLOCKS) {
                Some(snap) => {
                    ws_storage.push((addr, U256::from(V2_RESERVES_SLOT), snap.pack_slot8()));
                }
                None => storage_to_fetch.push(addr),
            }
        }
    } else {
        storage_to_fetch.extend_from_slice(v2_pool_addresses);
    }
    let storage_futs = storage_to_fetch.into_iter().map(|addr| {
        let p = provider.clone();
        async move {
            match p
                .get_storage_at(addr, U256::from(V2_RESERVES_SLOT))
                .block_id(block_id)
                .await
            {
                Ok(value) if value != U256::ZERO => {
                    Some((addr, U256::from(V2_RESERVES_SLOT), value))
                }
                Ok(_) => None,
                Err(e) => {
                    warn!(%addr, error = %e, "pre-warm: failed to fetch V2 reserve slot");
                    None
                }
            }
        }
    });

    let (code_results, storage_results) =
        tokio::join!(join_all(code_futs), join_all(storage_futs));

    let cache_hits = cached.len();
    let rpc_fetched = code_results.iter().filter(|r| r.is_some()).count();
    let ws_reserves_hits = ws_storage.len();
    let storage_warmed = storage_results.iter().filter(|r| r.is_some()).count();
    debug!(
        cache_hits,
        rpc_fetched,
        ws_reserves_hits,
        storage_warmed,
        "Block pre-warm complete"
    );

    // Merge cached + freshly fetched entries. Order doesn't matter because
    // injection is keyed by code hash on the consumer side.
    let mut code_cache = cached;
    code_cache.extend(code_results.into_iter().flatten());

    let mut storage = ws_storage;
    storage.extend(storage_results.into_iter().flatten());

    PrewarmedState {
        code_cache,
        storage,
    }
}

// ── RPC-backed forked state (AlloyDB) ──────────────────────────────

/// Inner AlloyDB parameterized on the type-erased provider.
type AlloyDbInner = AlloyDB<Ethereum, DynProvider<Ethereum>>;

/// Synchronous wrapper around the async AlloyDB.
type SyncAlloyDb = WrapDatabaseAsync<AlloyDbInner>;

/// The database type used by `RpcForkedState`: a local cache backed by
/// lazy RPC fetches via AlloyDB.
pub type RpcDB = CacheDB<SyncAlloyDb>;

/// Forked EVM state backed by a real Ethereum RPC endpoint.
///
/// On every cache miss (unknown account, storage slot, or block hash)
/// the underlying `AlloyDB` fetches the value from the remote node.
/// Subsequent reads are served from the in-memory `CacheDB`.
///
/// **Must** be created inside a multi-threaded tokio runtime
/// (`WrapDatabaseAsync::new` uses `block_in_place`).
pub struct RpcForkedState {
    pub db: RpcDB,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub base_fee: u64,
    pub chain_id: u64,
}

impl RpcForkedState {
    /// Create a new RPC-backed forked state pinned at `block_number`.
    ///
    /// Returns `None` when called outside a multi-threaded tokio runtime
    /// (required by `WrapDatabaseAsync`).
    pub fn new(
        provider: DynProvider<Ethereum>,
        block_number: u64,
        block_timestamp: u64,
        base_fee: u64,
    ) -> Option<Self> {
        let alloy_db = AlloyDB::new(provider, BlockId::from(block_number));
        let sync_db = WrapDatabaseAsync::new(alloy_db)?;
        let cache_db = CacheDB::new(sync_db);

        Some(Self {
            db: cache_db,
            block_number,
            block_timestamp,
            base_fee,
            chain_id: 1, // Ethereum mainnet
        })
    }

    /// Create a new RPC-backed forked state that queries the provider at the
    /// `latest` block tag (not a specific block number). Required when the
    /// backing provider is an Anvil fork whose local-mined block numbers
    /// ahead of its fork base may or may not resolve cleanly for state
    /// queries — using `latest` lets Anvil serve from its current state
    /// unambiguously.
    pub fn new_at_latest(
        provider: DynProvider<Ethereum>,
        block_number: u64,
        block_timestamp: u64,
        base_fee: u64,
    ) -> Option<Self> {
        let alloy_db = AlloyDB::new(provider, BlockId::latest());
        let sync_db = WrapDatabaseAsync::new(alloy_db)?;
        let cache_db = CacheDB::new(sync_db);

        Some(Self {
            db: cache_db,
            block_number,
            block_timestamp,
            base_fee,
            chain_id: 1,
        })
    }

    /// Override the ETH balance for an address (e.g. the simulation caller).
    pub fn insert_account_balance(&mut self, address: Address, balance: U256) {
        let info = AccountInfo {
            balance,
            nonce: 0,
            code_hash: revm::primitives::KECCAK_EMPTY,
            code: None,
            ..Default::default()
        };
        self.db.insert_account_info(address, info);
        debug!(%address, %balance, "RpcForkedState: inserted EOA override");
    }
}

/// Forked EVM state using revm's CacheDB.
/// In production, this would be backed by AlloyDB for actual RPC state.
/// For testing and simulation, we use CacheDB with EmptyDB.
pub struct ForkedState {
    pub db: CacheDB<EmptyDB>,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub base_fee: u64,
    pub chain_id: u64,
}

impl ForkedState {
    /// Create a new forked state (empty, for testing)
    pub fn new_empty(block_number: u64, block_timestamp: u64, base_fee: u64) -> Self {
        Self {
            db: CacheDB::new(EmptyDB::default()),
            block_number,
            block_timestamp,
            base_fee,
            chain_id: 1, // Ethereum mainnet
        }
    }

    /// Insert an account with balance and code
    pub fn insert_account(&mut self, address: Address, balance: U256, code: Bytes) {
        let code_hash = alloy::primitives::keccak256(&code);
        let info = AccountInfo {
            balance,
            nonce: 0,
            code_hash,
            code: Some(revm::bytecode::Bytecode::new_raw(
                revm::primitives::Bytes::copy_from_slice(&code),
            )),
            ..Default::default()
        };
        self.db.insert_account_info(address, info);
        debug!(%address, %balance, "Inserted account with code");
    }

    /// Insert an account with just a balance (EOA)
    pub fn insert_account_balance(&mut self, address: Address, balance: U256) {
        let info = AccountInfo {
            balance,
            nonce: 0,
            code_hash: revm::primitives::KECCAK_EMPTY,
            code: None,
            ..Default::default()
        };
        self.db.insert_account_info(address, info);
        debug!(%address, %balance, "Inserted EOA account");
    }

    /// Insert an account with balance and nonce
    pub fn insert_account_with_nonce(
        &mut self,
        address: Address,
        balance: U256,
        nonce: u64,
    ) {
        let info = AccountInfo {
            balance,
            nonce,
            code_hash: revm::primitives::KECCAK_EMPTY,
            code: None,
            ..Default::default()
        };
        self.db.insert_account_info(address, info);
        debug!(%address, %balance, nonce, "Inserted account with nonce");
    }

    /// Insert a storage slot value
    pub fn insert_storage(&mut self, address: Address, slot: U256, value: U256) {
        self.db.insert_account_storage(address, slot, value).ok();
        debug!(%address, %slot, %value, "Inserted storage slot");
    }

    /// Get account info from the cache.
    /// Returns None if the account doesn't exist in the cache.
    pub fn get_account(&self, address: &Address) -> Option<AccountInfo> {
        self.db
            .cache
            .accounts
            .get(address)
            .and_then(|db_account| db_account.info())
    }
}

/// Configuration for EVM simulation
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub gas_limit: u64,
    pub chain_id: u64,
    pub caller: Address,
    pub value: U256,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            gas_limit: 1_000_000,
            chain_id: 1,
            caller: Address::ZERO,
            value: U256::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, U256};

    #[test]
    fn test_new_empty_state() {
        let state = ForkedState::new_empty(18_000_000, 1_700_000_000, 30_000_000_000);
        assert_eq!(state.block_number, 18_000_000);
        assert_eq!(state.block_timestamp, 1_700_000_000);
        assert_eq!(state.base_fee, 30_000_000_000);
        assert_eq!(state.chain_id, 1);
    }

    #[test]
    fn test_insert_account_balance() {
        let mut state = ForkedState::new_empty(1, 1, 0);
        let addr = address!("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
        let balance = U256::from(10_000_000_000_000_000_000u128); // 10 ETH

        state.insert_account_balance(addr, balance);

        let info = state.get_account(&addr).expect("Account should exist");
        assert_eq!(info.balance, balance);
        assert_eq!(info.nonce, 0);
        assert!(info.code.as_ref().is_none_or(|c| c.is_empty()));
    }

    #[test]
    fn test_insert_account_with_code() {
        let mut state = ForkedState::new_empty(1, 1, 0);
        let addr = address!("1111111111111111111111111111111111111111");
        let balance = U256::from(5_000_000_000_000_000_000u128);
        // Simple bytecode: PUSH1 0x00 PUSH1 0x00 RETURN
        let code = Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xf3]);

        state.insert_account(addr, balance, code.clone());

        let info = state.get_account(&addr).expect("Account should exist");
        assert_eq!(info.balance, balance);
        assert!(info.code.is_some());
        assert_ne!(info.code_hash, revm::primitives::KECCAK_EMPTY);
    }

    #[test]
    fn test_insert_account_with_nonce() {
        let mut state = ForkedState::new_empty(1, 1, 0);
        let addr = address!("2222222222222222222222222222222222222222");
        let balance = U256::from(1_000_000_000_000_000_000u128);

        state.insert_account_with_nonce(addr, balance, 42);

        let info = state.get_account(&addr).expect("Account should exist");
        assert_eq!(info.balance, balance);
        assert_eq!(info.nonce, 42);
    }

    #[test]
    fn test_insert_storage() {
        let mut state = ForkedState::new_empty(1, 1, 0);
        let addr = address!("3333333333333333333333333333333333333333");
        let slot = U256::from(0);
        let value = U256::from(12345);

        // Must insert account first, then storage
        state.insert_account_balance(addr, U256::ZERO);
        state.insert_storage(addr, slot, value);

        // Verify storage was set by checking db directly
        let db_account = state.db.cache.accounts.get(&addr).unwrap();
        assert!(db_account.storage.contains_key(&slot));
        assert_eq!(*db_account.storage.get(&slot).unwrap(), value);
    }

    #[test]
    fn test_get_nonexistent_account() {
        let state = ForkedState::new_empty(1, 1, 0);
        let addr = address!("4444444444444444444444444444444444444444");
        assert!(state.get_account(&addr).is_none());
    }

    #[test]
    fn test_multiple_accounts() {
        let mut state = ForkedState::new_empty(1, 1, 0);
        let addr1 = address!("5555555555555555555555555555555555555555");
        let addr2 = address!("6666666666666666666666666666666666666666");

        state.insert_account_balance(addr1, U256::from(100));
        state.insert_account_balance(addr2, U256::from(200));

        let info1 = state.get_account(&addr1).expect("Account 1 should exist");
        let info2 = state.get_account(&addr2).expect("Account 2 should exist");

        assert_eq!(info1.balance, U256::from(100));
        assert_eq!(info2.balance, U256::from(200));
    }

    #[test]
    fn test_sim_config_default() {
        let config = SimConfig::default();
        assert_eq!(config.gas_limit, 1_000_000);
        assert_eq!(config.chain_id, 1);
        assert_eq!(config.caller, Address::ZERO);
        assert_eq!(config.value, U256::ZERO);
    }

    #[test]
    fn test_sim_config_custom() {
        let caller = address!("7777777777777777777777777777777777777777");
        let config = SimConfig {
            gas_limit: 5_000_000,
            chain_id: 5,
            caller,
            value: U256::from(1_000_000_000_000_000_000u128),
        };
        assert_eq!(config.gas_limit, 5_000_000);
        assert_eq!(config.chain_id, 5);
        assert_eq!(config.caller, caller);
        assert_eq!(config.value, U256::from(1_000_000_000_000_000_000u128));
    }

    #[test]
    fn test_overwrite_account() {
        let mut state = ForkedState::new_empty(1, 1, 0);
        let addr = address!("8888888888888888888888888888888888888888");

        state.insert_account_balance(addr, U256::from(100));
        let info = state.get_account(&addr).unwrap();
        assert_eq!(info.balance, U256::from(100));

        // Overwrite with new balance
        state.insert_account_balance(addr, U256::from(200));
        let info = state.get_account(&addr).unwrap();
        assert_eq!(info.balance, U256::from(200));
    }

    #[test]
    fn test_multiple_storage_slots() {
        let mut state = ForkedState::new_empty(1, 1, 0);
        let addr = address!("9999999999999999999999999999999999999999");

        state.insert_account_balance(addr, U256::ZERO);
        state.insert_storage(addr, U256::from(0), U256::from(111));
        state.insert_storage(addr, U256::from(1), U256::from(222));
        state.insert_storage(addr, U256::from(2), U256::from(333));

        let db_account = state.db.cache.accounts.get(&addr).unwrap();
        assert_eq!(db_account.storage.len(), 3);
        assert_eq!(*db_account.storage.get(&U256::from(0)).unwrap(), U256::from(111));
        assert_eq!(*db_account.storage.get(&U256::from(1)).unwrap(), U256::from(222));
        assert_eq!(*db_account.storage.get(&U256::from(2)).unwrap(), U256::from(333));
    }

    // ── prewarm + bytecode cache wiring ────────────────────────────

    /// When every requested address is already resident in the bytecode
    /// cache, `prewarm_state` must surface those entries without issuing a
    /// single RPC. We exercise this by handing the function a provider
    /// pointed at an unreachable port: any cache miss would trip the
    /// connection refusal and produce a `warn!` log, but with a fully warm
    /// cache the RPC code path is never entered and the returned state
    /// reflects exactly what was pre-populated.
    #[tokio::test]
    async fn prewarm_state_skips_rpc_on_full_cache_hit() {
        use crate::bytecode_cache::BytecodeCache;
        use alloy::providers::ProviderBuilder;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = BytecodeCache::open(tmp.path()).unwrap();

        // Two addresses, each pre-populated with a distinct bytecode.
        let addr_a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let code_a = vec![0x60u8, 0x80, 0x60, 0x40, 0x52];
        let hash_a = alloy::primitives::keccak256(&code_a);
        cache.put(addr_a, hash_a, &code_a).unwrap();

        let addr_b = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let code_b = vec![0x60u8, 0x00, 0x60, 0x00, 0xf3];
        let hash_b = alloy::primitives::keccak256(&code_b);
        cache.put(addr_b, hash_b, &code_b).unwrap();

        // Localhost on a port we'll never bind to — guarantees any RPC
        // attempt fails fast (and surfaces a `warn!`) instead of hanging.
        let provider = ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1/".parse().unwrap())
            .erased();

        let state =
            prewarm_state(&provider, 1, &[addr_a, addr_b], &[], Some(&cache), None).await;

        assert_eq!(
            state.code_cache.len(),
            2,
            "both addresses must come back via the cache, not RPC"
        );
        let returned_hashes: std::collections::HashSet<_> =
            state.code_cache.iter().map(|(h, _)| *h).collect();
        assert!(returned_hashes.contains(&hash_a));
        assert!(returned_hashes.contains(&hash_b));
    }

    /// Without a cache, the function must behave exactly as before. Pointing
    /// at an unreachable RPC and supplying no addresses gives us a stable
    /// "all paths empty" baseline that verifies the new signature did not
    /// break the historical `None`-cache code path.
    #[tokio::test]
    async fn prewarm_state_without_cache_returns_empty_for_empty_input() {
        use alloy::providers::ProviderBuilder;
        let provider = ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1/".parse().unwrap())
            .erased();
        let state = prewarm_state(&provider, 1, &[], &[], None, None).await;
        assert!(state.code_cache.is_empty());
        assert!(state.storage.is_empty());
    }

    /// Verifies the WS-fed V2 reserves cache short-circuits the storage
    /// fetch path. A fully populated reserves cache must let `prewarm_state`
    /// return slot-8 entries for every V2 pool address without ever
    /// touching the RPC endpoint (which here is unreachable).
    #[tokio::test]
    async fn prewarm_state_skips_rpc_on_full_v2_reserves_hit() {
        use crate::v2_reserves_cache::V2ReservesCache;
        use alloy::providers::ProviderBuilder;

        let rcache = V2ReservesCache::new();
        let p1 = address!("1111111111111111111111111111111111111111");
        let p2 = address!("2222222222222222222222222222222222222222");
        let r0_p1 = U256::from(1_000u64);
        let r1_p1 = U256::from(2_000u64);
        let r0_p2 = U256::from(5_000u64);
        let r1_p2 = U256::from(6_000u64);
        rcache.record(p1, r0_p1, r1_p1, 100);
        rcache.record(p2, r0_p2, r1_p2, 100);

        let provider = ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1/".parse().unwrap())
            .erased();

        // Target the same block the cache recorded — well inside the lag
        // window — so both pools must surface via the WS cache.
        let state = prewarm_state(&provider, 100, &[], &[p1, p2], None, Some(&rcache)).await;

        assert_eq!(
            state.storage.len(),
            2,
            "both pools must surface slot-8 entries via the WS cache"
        );
        let by_addr: std::collections::HashMap<_, _> =
            state.storage.iter().map(|(a, _, v)| (*a, *v)).collect();
        // Decode the packed slot-8 layout to confirm reserve0 and reserve1
        // round-trip intact for at least one pool.
        let packed_p1 = by_addr.get(&p1).copied().expect("p1 entry");
        let mask112 = (U256::from(1u64) << 112) - U256::from(1u64);
        assert_eq!(packed_p1 & mask112, r0_p1);
        assert_eq!((packed_p1 >> 112) & mask112, r1_p1);
    }

    /// Stale reserve snapshots (older than the lag window) must NOT short-
    /// circuit the RPC call. The cache hit is rejected and the address
    /// falls through to the normal RPC path; with an unreachable provider
    /// that path errors and the address is simply absent from the result.
    #[tokio::test]
    async fn prewarm_state_falls_through_when_v2_cache_is_stale() {
        use crate::v2_reserves_cache::V2ReservesCache;
        use alloy::providers::ProviderBuilder;

        let rcache = V2ReservesCache::new();
        let p1 = address!("3333333333333333333333333333333333333333");
        rcache.record(p1, U256::from(1u64), U256::from(2u64), 10);

        let provider = ProviderBuilder::new()
            .connect_http("http://127.0.0.1:1/".parse().unwrap())
            .erased();

        // Target block 100, cache entry at block 10 — well outside the
        // 1-block lag window. RPC is unreachable so the result is empty.
        let state = prewarm_state(&provider, 100, &[], &[p1], None, Some(&rcache)).await;
        assert!(
            state.storage.is_empty(),
            "stale cache entries must not surface in pre-warm output"
        );
    }
}
