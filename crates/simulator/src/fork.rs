use std::sync::Arc;

use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider};
use futures::StreamExt;
use revm::bytecode::Bytecode;
use revm::database::CacheDB;
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

/// Maximum number of pre-warm RPC requests in flight at once.
///
/// Free-tier RPC providers (notably Alchemy at 25 CU/s) collapse into 429
/// throttling when `eth_getCode` + `eth_getStorageAt` batches are dispatched
/// without a concurrency cap. Capping in-flight requests at this value keeps
/// pre-warm under typical free-tier burst ceilings while still parallelising
/// enough to keep wall-clock latency low.
///
/// Override via `AETHER_PREWARM_MAX_CONCURRENT`.
pub const DEFAULT_PREWARM_MAX_CONCURRENT: usize = 8;

/// Resolve the prewarm concurrency cap. Reads `AETHER_PREWARM_MAX_CONCURRENT`
/// if set and positive, otherwise returns [`DEFAULT_PREWARM_MAX_CONCURRENT`].
fn resolve_prewarm_max_concurrent() -> usize {
    std::env::var("AETHER_PREWARM_MAX_CONCURRENT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_PREWARM_MAX_CONCURRENT)
}

/// Fetch contract code and known storage slots for `code_addresses` and
/// `v2_pool_addresses` at `block_number`, returning a `PrewarmedState` ready
/// to be injected into parallel simulation tasks.
///
/// RPC calls are dispatched concurrently but capped by a shared in-flight
/// semaphore (default [`DEFAULT_PREWARM_MAX_CONCURRENT`], overridable via
/// `AETHER_PREWARM_MAX_CONCURRENT`). Errors on individual addresses are logged
/// and skipped — pre-warming is best-effort; missing entries simply result in
/// a per-task cache miss (lazy RPC fetch) rather than a hard failure.
///
/// **`v2_pool_addresses`**: UniswapV2 / SushiSwap pools whose packed-reserve
/// slot (slot 8) is pre-fetched. This is the single most impactful storage
/// slot to warm — `getReserves()` reads it on every V2 swap path.
pub async fn prewarm_state(
    provider: &DynProvider<Ethereum>,
    block_number: u64,
    code_addresses: &[Address],
    v2_pool_addresses: &[Address],
) -> PrewarmedState {
    let max_concurrent = resolve_prewarm_max_concurrent();
    // Shared in-flight gate across both the code and storage streams so the
    // total RPC pressure is bounded by `max_concurrent`, not 2× that value.
    let gate = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let block_id = BlockId::from(block_number);

    // Fetch contract code via a bounded-concurrency stream. Each future
    // acquires a semaphore permit before issuing the request and releases it
    // on completion, keeping at most `max_concurrent` in flight across both
    // streams combined.
    let code_addrs = code_addresses.to_vec();
    let code_gate = gate.clone();
    let code_provider = provider.clone();
    let code_stream = futures::stream::iter(code_addrs.into_iter().map(move |addr| {
        let p = code_provider.clone();
        let g = code_gate.clone();
        async move {
            let _permit = g.acquire().await.ok()?;
            match p.get_code_at(addr).block_id(block_id).await {
                Ok(code) if !code.is_empty() => {
                    let code_hash = alloy::primitives::keccak256(&code);
                    let bytecode = Bytecode::new_raw(
                        revm::primitives::Bytes::copy_from_slice(&code),
                    );
                    Some((code_hash, bytecode))
                }
                Ok(_) => None, // empty bytecode (EOA)
                Err(e) => {
                    warn!(%addr, error = %e, "pre-warm: failed to fetch contract code");
                    None
                }
            }
        }
    }))
    .buffer_unordered(max_concurrent);

    // Fetch slot 8 (packed reserves: reserve0 | reserve1 | blockTimestampLast)
    // for UniswapV2 / SushiSwap pools through the same shared gate.
    const V2_RESERVES_SLOT: u64 = 8;
    let storage_addrs = v2_pool_addresses.to_vec();
    let storage_gate = gate.clone();
    let storage_provider = provider.clone();
    let storage_stream = futures::stream::iter(storage_addrs.into_iter().map(move |addr| {
        let p = storage_provider.clone();
        let g = storage_gate.clone();
        async move {
            let _permit = g.acquire().await.ok()?;
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
    }))
    .buffer_unordered(max_concurrent);

    let (code_results, storage_results) = tokio::join!(
        code_stream.collect::<Vec<_>>(),
        storage_stream.collect::<Vec<_>>(),
    );

    debug!(
        code_warmed = code_results.iter().filter(|r| r.is_some()).count(),
        storage_warmed = storage_results.iter().filter(|r| r.is_some()).count(),
        max_concurrent,
        "Block pre-warm complete"
    );

    PrewarmedState {
        code_cache: code_results.into_iter().flatten().collect(),
        storage: storage_results.into_iter().flatten().collect(),
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

    // ── Pre-warm concurrency cap ───────────────────────────────────

    /// Mutex serialises tests that mutate the shared `AETHER_PREWARM_MAX_CONCURRENT`
    /// env var so they cannot race with each other.
    fn prewarm_env_guard() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn prewarm_max_concurrent_defaults_when_env_absent() {
        let _guard = prewarm_env_guard();
        // SAFETY: tests in this module are serialised through `prewarm_env_guard`.
        unsafe { std::env::remove_var("AETHER_PREWARM_MAX_CONCURRENT") };
        assert_eq!(
            resolve_prewarm_max_concurrent(),
            DEFAULT_PREWARM_MAX_CONCURRENT
        );
    }

    #[test]
    fn prewarm_max_concurrent_reads_env_override() {
        let _guard = prewarm_env_guard();
        unsafe { std::env::set_var("AETHER_PREWARM_MAX_CONCURRENT", "3") };
        assert_eq!(resolve_prewarm_max_concurrent(), 3);
        unsafe { std::env::remove_var("AETHER_PREWARM_MAX_CONCURRENT") };
    }

    #[test]
    fn prewarm_max_concurrent_rejects_zero_and_garbage() {
        let _guard = prewarm_env_guard();
        // Zero must fall back to the default — a 0-permit semaphore would
        // deadlock the pre-warm path.
        unsafe { std::env::set_var("AETHER_PREWARM_MAX_CONCURRENT", "0") };
        assert_eq!(
            resolve_prewarm_max_concurrent(),
            DEFAULT_PREWARM_MAX_CONCURRENT
        );
        unsafe { std::env::set_var("AETHER_PREWARM_MAX_CONCURRENT", "not-a-number") };
        assert_eq!(
            resolve_prewarm_max_concurrent(),
            DEFAULT_PREWARM_MAX_CONCURRENT
        );
        unsafe { std::env::remove_var("AETHER_PREWARM_MAX_CONCURRENT") };
    }

    /// Verifies the shared semaphore actually caps in-flight work. We dispatch
    /// 50 futures against a 4-permit gate built the same way `prewarm_state`
    /// builds its own, observe the peak concurrency reached, and confirm it
    /// never exceeds the configured cap.
    #[tokio::test]
    async fn prewarm_semaphore_bounds_in_flight_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        const TASKS: usize = 50;
        const CAP: usize = 4;

        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(CAP));

        let stream = futures::stream::iter((0..TASKS).map(|_| {
            let g = gate.clone();
            let inf = in_flight.clone();
            let pk = peak.clone();
            async move {
                let _permit = g.acquire().await.unwrap();
                let current = inf.fetch_add(1, Ordering::SeqCst) + 1;
                pk.fetch_max(current, Ordering::SeqCst);
                // Hold the permit long enough for concurrency to build up.
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                inf.fetch_sub(1, Ordering::SeqCst);
            }
        }))
        .buffer_unordered(CAP);

        stream.collect::<Vec<_>>().await;

        assert!(
            peak.load(Ordering::SeqCst) <= CAP,
            "peak in-flight {} exceeded cap {}",
            peak.load(Ordering::SeqCst),
            CAP
        );
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }
}
