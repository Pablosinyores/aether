//! Trade-ledger access layer (Rust side).
//!
//! Two impls of [`Ledger`]:
//!
//! - [`NoopLedger`] — default, used when `DATABASE_URL` is unset. Discards
//!   every write so engine behaviour is identical to runs without Postgres.
//! - [`PgLedger`] — `sqlx::PgPool`-backed. Public methods are sync; each call
//!   enqueues onto a **bounded** mpsc and a single dedicated writer task drains
//!   it. The hot path never awaits I/O. Channel saturation drops the row and
//!   bumps `aether_ledger_drops_total{op}`; a slow Postgres can never exert
//!   unbounded backpressure on the engine.
//!
//! Observability surface (registered against a shared `prometheus::Registry`):
//!
//! | Metric | Type | Labels |
//! |---|---|---|
//! | `aether_ledger_writes_total` | Counter | `op`, `result` (`ok`/`err`) |
//! | `aether_ledger_drops_total` | Counter | `op` |
//! | `aether_ledger_queue_depth` | Gauge | — |
//! | `aether_ledger_write_latency_ms` | Histogram | `op` |
//!
//! See `migrations/0001_trade_ledger.sql` for the schema.

use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use alloy::primitives::{Address, B256, U256};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry,
};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

use crate::types::ProtocolType;

/// Channel depth between the engine hot path and the PgLedger writer task.
/// Sized for ~5 s of bursty inserts at the engine's 200 arbs/s peak before
/// saturating (1024 / 200 ≈ 5.12 s). Breached only when Postgres stalls; the
/// drops counter is the alert signal.
const LEDGER_CHANNEL_CAPACITY: usize = 1024;

/// Maximum simultaneous in-flight INSERTs. Matches the sqlx pool size so the
/// writer can saturate the pool without queueing on the connection acquire
/// path. Higher than the pool size = wasted spawns waiting for a connection;
/// lower = pool sits idle while writes serialise.
const LEDGER_MAX_INFLIGHT: usize = 8;

/// sqlx connection pool size. Kept identical to LEDGER_MAX_INFLIGHT so the
/// two are tuned in lockstep.
const LEDGER_POOL_SIZE: u32 = 8;

/// Insert payload for the `arbs` table.
///
/// Field shapes mirror the SQL schema 1:1 so the [`PgLedger`] impl maps
/// without extra conversion. `Default` exists so callers can build the struct
/// field by field without filling every column.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewArb {
    pub arb_id: Uuid,
    /// Event time — when the engine published the arb. Per the migration's
    /// clock-authority policy this column is CLIENT-SET; producers MUST
    /// populate it and never rely on the schema's `DEFAULT now()` fallback.
    pub ts: DateTime<Utc>,
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
    /// Event time — when the GetBundleStats poll resolved. Per the
    /// migration's clock-authority policy this column is CLIENT-SET and
    /// must be populated by the writer; the schema `DEFAULT now()` is a
    /// safety net for ad-hoc psql inserts only.
    pub resolved_at: DateTime<Utc>,
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

/// Prometheus handles shared with [`PgLedger`]. Constructed once at startup
/// against the engine's existing `Registry` so a single `/metrics` endpoint
/// emits both engine and ledger families.
pub struct LedgerMetrics {
    writes_total: IntCounterVec,
    drops_total: IntCounterVec,
    queue_depth: IntGauge,
    write_latency_ms: HistogramVec,
}

impl LedgerMetrics {
    /// Register all four ledger metrics on the provided `Registry`.
    ///
    /// Panics on register failure; this is startup code and a duplicate
    /// registration indicates a programmer error, not a runtime condition.
    pub fn register(registry: &Registry) -> Arc<Self> {
        let writes_total = IntCounterVec::new(
            Opts::new(
                "aether_ledger_writes_total",
                "Trade-ledger writes attempted by the writer task, by op and outcome",
            ),
            &["op", "result"],
        )
        .expect("aether_ledger_writes_total counter vec");
        let drops_total = IntCounterVec::new(
            Opts::new(
                "aether_ledger_drops_total",
                "Trade-ledger writes dropped because the bounded channel was full",
            ),
            &["op"],
        )
        .expect("aether_ledger_drops_total counter vec");
        let queue_depth = IntGauge::new(
            "aether_ledger_queue_depth",
            "Pending trade-ledger writes sitting in the writer-task channel",
        )
        .expect("aether_ledger_queue_depth gauge");
        let write_latency_ms = HistogramVec::new(
            HistogramOpts::new(
                "aether_ledger_write_latency_ms",
                "Per-op latency of trade-ledger writes from dequeue to query completion",
            )
            .buckets(vec![0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0]),
            &["op"],
        )
        .expect("aether_ledger_write_latency_ms histogram vec");

        registry
            .register(Box::new(writes_total.clone()))
            .expect("register aether_ledger_writes_total");
        registry
            .register(Box::new(drops_total.clone()))
            .expect("register aether_ledger_drops_total");
        registry
            .register(Box::new(queue_depth.clone()))
            .expect("register aether_ledger_queue_depth");
        registry
            .register(Box::new(write_latency_ms.clone()))
            .expect("register aether_ledger_write_latency_ms");

        Arc::new(Self {
            writes_total,
            drops_total,
            queue_depth,
            write_latency_ms,
        })
    }
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

/// One unit of ledger work shipped over the writer-task channel. Owns its
/// payload so the hot path can drop the original immediately.
enum LedgerOp {
    InsertArb(Box<NewArb>),
    InsertPool(Box<NewPool>),
    UpdateInclusion(Box<InclusionUpdate>),
}

impl LedgerOp {
    fn label(&self) -> &'static str {
        match self {
            LedgerOp::InsertArb(_) => "insert_arb",
            LedgerOp::InsertPool(_) => "insert_pool",
            LedgerOp::UpdateInclusion(_) => "update_inclusion",
        }
    }
}

/// Postgres-backed [`Ledger`] using `sqlx`.
///
/// The hot path enqueues onto a bounded channel; a single dedicated writer
/// task drains and executes. Channel saturation drops the row (with metric)
/// rather than blocking the engine. The connection pool is bounded so a slow
/// Postgres still cannot fan out unbounded backpressure even if it acquires
/// every slot.
#[derive(Clone)]
pub struct PgLedger {
    tx: mpsc::Sender<LedgerOp>,
    metrics: Arc<LedgerMetrics>,
}

impl PgLedger {
    /// Connect to Postgres and spawn the dedicated writer task.
    ///
    /// Returns once the pool is ready and the writer is live. The writer task
    /// runs until the channel closes (i.e. every clone of the `Sender` is
    /// dropped — typically at process shutdown).
    pub async fn connect(
        database_url: &str,
        metrics: Arc<LedgerMetrics>,
    ) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(LEDGER_POOL_SIZE)
            // Short acquire timeout: misconfigured DATABASE_URL should fail
            // boot in seconds, not block the engine while we wait. The
            // ledger_from_env wrapper falls back to NoopLedger on this
            // error, so a slow Postgres degrades gracefully instead of
            // stalling startup.
            .acquire_timeout(std::time::Duration::from_secs(2))
            .connect(database_url)
            .await?;

        let (tx, rx) = mpsc::channel::<LedgerOp>(LEDGER_CHANNEL_CAPACITY);
        spawn_writer(pool, rx, Arc::clone(&metrics));

        tracing::info!(
            target: "aether::ledger",
            channel_capacity = LEDGER_CHANNEL_CAPACITY,
            pool_size = LEDGER_POOL_SIZE,
            max_inflight = LEDGER_MAX_INFLIGHT,
            "PgLedger connected — trade ledger writes enabled"
        );
        Ok(Self { tx, metrics })
    }

    /// Common enqueue path: try_send, bump the right metric on the result.
    /// Never awaits — the hot path stays non-blocking.
    fn enqueue(&self, op: LedgerOp) {
        let label = op.label();
        match self.tx.try_send(op) {
            Ok(()) => {
                self.metrics.queue_depth.inc();
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics
                    .drops_total
                    .with_label_values(&[label])
                    .inc();
                tracing::warn!(
                    target: "aether::ledger",
                    op = label,
                    capacity = LEDGER_CHANNEL_CAPACITY,
                    "ledger channel full — dropping row"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task has exited; this happens only at shutdown.
                tracing::debug!(
                    target: "aether::ledger",
                    op = label,
                    "ledger channel closed; dropping row"
                );
            }
        }
    }
}

impl Ledger for PgLedger {
    fn insert_arb(&self, arb: &NewArb) {
        self.enqueue(LedgerOp::InsertArb(Box::new(arb.clone())));
    }

    fn insert_pool(&self, pool_row: &NewPool) {
        self.enqueue(LedgerOp::InsertPool(Box::new(pool_row.clone())));
    }

    /// `update_inclusion` is currently **unused on the engine side** — the Go
    /// executor owns inclusion writes (it's the side that polls
    /// `GetBundleStats`). This Rust path is reserved for a future
    /// reconciliation worker that backfills `inclusion_results` from
    /// on-chain block data when a builder API loses the race. Tests
    /// exercise the wire-up so the code stays compilable; no engine-side
    /// caller wires it yet.
    fn update_inclusion(&self, update: &InclusionUpdate) {
        self.enqueue(LedgerOp::UpdateInclusion(Box::new(update.clone())));
    }
}

/// Spawn the dedicated writer dispatcher. The dispatcher dequeues from `rx`
/// and fans each op out to a tokio task gated by a semaphore so up to
/// [`LEDGER_MAX_INFLIGHT`] writes execute concurrently across the sqlx pool's
/// connections. Sequential await on the writer side previously left every
/// connection but one idle; the semaphore matches concurrency to the pool.
fn spawn_writer(
    pool: PgPool,
    mut rx: mpsc::Receiver<LedgerOp>,
    metrics: Arc<LedgerMetrics>,
) {
    let semaphore = Arc::new(Semaphore::new(LEDGER_MAX_INFLIGHT));
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            metrics.queue_depth.dec();
            // Permit drops at task end, releasing one in-flight slot.
            let permit = match Arc::clone(&semaphore).acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    // Semaphore was closed; the dispatcher is shutting down.
                    break;
                }
            };
            let pool = pool.clone();
            let metrics = Arc::clone(&metrics);
            tokio::spawn(async move {
                let label = op.label();
                let timer = Instant::now();
                let result = match op {
                    LedgerOp::InsertArb(arb) => insert_arb_inner(&pool, &arb).await,
                    LedgerOp::InsertPool(p) => insert_pool_inner(&pool, &p).await,
                    LedgerOp::UpdateInclusion(u) => update_inclusion_inner(&pool, &u).await,
                };
                let elapsed_ms = timer.elapsed().as_secs_f64() * 1_000.0;
                metrics
                    .write_latency_ms
                    .with_label_values(&[label])
                    .observe(elapsed_ms);
                match result {
                    Ok(()) => {
                        metrics
                            .writes_total
                            .with_label_values(&[label, "ok"])
                            .inc();
                    }
                    Err(e) => {
                        metrics
                            .writes_total
                            .with_label_values(&[label, "err"])
                            .inc();
                        tracing::warn!(
                            target: "aether::ledger",
                            op = label,
                            error = %e,
                            elapsed_ms,
                            "ledger write failed; row dropped"
                        );
                    }
                }
                drop(permit);
            });
        }
        tracing::info!(target: "aether::ledger", "PgLedger writer dispatcher exiting");
    });
}

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
            arb_id, ts, target_block, path_hash, hops,
            path, protocols, pool_addresses,
            flashloan_token, flashloan_amount,
            gross_profit_wei, net_profit_wei,
            gas_estimate, tip_bps,
            detection_us, sim_us, git_sha
        ) VALUES (
            $1, $2, $3, $4, $5,
            $6, $7, $8,
            $9, $10,
            $11, $12,
            $13, $14,
            $15, $16, $17
        )
        ON CONFLICT (arb_id) DO NOTHING
        "#,
    )
    .bind(arb_id)
    .bind(arb.ts)
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

    // resolved_at is bound from the caller (CLIENT-SET, per the
    // clock-authority policy in 0001_trade_ledger.sql). Both insert and
    // update branches use the bound value so the column reflects when the
    // GetBundleStats poll resolved in code, not when the row hit Postgres.
    sqlx::query(
        r#"
        INSERT INTO inclusion_results (
            bundle_id, builder, included, included_block, landed_tx_hash, error, resolved_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
        )
        ON CONFLICT (bundle_id, builder) DO UPDATE SET
            included       = EXCLUDED.included,
            included_block = EXCLUDED.included_block,
            landed_tx_hash = EXCLUDED.landed_tx_hash,
            error          = EXCLUDED.error,
            resolved_at    = EXCLUDED.resolved_at
        "#,
    )
    .bind(u.bundle_id)
    .bind(&u.builder)
    .bind(u.included)
    .bind(included_block)
    .bind(landed)
    .bind(u.error.as_deref())
    .bind(u.resolved_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Build a [`Ledger`] from `DATABASE_URL`. Returns [`NoopLedger`] when the var
/// is unset or empty so the engine stays runnable in dev / CI without
/// Postgres.
pub async fn ledger_from_env(metrics: Arc<LedgerMetrics>) -> Arc<dyn Ledger> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => match PgLedger::connect(&url, metrics).await {
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
///
/// `expect`s the parse rather than masking with `unwrap_or(0)`: `U256::to_string`
/// emits a base-10 digit sequence which `BigDecimal::from_str` accepts by
/// definition. A failure here would mean the alloy / bigdecimal contract
/// changed under us — a programmer bug we want to surface loudly, not a
/// silent zero that quietly corrupts every arb's economics.
fn u256_to_decimal(v: U256) -> BigDecimal {
    let s = v.to_string();
    BigDecimal::from_str(&s)
        .expect("U256::to_string is always a valid base-10 BigDecimal input")
}

/// Stable on-disk name for a [`ProtocolType`]. Matches the serde enum tag so
/// rows written today and rows written by a future serde-driven impl stay
/// comparable. Public so the engine can use the same mapping when building
/// the JSONB `protocols` column on `NewArb` — keeping a single source of
/// truth for on-disk protocol names.
pub fn protocol_label(p: ProtocolType) -> &'static str {
    match p {
        ProtocolType::UniswapV2 => "UniswapV2",
        ProtocolType::UniswapV3 => "UniswapV3",
        ProtocolType::SushiSwap => "SushiSwap",
        ProtocolType::Curve => "Curve",
        ProtocolType::BalancerV2 => "BalancerV2",
        ProtocolType::BancorV3 => "BancorV3",
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

    #[test]
    fn ledger_metrics_register_round_trips() {
        let registry = Registry::new();
        let m = LedgerMetrics::register(&registry);
        // Exercise every path so a counter typo surfaces in CI.
        m.writes_total.with_label_values(&["insert_arb", "ok"]).inc();
        m.writes_total.with_label_values(&["insert_pool", "err"]).inc();
        m.drops_total.with_label_values(&["update_inclusion"]).inc();
        m.queue_depth.set(7);
        m.write_latency_ms
            .with_label_values(&["insert_arb"])
            .observe(2.5);

        let families = registry.gather();
        let names: Vec<_> = families.iter().map(|f| f.get_name().to_string()).collect();
        for required in [
            "aether_ledger_writes_total",
            "aether_ledger_drops_total",
            "aether_ledger_queue_depth",
            "aether_ledger_write_latency_ms",
        ] {
            assert!(
                names.iter().any(|n| n == required),
                "missing metric family {required}"
            );
        }
    }
}
