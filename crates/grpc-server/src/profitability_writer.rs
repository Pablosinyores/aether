//! Mempool profitability writer.
//!
//! Sibling of [`crate::mempool_writer`] (the predictions writer from PR
//! #133). Same shape: bounded mpsc → dedicated writer task → `sqlx::PgPool`,
//! drop-on-saturation, separate metric namespace. The two writers run in
//! distinct processes (engine vs scorer binary) so collapsing them into
//! one type would force the engine to link in scorer-only code.
//!
//! Reuses the trade-ledger DSN convention by reading `MEMPOOL_LEDGER_DSN`
//! — the profitability table lives in the same Postgres as predictions
//! and reconciliation, so a separate DSN would force operators to keep
//! three DSNs in sync for no benefit.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use alloy::primitives::U256;
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use prometheus::{HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::mpsc;
use uuid::Uuid;

const WRITER_CHANNEL_CAPACITY: usize = 256;
const WRITER_POOL_SIZE: u32 = 4;

/// Wire labels for the `decision` column. Matches the CHECK constraint in
/// `migrations/0005_mempool_profitability.sql`.
pub const DECISION_PROFITABLE: &str = "profitable";
pub const DECISION_UNPROFITABLE: &str = "unprofitable";
/// Reserved for the revm-fork-verify path (planned follow-up). Not emitted
/// by the v1 scorer; the constant is here so a future code path produces
/// the same wire label without re-typing it.
#[allow(dead_code)]
pub const DECISION_REVERTED: &str = "reverted";
pub const DECISION_NO_PATH: &str = "no_path";

/// Prometheus-only sub-label on `aether_mempool_profit_scored_total` that
/// distinguishes WHICH code path produced the decision. NOT persisted to
/// the `mempool_profitability` table (the migration's CHECK constraint
/// only covers `decision`). Cardinality is bounded; every reason here is
/// a `&'static str` so adding a new one requires touching this file.
pub const REASON_NA: &str = "n/a";
/// V2-only path: the exact-U256 walker (`verify_cycle_u256`, PR #136)
/// reached a verdict. Profitable / unprofitable / reverted variants all
/// share this reason.
pub const REASON_U256_WALKER: &str = "u256_walker";
/// f64 fallback path: the f64 net exceeded `MAX_PLAUSIBLE_F64_NET_WEI`
/// and was downgraded to `reverted` (PR #136 precision-bias guard).
/// Only ever pairs with `DECISION_REVERTED`.
pub const REASON_ABSURDITY_FLOOR: &str = "absurdity_floor";
/// V3-touching path: revm sim explicitly reverted/halted (PR #144). Only
/// ever pairs with `DECISION_REVERTED`. Distinct from `revm_verdict`
/// because the cycle ran through revm to completion rather than declining.
pub const REASON_REVM_REVERT: &str = "revm_revert";
/// V3-touching path: revm sim ran to completion with a non-reverting
/// verdict. Pairs with `DECISION_PROFITABLE` / `DECISION_UNPROFITABLE`.
pub const REASON_REVM_VERDICT: &str = "revm_verdict";

/// Insert payload for the `mempool_profitability` table.
///
/// `realized_profit_eth` is derived from `realized_profit_wei` at write
/// time inside the SQL bind, not carried separately on the payload, so
/// callers can't accidentally hand the writer mismatched wei + eth
/// values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProfitabilityScore {
    pub prediction_id: Uuid,
    /// Event time — when the scorer finished computing this row.
    pub scored_at: DateTime<Utc>,
    /// JSONB cycle: `[{"pool":"0x..","token_in":"0x..","token_out":"0x..","protocol":"uni_v2"}, ...]`.
    pub cycle_path: serde_json::Value,
    pub realized_profit_wei: U256,
    pub gas_estimate_wei: U256,
    /// `realized - gas`. The caller computes this once and passes both
    /// halves so the writer does not need a signed-arithmetic helper.
    /// Negative values are represented as the wei *deficit* with the
    /// `is_loss` flag set.
    pub net_profit_wei: i128,
    pub decision: &'static str,
    /// Prometheus-only sub-label: which code path emitted this decision.
    /// One of the `REASON_*` constants. Skipped during DB insert (the
    /// `mempool_profitability` table has no `reason` column).
    #[serde(default = "default_reason")]
    pub reason: &'static str,
    pub scoring_engine_git_sha: Option<String>,
}

fn default_reason() -> &'static str {
    REASON_NA
}

/// Sink trait. Object-safe so a single `Arc<dyn ProfitabilitySink>` can
/// fan out to multiple scoring tasks (currently only one runs at a time,
/// but the trait shape leaves room for a parallel batch scorer).
pub trait ProfitabilitySink: Send + Sync {
    fn insert_score(&self, score: NewProfitabilityScore);
}

/// Prometheus surface. Three families:
///   - `aether_mempool_profit_scored_total{decision}` — the headline
///     counter the dashboard pivots on.
///   - drops / queue_depth — writer-internal health.
///   - write_latency_ms — per-write latency by result.
pub struct ProfitabilityWriterMetrics {
    pub scored_total: IntCounterVec,
    pub drops_total: IntCounter,
    pub queue_depth: IntGauge,
    pub write_latency_ms: HistogramVec,
}

impl ProfitabilityWriterMetrics {
    pub fn register(registry: &Registry) -> Arc<Self> {
        let scored_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_profit_scored_total",
                "Confirmed predictions scored by the profitability scorer, by decision and reason (which code path produced the decision: u256_walker / absurdity_floor / revm_verdict / revm_revert / n/a).",
            ),
            &["decision", "reason"],
        )
        .expect("aether_mempool_profit_scored_total counter vec");
        let drops_total = IntCounter::new(
            "aether_mempool_profit_writer_drops_total",
            "Profitability writes dropped because the bounded channel was full",
        )
        .expect("aether_mempool_profit_writer_drops_total counter");
        let queue_depth = IntGauge::new(
            "aether_mempool_profit_writer_queue_depth",
            "Pending profitability writes sitting in the writer-task channel",
        )
        .expect("aether_mempool_profit_writer_queue_depth gauge");
        let write_latency_ms = HistogramVec::new(
            HistogramOpts::new(
                "aether_mempool_profit_writer_write_latency_ms",
                "Per-write latency of profitability inserts from dequeue to query completion",
            )
            .buckets(vec![0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0]),
            &["result"],
        )
        .expect("aether_mempool_profit_writer_write_latency_ms histogram vec");

        registry
            .register(Box::new(scored_total.clone()))
            .expect("register aether_mempool_profit_scored_total");
        registry
            .register(Box::new(drops_total.clone()))
            .expect("register aether_mempool_profit_writer_drops_total");
        registry
            .register(Box::new(queue_depth.clone()))
            .expect("register aether_mempool_profit_writer_queue_depth");
        registry
            .register(Box::new(write_latency_ms.clone()))
            .expect("register aether_mempool_profit_writer_write_latency_ms");

        Arc::new(Self {
            scored_total,
            drops_total,
            queue_depth,
            write_latency_ms,
        })
    }
}

/// Default sink: discards every write. Logs once on construction.
pub struct NoopProfitabilitySink;

impl NoopProfitabilitySink {
    pub fn new() -> Self {
        tracing::info!(
            target: "aether::profit_writer",
            "MEMPOOL_LEDGER_DSN unset — profitability writes disabled (no-op)"
        );
        Self
    }
}

impl Default for NoopProfitabilitySink {
    fn default() -> Self {
        Self::new()
    }
}

impl ProfitabilitySink for NoopProfitabilitySink {
    fn insert_score(&self, _score: NewProfitabilityScore) {}
}

/// Postgres-backed sink. Bounded mpsc + dedicated writer task; saturation
/// drops the row rather than blocking the scoring loop. Slow Postgres
/// cannot exert unbounded backpressure on the scorer.
#[derive(Clone)]
pub struct PgProfitabilityWriter {
    tx: mpsc::Sender<NewProfitabilityScore>,
    metrics: Arc<ProfitabilityWriterMetrics>,
}

impl PgProfitabilityWriter {
    pub async fn connect(
        database_url: &str,
        metrics: Arc<ProfitabilityWriterMetrics>,
    ) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(WRITER_POOL_SIZE)
            .acquire_timeout(std::time::Duration::from_secs(2))
            .connect(database_url)
            .await?;

        let (tx, rx) = mpsc::channel::<NewProfitabilityScore>(WRITER_CHANNEL_CAPACITY);
        spawn_writer_task(pool, rx, Arc::clone(&metrics));

        tracing::info!(
            target: "aether::profit_writer",
            channel_capacity = WRITER_CHANNEL_CAPACITY,
            pool_size = WRITER_POOL_SIZE,
            "PgProfitabilityWriter connected — profitability writes enabled"
        );
        Ok(Self { tx, metrics })
    }

    /// Read API for the scorer's poll loop. Returns confirmed predictions
    /// that have no profitability row yet. Bounded to `limit` so a backlog
    /// burst does not blow the scorer's memory; the loop drains a page
    /// per tick and the next tick picks up the rest.
    ///
    /// This is a separate concern from the write path (lookups are sync
    /// because they live on the scoring loop, not the writer task) so we
    /// expose a public pool handle. The handle is `Arc<PgPool>` clone-safe.
    pub async fn fetch_unscored_confirmed(
        pool: &PgPool,
        limit: i64,
    ) -> Result<Vec<UnscoredConfirmedPrediction>, sqlx::Error> {
        let rows = sqlx::query_as::<_, RawUnscored>(
            r#"
            SELECT
                p.prediction_id          AS prediction_id,
                p.protocol               AS protocol,
                p.pool_address           AS pool_address,
                p.token_in               AS token_in,
                p.token_out              AS token_out,
                p.amount_in              AS amount_in,
                r.actual_target_block    AS actual_target_block
            FROM mempool_predictions p
            JOIN mempool_reconciliation r USING (prediction_id)
            LEFT JOIN mempool_profitability sc USING (prediction_id)
            WHERE r.outcome = 'confirmed'
              AND r.actual_target_block IS NOT NULL
              AND p.pool_address IS NOT NULL
              AND sc.prediction_id IS NULL
            ORDER BY r.actual_target_block ASC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(UnscoredConfirmedPrediction::from).collect())
    }
}

impl ProfitabilitySink for PgProfitabilityWriter {
    fn insert_score(&self, score: NewProfitabilityScore) {
        let decision = score.decision;
        let reason = score.reason;
        match self.tx.try_send(score) {
            Ok(()) => {
                self.metrics.queue_depth.inc();
                self.metrics
                    .scored_total
                    .with_label_values(&[decision, reason])
                    .inc();
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.drops_total.inc();
                tracing::warn!(
                    target: "aether::profit_writer",
                    capacity = WRITER_CHANNEL_CAPACITY,
                    "profitability writer channel full — dropping score"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(
                    target: "aether::profit_writer",
                    "profitability writer channel closed; dropping score"
                );
            }
        }
    }
}

/// Build a [`ProfitabilitySink`] from `MEMPOOL_LEDGER_DSN`. Returns
/// [`NoopProfitabilitySink`] when the var is unset or the connection
/// fails.
pub async fn profit_writer_from_env(
    metrics: Arc<ProfitabilityWriterMetrics>,
) -> Arc<dyn ProfitabilitySink> {
    match std::env::var("MEMPOOL_LEDGER_DSN") {
        Ok(url) if !url.is_empty() => match PgProfitabilityWriter::connect(&url, metrics).await {
            Ok(w) => Arc::new(w) as Arc<dyn ProfitabilitySink>,
            Err(e) => {
                tracing::error!(
                    target: "aether::profit_writer",
                    error = %e,
                    "PgProfitabilityWriter connect failed; falling back to NoopProfitabilitySink"
                );
                Arc::new(NoopProfitabilitySink::new())
            }
        },
        _ => Arc::new(NoopProfitabilitySink::new()),
    }
}

fn spawn_writer_task(
    pool: PgPool,
    mut rx: mpsc::Receiver<NewProfitabilityScore>,
    metrics: Arc<ProfitabilityWriterMetrics>,
) {
    tokio::spawn(async move {
        while let Some(score) = rx.recv().await {
            metrics.queue_depth.dec();
            let timer = Instant::now();
            let result = insert_score_inner(&pool, &score).await;
            let elapsed_ms = timer.elapsed().as_secs_f64() * 1_000.0;
            let label = if result.is_ok() { "ok" } else { "err" };
            metrics
                .write_latency_ms
                .with_label_values(&[label])
                .observe(elapsed_ms);
            if let Err(e) = result {
                tracing::warn!(
                    target: "aether::profit_writer",
                    error = %e,
                    elapsed_ms,
                    prediction_id = %score.prediction_id,
                    "profitability insert failed; row dropped"
                );
            }
        }
        tracing::info!(
            target: "aether::profit_writer",
            "PgProfitabilityWriter dispatcher exiting"
        );
    });
}

async fn insert_score_inner(
    pool: &PgPool,
    s: &NewProfitabilityScore,
) -> Result<(), sqlx::Error> {
    let realized_wei = u256_to_decimal(s.realized_profit_wei);
    let gas_wei = u256_to_decimal(s.gas_estimate_wei);
    // net can be negative. BigDecimal supports signed values natively.
    let net_wei = BigDecimal::from(s.net_profit_wei);
    // realized_eth = realized_wei / 1e18 with full precision. BigDecimal
    // division at NUMERIC(38,18) precision is exact for inputs <= 1e60
    // wei, which is many orders of magnitude beyond ETH total supply.
    let realized_eth = BigDecimal::from_str(&s.realized_profit_wei.to_string())
        .expect("U256::to_string always parses as BigDecimal")
        / BigDecimal::from(1_000_000_000_000_000_000u64);

    sqlx::query(
        r#"
        INSERT INTO mempool_profitability (
            prediction_id, scored_at, cycle_path,
            realized_profit_wei, realized_profit_eth,
            gas_estimate_wei, net_profit_wei,
            decision, scoring_engine_git_sha
        ) VALUES (
            $1, $2, $3,
            $4, $5,
            $6, $7,
            $8, $9
        )
        ON CONFLICT (prediction_id) DO NOTHING
        "#,
    )
    .bind(s.prediction_id)
    .bind(s.scored_at)
    .bind(&s.cycle_path)
    .bind(&realized_wei)
    .bind(&realized_eth)
    .bind(&gas_wei)
    .bind(&net_wei)
    .bind(s.decision)
    .bind(s.scoring_engine_git_sha.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

fn u256_to_decimal(v: U256) -> BigDecimal {
    let s = v.to_string();
    BigDecimal::from_str(&s).expect("U256::to_string is always a valid BigDecimal input")
}

/// One row from `fetch_unscored_confirmed`. Carries enough state for the
/// scoring loop to fetch the pool's actual reserves at the prediction's
/// confirmed block and re-run the detector.
#[derive(Debug, Clone)]
pub struct UnscoredConfirmedPrediction {
    pub prediction_id: Uuid,
    pub protocol: String,
    pub pool_address: alloy::primitives::Address,
    pub token_in: alloy::primitives::Address,
    pub token_out: alloy::primitives::Address,
    pub amount_in: U256,
    pub actual_target_block: u64,
}

#[derive(sqlx::FromRow)]
struct RawUnscored {
    prediction_id: Uuid,
    protocol: String,
    pool_address: Vec<u8>,
    token_in: Vec<u8>,
    token_out: Vec<u8>,
    amount_in: BigDecimal,
    actual_target_block: i64,
}

impl From<RawUnscored> for UnscoredConfirmedPrediction {
    fn from(r: RawUnscored) -> Self {
        use alloy::primitives::Address;
        let to_addr = |b: &[u8]| -> Address {
            let mut arr = [0u8; 20];
            if b.len() == 20 {
                arr.copy_from_slice(b);
            }
            Address::from(arr)
        };
        let amount_in = U256::from_str(&r.amount_in.to_string()).unwrap_or(U256::ZERO);
        Self {
            prediction_id: r.prediction_id,
            protocol: r.protocol,
            pool_address: to_addr(&r.pool_address),
            token_in: to_addr(&r.token_in),
            token_out: to_addr(&r.token_out),
            amount_in,
            actual_target_block: r.actual_target_block.max(0) as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn noop_sink_silently_accepts_writes() {
        let sink = NoopProfitabilitySink::new();
        sink.insert_score(sample_score());
    }

    #[test]
    fn noop_sink_is_object_safe() {
        let _: Box<dyn ProfitabilitySink> = Box::new(NoopProfitabilitySink::new());
    }

    #[test]
    fn decision_constants_match_check_constraint() {
        // Pinned to the CHECK constraint in
        // migrations/0005_mempool_profitability.sql. A rename here without
        // a matching migration would make every insert fail with
        // SQLSTATE 23514.
        assert_eq!(DECISION_PROFITABLE, "profitable");
        assert_eq!(DECISION_UNPROFITABLE, "unprofitable");
        assert_eq!(DECISION_REVERTED, "reverted");
        assert_eq!(DECISION_NO_PATH, "no_path");
    }

    #[test]
    fn reason_constants_are_stable_wire_labels() {
        // Pinned because dashboards (Grafana mempool.json from PR #146)
        // hard-code these strings in PromQL queries that sum by
        // `reason`. Renames break the panels silently.
        assert_eq!(REASON_NA, "n/a");
        assert_eq!(REASON_U256_WALKER, "u256_walker");
        assert_eq!(REASON_ABSURDITY_FLOOR, "absurdity_floor");
        assert_eq!(REASON_REVM_REVERT, "revm_revert");
        assert_eq!(REASON_REVM_VERDICT, "revm_verdict");
    }

    #[test]
    fn metrics_register_round_trips() {
        let registry = Registry::new();
        let m = ProfitabilityWriterMetrics::register(&registry);
        m.scored_total
            .with_label_values(&[DECISION_PROFITABLE, REASON_U256_WALKER])
            .inc();
        m.scored_total
            .with_label_values(&[DECISION_NO_PATH, REASON_NA])
            .inc();
        m.scored_total
            .with_label_values(&[DECISION_REVERTED, REASON_ABSURDITY_FLOOR])
            .inc();
        m.scored_total
            .with_label_values(&[DECISION_REVERTED, REASON_REVM_REVERT])
            .inc();
        m.drops_total.inc();
        m.queue_depth.set(2);
        m.write_latency_ms.with_label_values(&["ok"]).observe(1.0);

        let names: Vec<_> = registry
            .gather()
            .iter()
            .map(|f| f.get_name().to_string())
            .collect();
        for required in [
            "aether_mempool_profit_scored_total",
            "aether_mempool_profit_writer_drops_total",
            "aether_mempool_profit_writer_queue_depth",
            "aether_mempool_profit_writer_write_latency_ms",
        ] {
            assert!(
                names.iter().any(|n| n == required),
                "missing metric family {required}"
            );
        }
    }

    fn sample_score() -> NewProfitabilityScore {
        NewProfitabilityScore {
            prediction_id: Uuid::new_v4(),
            scored_at: Utc::now(),
            cycle_path: serde_json::json!([
                {"pool":"0x0000000000000000000000000000000000000001","token_in":"0x0","token_out":"0x0","protocol":"uni_v2"}
            ]),
            realized_profit_wei: U256::from(1_000_000_000_000_000u64),
            gas_estimate_wei: U256::from(50_000_000_000_000u64),
            net_profit_wei: 950_000_000_000_000,
            decision: DECISION_PROFITABLE,
            reason: REASON_U256_WALKER,
            scoring_engine_git_sha: Some("deadbeef".to_string()),
        }
    }

    #[test]
    fn unscored_from_raw_handles_bytea_widths() {
        // RawUnscored.pool_address etc. are Vec<u8> from pgx; the From
        // impl must handle the 20-byte case and gracefully fall back on
        // anything else without panicking (a defensive guard against a
        // future schema migration that widens / narrows the bytea
        // columns).
        let raw = RawUnscored {
            prediction_id: Uuid::new_v4(),
            protocol: "uni_v2".to_string(),
            pool_address: vec![0xab; 20],
            token_in: vec![0xcd; 20],
            token_out: vec![0xef; 20],
            amount_in: BigDecimal::from(123_456u64),
            actual_target_block: 100,
        };
        let conv: UnscoredConfirmedPrediction = raw.into();
        assert_eq!(conv.actual_target_block, 100);
        assert_eq!(conv.amount_in, U256::from(123_456u64));
        let expected_pool = Address::from([0xab; 20]);
        assert_eq!(conv.pool_address, expected_pool);
    }
}
