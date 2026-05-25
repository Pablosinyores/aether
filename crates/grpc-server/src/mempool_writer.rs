//! Mempool prediction writer.
//!
//! Sibling pattern to `aether_common::db::PgLedger`: the hot path enqueues
//! a [`NewMempoolPrediction`] onto a bounded mpsc and returns; a dedicated
//! writer task drains the channel and runs `INSERT`s through `sqlx::PgPool`.
//! Channel saturation drops the row (with metric) so a slow Postgres can
//! never exert unbounded backpressure on the mempool decode pipeline.
//!
//! Independent of the trade ledger by design — distinct DSN
//! (`MEMPOOL_LEDGER_DSN` vs `DATABASE_URL`), distinct connection pool,
//! distinct metric namespace. An operator can enable mempool observability
//! without provisioning the executor schema and vice versa.
//!
//! Observability surface (registered against the engine's
//! `prometheus::Registry` so a single `/metrics` endpoint emits everything):
//!
//! | Metric | Type | Labels |
//! |---|---|---|
//! | `aether_mempool_predictions_persisted_total` | Counter | `protocol` |
//! | `aether_mempool_writer_drops_total` | Counter | — |
//! | `aether_mempool_writer_queue_depth` | Gauge | — |
//! | `aether_mempool_writer_write_latency_ms` | Histogram | `result` (`ok`/`err`) |
//!
//! See `migrations/0003_mempool_predictions.sql` for the schema.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use alloy::primitives::{Address, B256, U256};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use prometheus::{HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Channel depth between the decode pipeline and the writer task. Sized for
/// ~5 s of bursty pending-tx decodes at peak rate (~100 swaps/s sustained
/// during high mempool activity → 512 / 100 ≈ 5 s). Breached only when
/// Postgres stalls; the drops counter is the alert signal.
const WRITER_CHANNEL_CAPACITY: usize = 512;

/// sqlx connection pool size for the mempool writer. Sized below the trade
/// ledger's pool (4 vs 8) because the prediction insert is a smaller, fixed
/// shape with no per-arb cross-table writes — four connections saturate the
/// writer task without leaving the pool idle.
const WRITER_POOL_SIZE: u32 = 4;

/// Wire labels for the `protocol` column. Matches the rendering in issue
/// #131's schema body so SQL `WHERE protocol = 'uni_v2'` works without a
/// reverse mapping table. Kept in sync with [`decoder_protocol_label`] in
/// `mempool_pipeline.rs` — both must produce the same string per decoded
/// protocol.
pub const PROTOCOL_UNI_V2: &str = "uni_v2";
pub const PROTOCOL_SUSHI: &str = "sushi";
pub const PROTOCOL_UNI_V3: &str = "uni_v3";
/// Reserved for a future Curve decoder path. The router decoder rejects
/// every Curve calldata shape with `CurveUnsupported` today, so no writer
/// call ever lands here — but the constant documents the schema's
/// `protocol` TEXT domain so a future decoder addition does not introduce
/// a new wire label.
#[allow(dead_code)]
pub const PROTOCOL_CURVE: &str = "curve";
pub const PROTOCOL_BALANCER: &str = "balancer";
/// Reserved for Bancor V3 decoder path. Like `PROTOCOL_CURVE`, listed
/// here so the wire label is pinned even before the writer is wired up
/// downstream.
#[allow(dead_code)]
pub const PROTOCOL_BANCOR: &str = "bancor";

/// Insert payload for the `mempool_predictions` table. Field shapes mirror
/// the SQL schema 1:1 so a sqlx bind is a straight enumeration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMempoolPrediction {
    pub prediction_id: Uuid,
    /// Event time — when the decode pipeline accepted the pending tx.
    /// Per the migration's clock-authority policy this is CLIENT-SET and
    /// the writer MUST populate it; the schema's `DEFAULT now()` is a
    /// psql-level safety net only.
    pub decoded_at: DateTime<Utc>,
    pub pending_tx_hash: B256,
    pub router_address: Address,
    /// One of [`PROTOCOL_UNI_V2`] / [`PROTOCOL_SUSHI`] / [`PROTOCOL_UNI_V3`] /
    /// [`PROTOCOL_CURVE`] / [`PROTOCOL_BALANCER`]. Bound to `&'static str`
    /// (not [`String`]) so callers cannot invent values the reconciler is
    /// unprepared for.
    pub protocol: &'static str,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub pool_address: Option<Address>,
    pub predicted_target_block: u64,
    /// JSONB payload describing the post-state the analytical sim produced.
    /// Shape varies by protocol; consumers should match on `kind` first.
    /// See [`PredictedPostState`] for the writer-side helpers.
    pub predicted_post_state: serde_json::Value,
    /// `Some(f)` when the post-state Bellman-Ford scan found a profitable
    /// cycle; `None` when the scan ran but the result was unprofitable.
    pub profit_factor_predicted: Option<f64>,
    /// Reserved for the MEV-Share SSE path (issue #126) — Alchemy WS does
    /// not expose a builder-side timestamp today, so this is always `None`
    /// in the current pipeline. Kept on the payload so the schema and
    /// writer stay forward-compatible.
    pub detection_lead_ms: Option<i64>,
    pub engine_git_sha: Option<String>,
}

/// Convenience builder for the `predicted_post_state` JSONB column. The
/// reconciler (issue #131 Go half) and the profitability scorer (#132)
/// inspect `kind` first; per-variant fields then carry the protocol-specific
/// state. Kept here, in the writer crate, because every consumer is on the
/// Rust side today — emitting the JSON via [`serde_json::Value`] avoids a
/// generic enum dance on the read side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PredictedPostState {
    /// V2 / Sushi: constant-product post-state mapped onto graph reserves.
    /// `reserve_in` and `reserve_out` are the post-swap reserves in the
    /// affected pool, expressed as `f64` so the JSONB row matches what the
    /// price graph holds — the profitability scorer pulls these directly
    /// without unit conversion.
    V2 {
        reserve_in: f64,
        reserve_out: f64,
    },
    /// V3: analytical predictor result mapped onto the synthetic
    /// `(1.0, spot_price_post)` pair the price graph stores. The raw
    /// `new_sqrt_price_x96` is reserved for the scorer (PR-3) — emitting
    /// `reserve_in/out` matches the V2 case and keeps reconciler SQL simple.
    V3 {
        reserve_in: f64,
        reserve_out: f64,
    },
    /// Balancer equal-weight 2-token: balances map directly to graph
    /// reserves with the pool's fee factor applied at the graph layer.
    Balancer {
        reserve_in: f64,
        reserve_out: f64,
    },
}

impl PredictedPostState {
    pub fn into_json(self) -> serde_json::Value {
        serde_json::to_value(self).expect("PredictedPostState is always serialisable")
    }
}

/// Persistence boundary for mempool predictions.
///
/// `Send + Sync` so a single `Arc<dyn MempoolPredictionSink>` can fan out
/// to every decode task without further locking. Methods take `&self` and
/// are infallible from the caller's perspective — a connection blip must
/// never bring down the decode pipeline. Implementations log and drop.
pub trait MempoolPredictionSink: Send + Sync {
    fn insert_prediction(&self, prediction: NewMempoolPrediction);
}

/// Prometheus surface for the writer. Registered once at startup against
/// the engine's shared `Registry`.
pub struct MempoolWriterMetrics {
    persisted_total: IntCounterVec,
    drops_total: IntCounter,
    queue_depth: IntGauge,
    write_latency_ms: HistogramVec,
}

impl MempoolWriterMetrics {
    /// Register all writer metrics on the provided `Registry`.
    ///
    /// Panics on duplicate registration — this is startup code and a
    /// duplicate indicates a programmer error, not a runtime condition.
    pub fn register(registry: &Registry) -> Arc<Self> {
        let persisted_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_predictions_persisted_total",
                "Mempool predictions accepted by the writer task and queued for insert, by protocol",
            ),
            &["protocol"],
        )
        .expect("aether_mempool_predictions_persisted_total counter vec");
        let drops_total = IntCounter::new(
            "aether_mempool_writer_drops_total",
            "Mempool predictions dropped because the writer channel was full",
        )
        .expect("aether_mempool_writer_drops_total counter");
        let queue_depth = IntGauge::new(
            "aether_mempool_writer_queue_depth",
            "Pending mempool predictions sitting in the writer-task channel",
        )
        .expect("aether_mempool_writer_queue_depth gauge");
        let write_latency_ms = HistogramVec::new(
            HistogramOpts::new(
                "aether_mempool_writer_write_latency_ms",
                "Per-write latency of mempool prediction inserts from dequeue to query completion",
            )
            .buckets(vec![0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0]),
            &["result"],
        )
        .expect("aether_mempool_writer_write_latency_ms histogram vec");

        registry
            .register(Box::new(persisted_total.clone()))
            .expect("register aether_mempool_predictions_persisted_total");
        registry
            .register(Box::new(drops_total.clone()))
            .expect("register aether_mempool_writer_drops_total");
        registry
            .register(Box::new(queue_depth.clone()))
            .expect("register aether_mempool_writer_queue_depth");
        registry
            .register(Box::new(write_latency_ms.clone()))
            .expect("register aether_mempool_writer_write_latency_ms");

        Arc::new(Self {
            persisted_total,
            drops_total,
            queue_depth,
            write_latency_ms,
        })
    }
}

/// Default sink: discards every prediction. Used when `MEMPOOL_LEDGER_DSN`
/// is unset so the engine's mempool path is fully functional without
/// Postgres. Logs once on construction so operators can grep startup output
/// and rule out persistence as the reason rows are missing.
pub struct NoopMempoolSink;

impl NoopMempoolSink {
    pub fn new() -> Self {
        tracing::info!(
            target: "aether::mempool_writer",
            "MEMPOOL_LEDGER_DSN unset — mempool prediction writes disabled (no-op)"
        );
        Self
    }
}

impl Default for NoopMempoolSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MempoolPredictionSink for NoopMempoolSink {
    fn insert_prediction(&self, _prediction: NewMempoolPrediction) {}
}

/// Postgres-backed [`MempoolPredictionSink`].
///
/// The hot path enqueues onto a bounded channel; a single dedicated writer
/// task drains and executes inserts. Channel saturation drops the row (with
/// metric) rather than blocking the decoder. The connection pool is bounded
/// so a slow Postgres still cannot fan out unbounded backpressure even when
/// every connection is busy.
#[derive(Clone)]
pub struct PgMempoolWriter {
    tx: mpsc::Sender<NewMempoolPrediction>,
    metrics: Arc<MempoolWriterMetrics>,
}

impl PgMempoolWriter {
    /// Connect to Postgres and spawn the writer task. Returns once the pool
    /// is ready. The writer task exits when every clone of the `Sender` is
    /// dropped (typically at process shutdown).
    pub async fn connect(
        database_url: &str,
        metrics: Arc<MempoolWriterMetrics>,
    ) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(WRITER_POOL_SIZE)
            // Short acquire timeout: misconfigured DSN should fail boot in
            // seconds, not block the decoder while we wait. The
            // `mempool_writer_from_env` wrapper falls back to NoopSink on
            // this error so a slow Postgres degrades gracefully.
            .acquire_timeout(std::time::Duration::from_secs(2))
            .connect(database_url)
            .await?;

        let (tx, rx) = mpsc::channel::<NewMempoolPrediction>(WRITER_CHANNEL_CAPACITY);
        spawn_writer_task(pool, rx, Arc::clone(&metrics));

        tracing::info!(
            target: "aether::mempool_writer",
            channel_capacity = WRITER_CHANNEL_CAPACITY,
            pool_size = WRITER_POOL_SIZE,
            "PgMempoolWriter connected — mempool prediction writes enabled"
        );
        Ok(Self { tx, metrics })
    }
}

impl MempoolPredictionSink for PgMempoolWriter {
    fn insert_prediction(&self, prediction: NewMempoolPrediction) {
        let protocol = prediction.protocol;
        match self.tx.try_send(prediction) {
            Ok(()) => {
                self.metrics.queue_depth.inc();
                self.metrics
                    .persisted_total
                    .with_label_values(&[protocol])
                    .inc();
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.drops_total.inc();
                tracing::warn!(
                    target: "aether::mempool_writer",
                    capacity = WRITER_CHANNEL_CAPACITY,
                    "mempool writer channel full — dropping prediction"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task has exited; only happens at shutdown.
                tracing::debug!(
                    target: "aether::mempool_writer",
                    "mempool writer channel closed; dropping prediction"
                );
            }
        }
    }
}

/// Build a [`MempoolPredictionSink`] from `MEMPOOL_LEDGER_DSN`. Returns
/// [`NoopMempoolSink`] when the var is unset, empty, or the connection
/// fails — the decoder stays runnable in dev / CI without Postgres.
pub async fn mempool_writer_from_env(
    metrics: Arc<MempoolWriterMetrics>,
) -> Arc<dyn MempoolPredictionSink> {
    match std::env::var("MEMPOOL_LEDGER_DSN") {
        Ok(url) if !url.is_empty() => match PgMempoolWriter::connect(&url, metrics).await {
            Ok(w) => Arc::new(w) as Arc<dyn MempoolPredictionSink>,
            Err(e) => {
                tracing::error!(
                    target: "aether::mempool_writer",
                    error = %e,
                    "PgMempoolWriter connect failed; falling back to NoopMempoolSink"
                );
                Arc::new(NoopMempoolSink::new())
            }
        },
        _ => Arc::new(NoopMempoolSink::new()),
    }
}

/// Spawn the writer dispatcher. Sequential by design — the prediction
/// insert is a single-table `ON CONFLICT DO NOTHING` and the pool's
/// natural per-connection serialisation matches the per-pending-tx
/// ordering, so a semaphore-fanned-out variant (like the trade ledger
/// uses) would add machinery without throughput gain at the writer's
/// expected rate.
fn spawn_writer_task(
    pool: PgPool,
    mut rx: mpsc::Receiver<NewMempoolPrediction>,
    metrics: Arc<MempoolWriterMetrics>,
) {
    tokio::spawn(async move {
        while let Some(prediction) = rx.recv().await {
            metrics.queue_depth.dec();
            let timer = Instant::now();
            let result = insert_prediction_inner(&pool, &prediction).await;
            let elapsed_ms = timer.elapsed().as_secs_f64() * 1_000.0;
            let label = if result.is_ok() { "ok" } else { "err" };
            metrics
                .write_latency_ms
                .with_label_values(&[label])
                .observe(elapsed_ms);
            if let Err(e) = result {
                tracing::warn!(
                    target: "aether::mempool_writer",
                    error = %e,
                    elapsed_ms,
                    tx_hash = %prediction.pending_tx_hash,
                    "mempool prediction insert failed; row dropped"
                );
            }
        }
        tracing::info!(
            target: "aether::mempool_writer",
            "PgMempoolWriter dispatcher exiting"
        );
    });
}

async fn insert_prediction_inner(
    pool: &PgPool,
    p: &NewMempoolPrediction,
) -> Result<(), sqlx::Error> {
    let predicted_target_block = i64::try_from(p.predicted_target_block).unwrap_or(i64::MAX);
    let amount_in = u256_to_decimal(p.amount_in);
    let pool_address_bytes = p.pool_address.as_ref().map(|a| a.as_slice());

    sqlx::query(
        r#"
        INSERT INTO mempool_predictions (
            prediction_id, decoded_at, pending_tx_hash, router_address, protocol,
            token_in, token_out, amount_in, pool_address,
            predicted_target_block, predicted_post_state, profit_factor_predicted,
            detection_lead_ms, engine_git_sha
        ) VALUES (
            $1, $2, $3, $4, $5,
            $6, $7, $8, $9,
            $10, $11, $12,
            $13, $14
        )
        ON CONFLICT (pending_tx_hash) DO NOTHING
        "#,
    )
    .bind(p.prediction_id)
    .bind(p.decoded_at)
    .bind(p.pending_tx_hash.as_slice())
    .bind(p.router_address.as_slice())
    .bind(p.protocol)
    .bind(p.token_in.as_slice())
    .bind(p.token_out.as_slice())
    .bind(&amount_in)
    .bind(pool_address_bytes)
    .bind(predicted_target_block)
    .bind(&p.predicted_post_state)
    .bind(p.profit_factor_predicted)
    .bind(p.detection_lead_ms)
    .bind(p.engine_git_sha.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Map a U256 to the `NUMERIC(78,0)` representation sqlx accepts via
/// [`BigDecimal`]. Identical to the trade-ledger helper; pinned here to
/// keep the writer self-contained.
fn u256_to_decimal(v: U256) -> BigDecimal {
    let s = v.to_string();
    BigDecimal::from_str(&s)
        .expect("U256::to_string is always a valid base-10 BigDecimal input")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory sink used by the pipeline test to assert that a row was
    /// produced without standing up Postgres.
    pub(crate) struct CapturingSink {
        pub seen: Mutex<Vec<NewMempoolPrediction>>,
    }

    impl CapturingSink {
        pub fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl MempoolPredictionSink for CapturingSink {
        fn insert_prediction(&self, prediction: NewMempoolPrediction) {
            self.seen.lock().expect("capturing sink poisoned").push(prediction);
        }
    }

    fn sample_prediction() -> NewMempoolPrediction {
        NewMempoolPrediction {
            prediction_id: Uuid::new_v4(),
            decoded_at: Utc::now(),
            pending_tx_hash: B256::ZERO,
            router_address: Address::ZERO,
            protocol: PROTOCOL_UNI_V2,
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount_in: U256::from(1_000_000u64),
            pool_address: Some(Address::ZERO),
            predicted_target_block: 19_000_001,
            predicted_post_state: PredictedPostState::V2 {
                reserve_in: 1_000.0,
                reserve_out: 2_000.0,
            }
            .into_json(),
            profit_factor_predicted: Some(0.0042),
            detection_lead_ms: None,
            engine_git_sha: Some("deadbeef".to_string()),
        }
    }

    #[test]
    fn noop_sink_accepts_writes_silently() {
        let sink = NoopMempoolSink::new();
        sink.insert_prediction(sample_prediction());
    }

    #[test]
    fn noop_sink_is_object_safe() {
        let _: Box<dyn MempoolPredictionSink> = Box::new(NoopMempoolSink::new());
    }

    #[test]
    fn predicted_post_state_round_trips_through_json() {
        for original in [
            PredictedPostState::V2 {
                reserve_in: 1.5,
                reserve_out: 2.5,
            },
            PredictedPostState::V3 {
                reserve_in: 1.0,
                reserve_out: 1.234e18,
            },
            PredictedPostState::Balancer {
                reserve_in: 10.0,
                reserve_out: 20.0,
            },
        ] {
            let json = serde_json::to_value(&original).expect("serialize");
            let kind = json.get("kind").and_then(|v| v.as_str()).expect("kind present");
            // `kind` lives under `#[serde(rename_all = "snake_case")]` so a
            // future refactor that drops the rename surfaces here.
            assert!(
                ["v2", "v3", "balancer"].contains(&kind),
                "unexpected kind {kind}"
            );
            let parsed: PredictedPostState = serde_json::from_value(json).expect("deserialize");
            // Re-serialise both and compare strings — partial_eq via f64 is
            // brittle but the JSON form is stable.
            assert_eq!(
                serde_json::to_string(&parsed).expect("re-serialize"),
                serde_json::to_string(&original).expect("re-serialize-original"),
            );
        }
    }

    #[test]
    fn capturing_sink_records_every_insert() {
        let sink = CapturingSink::new();
        sink.insert_prediction(sample_prediction());
        sink.insert_prediction(sample_prediction());
        assert_eq!(sink.seen.lock().expect("capturing sink poisoned").len(), 2);
    }

    #[test]
    fn metrics_register_round_trips() {
        let registry = Registry::new();
        let m = MempoolWriterMetrics::register(&registry);
        m.persisted_total.with_label_values(&[PROTOCOL_UNI_V2]).inc();
        m.drops_total.inc();
        m.queue_depth.set(3);
        m.write_latency_ms.with_label_values(&["ok"]).observe(1.5);

        let names: Vec<_> = registry
            .gather()
            .iter()
            .map(|f| f.get_name().to_string())
            .collect();
        for required in [
            "aether_mempool_predictions_persisted_total",
            "aether_mempool_writer_drops_total",
            "aether_mempool_writer_queue_depth",
            "aether_mempool_writer_write_latency_ms",
        ] {
            assert!(
                names.iter().any(|n| n == required),
                "missing metric family {required}"
            );
        }
    }

    #[tokio::test]
    async fn mempool_writer_from_env_falls_back_when_dsn_unset() {
        // Save/restore so the test does not leak state into siblings.
        let prev = std::env::var("MEMPOOL_LEDGER_DSN").ok();
        // SAFETY: tests in this crate run single-threaded against
        // `MEMPOOL_LEDGER_DSN`; no concurrent reader can observe the unset.
        unsafe {
            std::env::remove_var("MEMPOOL_LEDGER_DSN");
        }

        let registry = Registry::new();
        let metrics = MempoolWriterMetrics::register(&registry);
        let sink = mempool_writer_from_env(metrics).await;
        // Should not panic; should not write.
        sink.insert_prediction(sample_prediction());

        if let Some(v) = prev {
            // SAFETY: restored in the same single-threaded test scope.
            unsafe {
                std::env::set_var("MEMPOOL_LEDGER_DSN", v);
            }
        }
    }
}
