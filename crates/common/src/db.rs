//! Trade-ledger access layer (Rust side).
//!
//! Ships the [`Ledger`] trait and a [`NoopLedger`] default. The real
//! `sqlx::PgPool`-backed impl plus engine wiring on the `ARB PUBLISHED` path
//! land in a follow-up. Today the engine can depend on the trait without
//! pulling `sqlx`, keeping the default build and `DATABASE_URL`-unset
//! behaviour identical to current `main`.
//!
//! See `migrations/0001_trade_ledger.sql` for the schema this trait targets.
//!
//! ```ignore
//! use aether_common::db::{Ledger, NoopLedger, NewArb};
//!
//! let ledger: Box<dyn Ledger> = Box::new(NoopLedger);
//! ledger.insert_arb(&NewArb::default());  // no-op, no panic
//! ```

use std::sync::OnceLock;

use alloy::primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

use crate::types::ProtocolType;

/// Insert payload for the `arbs` table.
///
/// Field shapes mirror the SQL schema 1:1 so a Postgres-backed `Ledger` impl
/// can map without extra conversion. `Default` exists so callers can build
/// the struct field by field without filling in every column.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewArb {
    pub arb_id: uuid_compat::Uuid,
    pub target_block: u64,
    pub path_hash: B256,
    pub hops: u8,
    pub path: serde_json::Value,
    pub protocols: serde_json::Value,
    pub pool_addresses: serde_json::Value,
    pub flashloan_token: Address,
    pub flashloan_amount: U256,
    pub gross_profit_wei: U256,
    pub net_profit_wei: U256,
    pub gas_estimate: u64,
    pub tip_bps: u32,
    pub detection_us: Option<u64>,
    pub sim_us: Option<u64>,
    pub git_sha: Option<String>,
}

/// Insert payload for the `pool_registry` table.
///
/// `protocol` is bound to [`ProtocolType`] (not `String`) so callers cannot
/// invent values the rest of the system does not understand. The Postgres
/// column stays `TEXT`; the impl serialises via `ProtocolType`'s serde
/// representation, giving a stable on-disk name without losing type safety.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewPool {
    pub address: Address,
    pub protocol: ProtocolType,
    pub token0: Address,
    pub token1: Address,
    pub fee_bps: Option<u32>,
    pub tier: Option<String>,
    pub source: String,
}

/// Update payload for the `inclusion_results` table — written when a
/// `GetBundleStats` poll resolves on the Go side. Surfaced here so a future
/// reconciliation job can backfill from Rust if needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InclusionUpdate {
    pub bundle_id: uuid_compat::Uuid,
    pub builder: String,
    pub included: bool,
    pub included_block: Option<u64>,
    pub landed_tx_hash: Option<B256>,
    pub error: Option<String>,
}

/// Persistence boundary for arb / pool / inclusion records.
///
/// Trait is `Send + Sync` so a single `Arc<dyn Ledger>` can be cloned to every
/// detector and ingestion task without further locking. Methods take `&self`
/// (no mutation) so the impl owns its own pool / connection synchronisation.
///
/// All methods are infallible from the caller's perspective — a connection
/// blip must never bring down the engine. Implementations log and drop.
pub trait Ledger: Send + Sync {
    fn insert_arb(&self, arb: &NewArb);
    fn insert_pool(&self, pool: &NewPool);
    fn update_inclusion(&self, update: &InclusionUpdate);
}

/// Default ledger: discards every write.
///
/// Logs once on construction so operators can grep for "ledger disabled" in
/// startup output and rule out persistence as the reason rows are missing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopLedger;

static NOOP_WARNED: OnceLock<()> = OnceLock::new();

impl NoopLedger {
    pub fn new() -> Self {
        NOOP_WARNED.get_or_init(|| {
            tracing::info!(
                target: "aether::ledger",
                "DATABASE_URL unset — trade ledger disabled (no-op writes)"
            );
        });
        Self
    }
}

impl Ledger for NoopLedger {
    fn insert_arb(&self, _arb: &NewArb) {}
    fn insert_pool(&self, _pool: &NewPool) {}
    fn update_inclusion(&self, _update: &InclusionUpdate) {}
}

/// Minimal UUID stand-in so this module does not pull a new workspace dep
/// today. A follow-up swaps this for `uuid::Uuid` once the `sqlx` / `uuid`
/// features land.
pub mod uuid_compat {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct Uuid(pub [u8; 16]);

    impl Uuid {
        pub const fn nil() -> Self {
            Self([0u8; 16])
        }

        pub const fn from_bytes(b: [u8; 16]) -> Self {
            Self(b)
        }

        pub const fn as_bytes(&self) -> &[u8; 16] {
            &self.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_ledger_silently_accepts_writes() {
        let ledger = NoopLedger::new();
        ledger.insert_arb(&NewArb::default());
        ledger.insert_pool(&NewPool::default());
        ledger.update_inclusion(&InclusionUpdate::default());
    }

    #[test]
    fn noop_ledger_is_object_safe() {
        let _: Box<dyn Ledger> = Box::new(NoopLedger::new());
    }
}
