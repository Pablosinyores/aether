//! Persistent on-disk bytecode cache.
//!
//! Every contract on Ethereum has immutable bytecode: once deployed, the EOF
//! image at a given address (1) is content-addressable by its keccak256 hash
//! and (2) can never change for the lifetime of that address. (Cancun's
//! `SELFDESTRUCT` semantic restriction means the code field is no longer
//! cleared after a self-destruct — only storage and balance flow to the
//! beneficiary.) That makes bytecode the single highest-leverage RPC call to
//! eliminate: fetch once, persist forever.
//!
//! The cache is a two-tier structure:
//!
//! * **Disk tier** — a `redb` (pure Rust, ACID) embedded key/value store
//!   holding `address -> code_hash` and `code_hash -> raw bytecode` tables.
//!   Survives process restarts.
//! * **Memory tier** — an in-process `DashMap<Address, (B256, Bytecode)>`
//!   hot cache so repeat lookups within a process never touch disk.
//!
//! The dispatcher pattern is:
//!
//! ```text
//! mem.get(addr)            -> hit, return
//!     ↓ miss
//! disk.get(addr)           -> hit, populate mem, return
//!     ↓ miss
//! provider.get_code_at()   -> fetch, populate disk + mem, return
//! ```
//!
//! All disk failures degrade gracefully: a cache that cannot be opened simply
//! returns `None` from every lookup, forcing the caller's existing RPC
//! fallback path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy::network::Ethereum;
use alloy::primitives::{Address, B256};
use alloy::providers::{DynProvider, Provider};
use dashmap::DashMap;
use redb::{Database, TableDefinition};
use revm::bytecode::Bytecode;
use tracing::{debug, warn};

/// `address -> 32-byte code hash` table.
const ADDR_TO_HASH: TableDefinition<&[u8; 20], &[u8; 32]> = TableDefinition::new("addr_to_hash");
/// `code hash -> raw bytecode bytes` table.
const HASH_TO_CODE: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("hash_to_code");

/// Errors returned when opening or operating on the cache.
///
/// The variants box their inner redb errors because `redb`'s error types are
/// large (several enum variants with payloads). Without boxing, every
/// `Result<_, CacheError>` would force callers to allocate ~200+ bytes on the
/// stack per return value — clippy's `result_large_err` lint flags this and
/// boxing is the recommended remediation.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("redb database error: {0}")]
    Db(#[from] Box<redb::DatabaseError>),
    #[error("redb transaction error: {0}")]
    Tx(#[from] Box<redb::TransactionError>),
    #[error("redb commit error: {0}")]
    Commit(#[from] Box<redb::CommitError>),
    #[error("redb table error: {0}")]
    Table(#[from] Box<redb::TableError>),
    #[error("redb storage error: {0}")]
    Storage(#[from] Box<redb::StorageError>),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// Glue conversions so the non-boxed redb errors that `?` produces from the
// `redb` crate's APIs can flow into our boxed variants without manual mapping.
impl From<redb::DatabaseError> for CacheError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Db(Box::new(e))
    }
}
impl From<redb::TransactionError> for CacheError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Tx(Box::new(e))
    }
}
impl From<redb::CommitError> for CacheError {
    fn from(e: redb::CommitError) -> Self {
        Self::Commit(Box::new(e))
    }
}
impl From<redb::TableError> for CacheError {
    fn from(e: redb::TableError) -> Self {
        Self::Table(Box::new(e))
    }
}
impl From<redb::StorageError> for CacheError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(Box::new(e))
    }
}

/// Persistent two-tier bytecode cache.
///
/// Cheap to clone — internally an `Arc` over the redb handle and the
/// in-memory hot map.
#[derive(Clone)]
pub struct BytecodeCache {
    inner: Arc<Inner>,
}

struct Inner {
    db: Database,
    /// Hot memory tier. Populated on read; never evicted within a process
    /// lifetime (bytecode is at most ~25 MB for ~5000 contracts).
    mem: DashMap<Address, (B256, Bytecode)>,
    /// Path the database was opened from (for diagnostics).
    path: PathBuf,
}

impl BytecodeCache {
    /// Open or create a cache rooted at `path`. The file is created with
    /// both required tables initialised, so subsequent operations can
    /// assume the schema exists.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, CacheError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(&path)?;
        // Initialise tables idempotently so cold reads after first open
        // don't fail with `TableDoesNotExist`.
        {
            let tx = db.begin_write()?;
            tx.open_table(ADDR_TO_HASH)?;
            tx.open_table(HASH_TO_CODE)?;
            tx.commit()?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                mem: DashMap::new(),
                path,
            }),
        })
    }

    /// Path the cache database lives at. Useful for log lines / debug.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Read-only lookup. Returns `(code_hash, bytecode)` if a cached entry
    /// exists in memory or on disk, otherwise `None`.
    ///
    /// On a disk hit, the entry is promoted into the memory tier so the
    /// next call is served without touching disk.
    pub fn get(&self, addr: Address) -> Option<(B256, Bytecode)> {
        if let Some(v) = self.inner.mem.get(&addr) {
            return Some(v.value().clone());
        }
        match self.disk_get(addr) {
            Ok(Some((hash, code))) => {
                let bytecode =
                    Bytecode::new_raw(revm::primitives::Bytes::copy_from_slice(&code));
                let entry = (hash, bytecode);
                self.inner.mem.insert(addr, entry.clone());
                Some(entry)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(addr = %addr, error = %e, "bytecode cache: disk read failed");
                None
            }
        }
    }

    /// Insert a fresh `(address, hash, bytecode)` tuple into both tiers.
    /// Skips the write if the address already maps to the same hash, which
    /// avoids redundant disk traffic on repeat fetches.
    pub fn put(&self, addr: Address, hash: B256, code: &[u8]) -> Result<(), CacheError> {
        let bytecode = Bytecode::new_raw(revm::primitives::Bytes::copy_from_slice(code));
        // Short-circuit when the in-memory tier already has the same hash for
        // this address — avoids redundant disk writes on repeat fetches.
        if let Some(existing) = self.inner.mem.get(&addr) {
            if existing.value().0 == hash {
                return Ok(());
            }
        }
        self.disk_put(addr, hash, code)?;
        self.inner.mem.insert(addr, (hash, bytecode));
        Ok(())
    }

    /// Memory-only lookup. Useful for hot-path probes where a disk miss
    /// would be wasteful (caller will fall through to RPC anyway).
    pub fn get_mem(&self, addr: Address) -> Option<(B256, Bytecode)> {
        self.inner.mem.get(&addr).map(|v| v.value().clone())
    }

    /// Hit-or-fetch helper: returns the cached entry if present, otherwise
    /// fetches the bytecode from `provider`, persists it, and returns it.
    /// `None` means the address has no code (EOA) or the provider failed.
    pub async fn get_or_fetch(
        &self,
        addr: Address,
        provider: &DynProvider<Ethereum>,
    ) -> Option<(B256, Bytecode)> {
        if let Some(hit) = self.get(addr) {
            return Some(hit);
        }
        match provider.get_code_at(addr).await {
            Ok(code) if !code.is_empty() => {
                let hash = alloy::primitives::keccak256(&code);
                if let Err(e) = self.put(addr, hash, &code) {
                    warn!(%addr, error = %e, "bytecode cache: persist failed; serving fetched value");
                }
                let bytecode =
                    Bytecode::new_raw(revm::primitives::Bytes::copy_from_slice(&code));
                Some((hash, bytecode))
            }
            Ok(_) => None,
            Err(e) => {
                warn!(%addr, error = %e, "bytecode cache: rpc fetch failed");
                None
            }
        }
    }

    /// Number of entries currently in the memory tier.
    pub fn mem_len(&self) -> usize {
        self.inner.mem.len()
    }

    // ── internal ──────────────────────────────────────────────────────

    fn disk_get(&self, addr: Address) -> Result<Option<(B256, Vec<u8>)>, CacheError> {
        let tx = self.inner.db.begin_read()?;
        let addr_table = tx.open_table(ADDR_TO_HASH)?;
        let Some(hash_bytes) = addr_table.get(addr.as_ref() as &[u8; 20])? else {
            return Ok(None);
        };
        let hash = B256::from(*hash_bytes.value());
        drop(addr_table);
        let code_table = tx.open_table(HASH_TO_CODE)?;
        let Some(code) = code_table.get(hash.as_ref() as &[u8; 32])? else {
            // Dangling addr -> hash with no payload: treat as a miss so the
            // caller will re-fetch and self-heal.
            debug!(%addr, %hash, "bytecode cache: dangling addr->hash, treating as miss");
            return Ok(None);
        };
        Ok(Some((hash, code.value().to_vec())))
    }

    fn disk_put(&self, addr: Address, hash: B256, code: &[u8]) -> Result<(), CacheError> {
        let tx = self.inner.db.begin_write()?;
        {
            let mut addr_table = tx.open_table(ADDR_TO_HASH)?;
            addr_table.insert(addr.as_ref() as &[u8; 20], hash.as_ref() as &[u8; 32])?;
        }
        {
            let mut code_table = tx.open_table(HASH_TO_CODE)?;
            code_table.insert(hash.as_ref() as &[u8; 32], code)?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn sample_bytecode() -> Vec<u8> {
        // A minimal but non-empty bytecode payload: PUSH1 0x60, PUSH1 0x40,
        // MSTORE — the prologue of practically every Solidity contract.
        vec![0x60, 0x60, 0x60, 0x40, 0x52]
    }

    #[test]
    fn open_creates_tables_idempotently() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp); // we only need a unique path; redb will create it.
        // First open creates schema.
        let c1 = BytecodeCache::open(&path).expect("first open");
        drop(c1);
        // Reopen must succeed without TableDoesNotExist.
        let _c2 = BytecodeCache::open(&path).expect("reopen");
    }

    #[test]
    fn put_then_get_roundtrip_memory() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = BytecodeCache::open(tmp.path()).unwrap();
        let addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let code = sample_bytecode();
        let hash = alloy::primitives::keccak256(&code);

        cache.put(addr, hash, &code).expect("put");
        let (h, bc) = cache.get(addr).expect("get hit");
        assert_eq!(h, hash);
        assert_eq!(bc.original_bytes().as_ref(), &code[..]);
        assert_eq!(cache.mem_len(), 1, "memory tier populated");
    }

    #[test]
    fn get_returns_none_when_missing() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = BytecodeCache::open(tmp.path()).unwrap();
        let addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        assert!(cache.get(addr).is_none());
        assert_eq!(cache.mem_len(), 0);
    }

    #[test]
    fn disk_persistence_across_reopen() {
        // Write to one cache instance, close it, reopen on the same path,
        // verify the value comes back from disk.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let code = sample_bytecode();
        let hash = alloy::primitives::keccak256(&code);

        {
            let cache = BytecodeCache::open(&path).unwrap();
            cache.put(addr, hash, &code).unwrap();
        }
        // Reopen — memory tier is empty, disk tier must serve the hit.
        let cache = BytecodeCache::open(&path).unwrap();
        assert_eq!(cache.mem_len(), 0, "memory tier starts cold after reopen");
        let (h, bc) = cache.get(addr).expect("disk hit");
        assert_eq!(h, hash);
        assert_eq!(bc.original_bytes().as_ref(), &code[..]);
        assert_eq!(cache.mem_len(), 1, "disk hit promotes to memory");
    }

    #[test]
    fn put_with_same_hash_is_idempotent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = BytecodeCache::open(tmp.path()).unwrap();
        let addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let code = sample_bytecode();
        let hash = alloy::primitives::keccak256(&code);

        cache.put(addr, hash, &code).unwrap();
        // Second insert with identical hash must short-circuit cleanly.
        cache.put(addr, hash, &code).unwrap();
        assert_eq!(cache.mem_len(), 1);
    }

    #[test]
    fn put_with_different_hash_overwrites() {
        // Defensive: the CREATE2-redeploy edge case. If the same address ever
        // reports a different code hash, the cache must record the new value
        // rather than silently serving stale bytecode that no longer matches
        // on-chain.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cache = BytecodeCache::open(tmp.path()).unwrap();
        let addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

        let code_a = sample_bytecode();
        let hash_a = alloy::primitives::keccak256(&code_a);
        cache.put(addr, hash_a, &code_a).unwrap();

        let code_b = vec![0x60, 0x80, 0x60, 0x40, 0x52, 0x00]; // different payload
        let hash_b = alloy::primitives::keccak256(&code_b);
        assert_ne!(hash_a, hash_b);
        cache.put(addr, hash_b, &code_b).unwrap();

        let (h, bc) = cache.get(addr).expect("hit");
        assert_eq!(h, hash_b, "latest hash wins");
        assert_eq!(bc.bytes_slice(), &code_b[..]);
    }

    #[test]
    fn get_mem_does_not_touch_disk() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let code = sample_bytecode();
        let hash = alloy::primitives::keccak256(&code);

        // Seed on disk, then reopen with cold memory tier.
        {
            let cache = BytecodeCache::open(&path).unwrap();
            cache.put(addr, hash, &code).unwrap();
        }
        let cache = BytecodeCache::open(&path).unwrap();
        // `get_mem` is explicitly memory-only and must miss before any
        // memory population.
        assert!(cache.get_mem(addr).is_none());
        // Full `get` then warms memory.
        let _ = cache.get(addr).unwrap();
        assert!(cache.get_mem(addr).is_some());
    }
}
