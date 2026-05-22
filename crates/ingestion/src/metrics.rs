//! Prometheus surface for the mempool ingestion (subscription) layer.
//!
//! This is the metrics module the mempool *transport* path uses. The
//! decode pipeline's funnel counters live downstream in the gRPC engine
//! (`aether_grpc_server::metrics::EngineMetrics`), but a handful of signals
//! can only be observed at the subscription boundary — before any
//! `PendingTxEvent` is broadcast — and so must be counted here.
//!
//! Registered against the engine's shared `prometheus::Registry` exactly
//! like [`aether_common::db::LedgerMetrics`] so a single `/metrics` endpoint
//! emits everything. The struct is cheap to clone-via-`Arc` and is handed to
//! [`crate::mempool::AlchemyMempool`] at construction.

use std::sync::Arc;

use prometheus::{IntCounter, Registry};

/// Metric families owned by the mempool ingestion layer.
pub struct MempoolIngestMetrics {
    raw_reencode_mismatch_total: IntCounter,
}

impl MempoolIngestMetrics {
    /// Register all ingestion metrics on the provided `Registry`.
    ///
    /// Panics on duplicate registration — this is startup code and a
    /// duplicate indicates a programmer error, not a runtime condition.
    pub fn register(registry: &Registry) -> Arc<Self> {
        let raw_reencode_mismatch_total = IntCounter::new(
            "aether_mempool_raw_reencode_mismatch_total",
            "Pending victim txs dropped because the canonical EIP-2718 re-encode \
             did not hash back to the subscription-reported tx hash",
        )
        .expect("aether_mempool_raw_reencode_mismatch_total counter");

        registry
            .register(Box::new(raw_reencode_mismatch_total.clone()))
            .expect("register aether_mempool_raw_reencode_mismatch_total");

        Arc::new(Self {
            raw_reencode_mismatch_total,
        })
    }

    /// Bump `aether_mempool_raw_reencode_mismatch_total`. Called when a
    /// re-encoded raw tx fails the keccak256 round-trip gate and the event
    /// is dropped at the subscription boundary.
    pub fn inc_raw_reencode_mismatch(&self) {
        self.raw_reencode_mismatch_total.inc();
    }

    /// Read the current value of `aether_mempool_raw_reencode_mismatch_total`.
    /// Public so tests can assert the gate fires without re-implementing
    /// Prometheus text parsing.
    pub fn raw_reencode_mismatch_count(&self) -> u64 {
        self.raw_reencode_mismatch_total.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_round_trips_and_increments() {
        let registry = Registry::new();
        let m = MempoolIngestMetrics::register(&registry);
        assert_eq!(m.raw_reencode_mismatch_count(), 0);
        m.inc_raw_reencode_mismatch();
        m.inc_raw_reencode_mismatch();
        assert_eq!(m.raw_reencode_mismatch_count(), 2);

        let names: Vec<_> = registry
            .gather()
            .iter()
            .map(|f| f.get_name().to_string())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n == "aether_mempool_raw_reencode_mismatch_total"),
            "missing metric family aether_mempool_raw_reencode_mismatch_total"
        );
    }
}
