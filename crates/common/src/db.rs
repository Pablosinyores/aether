//! Trade-ledger access layer (Rust side).
//!
//! Two impls of [`Ledger`]:
//!
//! - [`NoopLedger`] — default, used when `DATABASE_URL` is unset. Discards
//!   every write so engine behaviour is identical to runs without Postgres.
//! - [`PgLedger`] — `sqlx::PgPool`-backed. Public methods are sync; each call
//!   spawns a detached tokio task so the engine hot path never blocks on I/O.
//!   A connection blip logs and drops; the engine never crashes on ledger I/O.
//!
//! See `migrations/0001_trade_ledger.sql` for the schema.

use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use alloy::primitives::{Address, B256, U256};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

use crate::types::ProtocolType;

/// Insert payload for the `arbs` table.
///
/// Field shapes mirror the SQL schema 1:1 so the [`PgLedger`] impl maps
/// without extra conversion. `Default` exists so callers can build the struct
/// field by field without filling every column.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewArb {
    pub arb_id: Uuid,
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
/// column stays `TEXT`; [`PgLedger::insert_pool_inner`] serialises via
/// `protocol_label` (matching the serde tag), giving a stable on-disk name
/// without losing type safety.
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
    pub bundle_id: Uuid,
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

/// Postgres-backed [`Ledger`] using `sqlx`.
///
/// Each public call spawns a detached tokio task so the caller (typically the
/// engine on the hot path after `ARB PUBLISHED`) never awaits I/O. The pool
/// is bounded so a slow Postgres cannot fan out unbounded backpressure.
#[derive(Clone)]
pub struct PgLedger {
    pool: PgPool,
}

impl PgLedger {
    /// Connect to Postgres at `database_url` and return a ready ledger.
    ///
    /// Pool sizing matches the engine: a few simultaneous inserts are common
    /// but most blocks publish 0–5 arbs, so a small pool with idle timeout is
    /// enough. Callers should construct this once at startup and clone the
    /// `Arc` everywhere.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(database_url)
            .await?;
        tracing::info!(
            target: "aether::ledger",
            "PgLedger connected — trade ledger writes enabled"
        );
        Ok(Self { pool })
    }

    /// Fallible variant of [`Ledger::insert_arb`] used by the spawned task.
    async fn insert_arb_inner(pool: &PgPool, arb: &NewArb) -> Result<(), sqlx::Error> {
        let arb_id = arb.arb_id;
        let target_block = i64::try_from(arb.target_block).unwrap_or(i64::MAX);
        let path_hash = arb.path_hash.as_slice();
        let hops = i16::from(arb.hops);
        let flashloan_token = arb.flashloan_token.as_slice();
        let flashloan_amount = u256_to_decimal(arb.flashloan_amount);
        let gross_profit = u256_to_decimal(arb.gross_profit_wei);
        let net_profit = u256_to_decimal(arb.net_profit_wei);
        let gas_estimate = i64::try_from(arb.gas_estimate).unwrap_or(i64::MAX);
        let tip_bps = i32::try_from(arb.tip_bps).unwrap_or(i32::MAX);
        let detection_us = arb
            .detection_us
            .map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        let sim_us = arb.sim_us.map(|v| i64::try_from(v).unwrap_or(i64::MAX));

        sqlx::query(
            r#"
            INSERT INTO arbs (
                arb_id, target_block, path_hash, hops,
                path, protocols, pool_addresses,
                flashloan_token, flashloan_amount,
                gross_profit_wei, net_profit_wei,
                gas_estimate, tip_bps,
                detection_us, sim_us, git_sha
            ) VALUES (
                $1, $2, $3, $4,
                $5, $6, $7,
                $8, $9,
                $10, $11,
                $12, $13,
                $14, $15, $16
            )
            ON CONFLICT (arb_id) DO NOTHING
            "#,
        )
        .bind(arb_id)
        .bind(target_block)
        .bind(path_hash)
        .bind(hops)
        .bind(&arb.path)
        .bind(&arb.protocols)
        .bind(&arb.pool_addresses)
        .bind(flashloan_token)
        .bind(&flashloan_amount)
        .bind(&gross_profit)
        .bind(&net_profit)
        .bind(gas_estimate)
        .bind(tip_bps)
        .bind(detection_us)
        .bind(sim_us)
        .bind(arb.git_sha.as_deref())
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn insert_pool_inner(pool: &PgPool, np: &NewPool) -> Result<(), sqlx::Error> {
        let address = np.address.as_slice();
        let protocol = protocol_label(np.protocol);
        let token0 = np.token0.as_slice();
        let token1 = np.token1.as_slice();
        let fee_bps = np.fee_bps.map(|v| i32::try_from(v).unwrap_or(i32::MAX));

        sqlx::query(
            r#"
            INSERT INTO pool_registry (
                address, protocol, token0, token1, fee_bps, tier, source
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
            )
            ON CONFLICT (address) DO UPDATE
                SET last_seen = now()
            "#,
        )
        .bind(address)
        .bind(protocol)
        .bind(token0)
        .bind(token1)
        .bind(fee_bps)
        .bind(np.tier.as_deref())
        .bind(&np.source)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn update_inclusion_inner(
        pool: &PgPool,
        u: &InclusionUpdate,
    ) -> Result<(), sqlx::Error> {
        let included_block = u
            .included_block
            .map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        let landed = u.landed_tx_hash.as_ref().map(|h| h.as_slice());

        sqlx::query(
            r#"
            INSERT INTO inclusion_results (
                bundle_id, builder, included, included_block, landed_tx_hash, error
            ) VALUES (
                $1, $2, $3, $4, $5, $6
            )
            ON CONFLICT (bundle_id, builder) DO UPDATE SET
                included       = EXCLUDED.included,
                included_block = EXCLUDED.included_block,
                landed_tx_hash = EXCLUDED.landed_tx_hash,
                error          = EXCLUDED.error,
                resolved_at    = now()
            "#,
        )
        .bind(u.bundle_id)
        .bind(&u.builder)
        .bind(u.included)
        .bind(included_block)
        .bind(landed)
        .bind(u.error.as_deref())
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Borrow the underlying pool. Useful for read-only queries (reporters,
    /// integration tests) that want to share the connection budget.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Ledger for PgLedger {
    fn insert_arb(&self, arb: &NewArb) {
        let pool = self.pool.clone();
        let arb = arb.clone();
        spawn_detached(async move {
            if let Err(e) = PgLedger::insert_arb_inner(&pool, &arb).await {
                tracing::warn!(
                    target: "aether::ledger",
                    error = %e,
                    arb_id = %arb.arb_id,
                    "insert_arb failed; dropping row"
                );
            }
        });
    }

    fn insert_pool(&self, pool_row: &NewPool) {
        let pool = self.pool.clone();
        let row = pool_row.clone();
        spawn_detached(async move {
            if let Err(e) = PgLedger::insert_pool_inner(&pool, &row).await {
                tracing::warn!(
                    target: "aether::ledger",
                    error = %e,
                    pool = %row.address,
                    "insert_pool failed; dropping row"
                );
            }
        });
    }

    fn update_inclusion(&self, update: &InclusionUpdate) {
        let pool = self.pool.clone();
        let upd = update.clone();
        spawn_detached(async move {
            if let Err(e) = PgLedger::update_inclusion_inner(&pool, &upd).await {
                tracing::warn!(
                    target: "aether::ledger",
                    error = %e,
                    bundle_id = %upd.bundle_id,
                    "update_inclusion failed; dropping row"
                );
            }
        });
    }
}

/// Build a [`Ledger`] from `DATABASE_URL`. Returns [`NoopLedger`] when the var
/// is unset or empty so the engine stays runnable in dev / CI without
/// Postgres.
pub async fn ledger_from_env() -> Arc<dyn Ledger> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => match PgLedger::connect(&url).await {
            Ok(p) => Arc::new(p) as Arc<dyn Ledger>,
            Err(e) => {
                tracing::error!(
                    target: "aether::ledger",
                    error = %e,
                    "PgLedger connect failed; falling back to NoopLedger"
                );
                Arc::new(NoopLedger::new())
            }
        },
        _ => Arc::new(NoopLedger::new()),
    }
}

/// Map a U256 to the `NUMERIC(78,0)` representation sqlx accepts via
/// [`BigDecimal`]. U256::MAX has 78 decimal digits, which fits.
fn u256_to_decimal(v: U256) -> BigDecimal {
    BigDecimal::from_str(&v.to_string()).unwrap_or_else(|_| BigDecimal::from(0))
}

/// Stable on-disk name for a [`ProtocolType`]. Matches the serde enum tag so
/// rows written today and rows written by a future serde-driven impl stay
/// comparable.
fn protocol_label(p: ProtocolType) -> &'static str {
    match p {
        ProtocolType::UniswapV2 => "UniswapV2",
        ProtocolType::UniswapV3 => "UniswapV3",
        ProtocolType::SushiSwap => "SushiSwap",
        ProtocolType::Curve => "Curve",
        ProtocolType::BalancerV2 => "BalancerV2",
        ProtocolType::BancorV3 => "BancorV3",
    }
}

/// Spawn a future on the current tokio runtime if one exists; otherwise log
/// and drop. The engine always runs under tokio so the drop branch is dev /
/// test only.
fn spawn_detached<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
    } else {
        tracing::debug!(
            target: "aether::ledger",
            "no tokio runtime; dropping ledger write"
        );
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

    #[test]
    fn u256_to_decimal_max() {
        let max = U256::MAX;
        let d = u256_to_decimal(max);
        assert_eq!(d.to_string(), max.to_string());
    }

    #[test]
    fn protocol_label_matches_serde_tag() {
        for (p, expected) in [
            (ProtocolType::UniswapV2, "UniswapV2"),
            (ProtocolType::UniswapV3, "UniswapV3"),
            (ProtocolType::SushiSwap, "SushiSwap"),
            (ProtocolType::Curve, "Curve"),
            (ProtocolType::BalancerV2, "BalancerV2"),
            (ProtocolType::BancorV3, "BancorV3"),
        ] {
            assert_eq!(protocol_label(p), expected);
            // Pin the static label to the serde tag so a future serde-driven
            // query path produces the same on-disk value.
            let serde_repr = serde_json::to_string(&p).expect("serde");
            assert_eq!(serde_repr, format!("\"{expected}\""));
        }
    }
}
