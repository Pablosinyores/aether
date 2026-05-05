use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, Opts, Registry, TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

pub struct EngineMetrics {
    registry: Registry,
    detection_latency_ms: Histogram,
    simulation_latency_ms: Histogram,
    cycles_detected: IntCounter,
    simulations_run: IntCounter,
    arbs_published: IntCounter,
    blocks_processed: IntCounter,
    decode_errors: IntCounterVec,
    /// Pending DEX-router txs forwarded by the mempool subscription, labelled
    /// by router (raw address) and the decoded protocol family. The
    /// `decoded` label distinguishes successful ABI parses from
    /// `decode_failure` so dashboards can surface decoder gaps directly.
    pending_dex_tx_total: IntCounterVec,
    /// Reason-tagged decoder failure counter. Reasons match
    /// `aether_pools::router_decoder::DecodeError` variants so a dashboard
    /// drill-down points at the exact path that needs work next.
    pending_decode_errors_total: IntCounterVec,
    /// Profitable cycles found by the post-state mempool simulator, labelled
    /// by router and a coarse profit bucket. Counts candidates only — these
    /// are not validated arbs and never get submitted; they prove the
    /// post-state pipeline produces non-empty output on real traffic.
    pending_arb_candidates_total: IntCounterVec,
    /// Reasons the post-state simulator skipped a decoded swap (no pool in
    /// registry, missing token index, no graph edge, zero reserves, etc.).
    /// Mirrors `pending_decode_errors_total` for the layer above the decoder.
    pending_arb_sim_skipped_total: IntCounterVec,
    /// Pending-tx broadcast events the decode pipeline failed to receive
    /// because it lagged behind the producer (tokio broadcast `Lagged(n)`).
    /// Bumped by the `n` returned by the broadcast receiver so dashboards
    /// can show *how many events* were dropped, not just how many lag
    /// events fired. Sustained non-zero growth = pipeline is the bottleneck;
    /// either widen the channel or shed mempool sources.
    pending_pipeline_lagged_total: IntCounter,
}

impl EngineMetrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let detection_latency_ms = Histogram::with_opts(
            HistogramOpts::new(
                "aether_detection_latency_ms",
                "Detection latency in milliseconds",
            )
            .buckets(vec![0.1, 0.5, 1.0, 3.0, 5.0, 10.0, 50.0]),
        )
        .expect("aether_detection_latency_ms histogram");
        let simulation_latency_ms = Histogram::with_opts(
            HistogramOpts::new(
                "aether_simulation_latency_ms",
                "EVM simulation latency in milliseconds",
            )
            .buckets(vec![
                0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0,
            ]),
        )
        .expect("aether_simulation_latency_ms histogram");
        let cycles_detected = IntCounter::new(
            "aether_cycles_detected_total",
            "Total negative cycles detected",
        )
        .expect("aether_cycles_detected_total counter");
        let simulations_run =
            IntCounter::new("aether_simulations_run_total", "Total simulations executed")
                .expect("aether_simulations_run_total counter");
        let arbs_published = IntCounter::new(
            "aether_arbs_published_total",
            "Total validated arbs published",
        )
        .expect("aether_arbs_published_total counter");
        let blocks_processed =
            IntCounter::new("aether_blocks_processed_total", "Total blocks processed")
                .expect("aether_blocks_processed_total counter");
        let decode_errors = IntCounterVec::new(
            Opts::new(
                "aether_decode_errors_total",
                "Total logs the event decoder could not parse, labelled by reason",
            ),
            &["reason"],
        )
        .expect("aether_decode_errors_total counter vec");
        let pending_dex_tx_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_dex_tx_total",
                "Pending DEX-router txs forwarded by the mempool subscription, by router and decoded protocol",
            ),
            &["router", "protocol", "decoded"],
        )
        .expect("aether_pending_dex_tx_total counter vec");
        let pending_decode_errors_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_decode_errors_total",
                "Pending-tx calldata decoder failures, by reason",
            ),
            &["reason"],
        )
        .expect("aether_pending_decode_errors_total counter vec");
        let pending_arb_candidates_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_arb_candidates_total",
                "Profitable cycles found by the post-state mempool simulator, by router and profit bucket",
            ),
            &["router", "profit_bucket"],
        )
        .expect("aether_pending_arb_candidates_total counter vec");
        let pending_arb_sim_skipped_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_arb_sim_skipped_total",
                "Decoded swaps the post-state simulator skipped, by reason",
            ),
            &["reason"],
        )
        .expect("aether_pending_arb_sim_skipped_total counter vec");
        let pending_pipeline_lagged_total = IntCounter::new(
            "aether_pending_pipeline_lagged_total",
            "Pending-tx events dropped because the decode pipeline lagged behind the broadcast",
        )
        .expect("aether_pending_pipeline_lagged_total counter");

        registry
            .register(Box::new(detection_latency_ms.clone()))
            .expect("register aether_detection_latency_ms");
        registry
            .register(Box::new(simulation_latency_ms.clone()))
            .expect("register aether_simulation_latency_ms");
        registry
            .register(Box::new(cycles_detected.clone()))
            .expect("register aether_cycles_detected_total");
        registry
            .register(Box::new(simulations_run.clone()))
            .expect("register aether_simulations_run_total");
        registry
            .register(Box::new(arbs_published.clone()))
            .expect("register aether_arbs_published_total");
        registry
            .register(Box::new(blocks_processed.clone()))
            .expect("register aether_blocks_processed_total");
        registry
            .register(Box::new(decode_errors.clone()))
            .expect("register aether_decode_errors_total");
        registry
            .register(Box::new(pending_dex_tx_total.clone()))
            .expect("register aether_pending_dex_tx_total");
        registry
            .register(Box::new(pending_decode_errors_total.clone()))
            .expect("register aether_pending_decode_errors_total");
        registry
            .register(Box::new(pending_arb_candidates_total.clone()))
            .expect("register aether_pending_arb_candidates_total");
        registry
            .register(Box::new(pending_arb_sim_skipped_total.clone()))
            .expect("register aether_pending_arb_sim_skipped_total");
        registry
            .register(Box::new(pending_pipeline_lagged_total.clone()))
            .expect("register aether_pending_pipeline_lagged_total");

        Self {
            registry,
            detection_latency_ms,
            simulation_latency_ms,
            cycles_detected,
            simulations_run,
            arbs_published,
            blocks_processed,
            decode_errors,
            pending_dex_tx_total,
            pending_decode_errors_total,
            pending_arb_candidates_total,
            pending_arb_sim_skipped_total,
            pending_pipeline_lagged_total,
        }
    }

    pub fn observe_detection_latency_us(&self, us: u128) {
        let ms = us as f64 / 1000.0;
        self.detection_latency_ms.observe(ms);
    }

    pub fn observe_simulation_latency_us(&self, us: u128) {
        let ms = us as f64 / 1000.0;
        self.simulation_latency_ms.observe(ms);
    }

    pub fn inc_cycles_detected(&self, count: u64) {
        if count > 0 {
            self.cycles_detected.inc_by(count);
        }
    }

    pub fn inc_simulations_run(&self, count: u64) {
        if count > 0 {
            self.simulations_run.inc_by(count);
        }
    }

    pub fn inc_arbs_published(&self, count: u64) {
        if count > 0 {
            self.arbs_published.inc_by(count);
        }
    }

    pub fn inc_blocks_processed(&self) {
        self.blocks_processed.inc();
    }

    /// Bump `aether_decode_errors_total{reason="..."}` for the given reason.
    /// Labels come from `DecodeReason::as_str()` so the label set stays
    /// stable and enumerable for dashboards / alerts.
    pub fn inc_decode_errors(&self, reason: &str) {
        self.decode_errors.with_label_values(&[reason]).inc();
    }

    /// Borrow the underlying `Registry` so foreign metric families (e.g. the
    /// trade-ledger counters in `aether_common::db`) can register on the same
    /// scrape endpoint without standing up a second `/metrics` server.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Bump `aether_pending_dex_tx_total{router, protocol, decoded}` for a
    /// pending DEX-router tx the mempool source forwarded. `protocol` is
    /// `unknown` when decoding failed; `decoded` is `"true"` or `"false"`.
    pub fn inc_pending_dex_tx(&self, router: &str, protocol: &str, decoded: bool) {
        self.pending_dex_tx_total
            .with_label_values(&[router, protocol, if decoded { "true" } else { "false" }])
            .inc();
    }

    /// Bump `aether_pending_decode_errors_total{reason="..."}`. Reasons
    /// should be a small fixed set (`too_short`, `unknown_selector`,
    /// `abi_decode`, `empty_path`) so dashboards can rely on stable labels.
    pub fn inc_pending_decode_errors(&self, reason: &str) {
        self.pending_decode_errors_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Bump `aether_pending_arb_candidates_total{router, profit_bucket}`.
    /// Buckets are coarse (`<10bps`, `10-50bps`, `50-200bps`, `>200bps`) so
    /// the cardinality stays bounded.
    pub fn inc_pending_arb_candidates(&self, router: &str, profit_bucket: &str) {
        self.pending_arb_candidates_total
            .with_label_values(&[router, profit_bucket])
            .inc();
    }

    /// Bump `aether_pending_arb_sim_skipped_total{reason="..."}`.
    pub fn inc_pending_arb_sim_skipped(&self, reason: &str) {
        self.pending_arb_sim_skipped_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Add `n` to `aether_pending_pipeline_lagged_total`. Pass the count
    /// returned by `broadcast::error::RecvError::Lagged(n)` so the metric
    /// reflects events dropped, not lag events fired.
    pub fn add_pending_pipeline_lagged(&self, n: u64) {
        if n > 0 {
            self.pending_pipeline_lagged_total.inc_by(n);
        }
    }

    /// Render the registered metrics in Prometheus text exposition format.
    /// `pub(crate)` so sibling modules (`provider::tests`) can assert on
    /// rendered counter values without exposing the whole registry.
    pub(crate) fn render(&self) -> Vec<u8> {
        let metric_families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        if encoder.encode(&metric_families, &mut buffer).is_err() {
            return b"".to_vec();
        }
        buffer
    }
}

impl Default for EngineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

pub fn start_metrics_server(metrics: Arc<EngineMetrics>) {
    let addr = metrics_addr();

    tokio::spawn(async move {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                info!(%addr, "Metrics server listening");
                loop {
                    match listener.accept().await {
                        Ok((mut socket, _)) => {
                            let metrics = Arc::clone(&metrics);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(&mut socket, metrics).await {
                                    warn!(error = %e, "Metrics connection error");
                                }
                            });
                        }
                        Err(e) => {
                            warn!(error = %e, "Metrics accept failed");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to bind metrics server");
            }
        }
    });
}

async fn handle_connection(
    socket: &mut tokio::net::TcpStream,
    metrics: Arc<EngineMetrics>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
        Ok(result) => result?,
        Err(_) => return Ok(()),
    };
    if n == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    if path != "/metrics" {
        let response = "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
        socket.write_all(response.as_bytes()).await?;
        return Ok(());
    }

    let body = metrics.render();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await?;
    socket.write_all(&body).await?;
    Ok(())
}

fn metrics_addr() -> SocketAddr {
    if let Ok(addr) = std::env::var("RUST_METRICS_ADDR") {
        if let Ok(parsed) = addr.parse() {
            return parsed;
        }
    }

    let port = std::env::var("RUST_METRICS_PORT").unwrap_or_else(|_| "9092".to_string());
    format!("0.0.0.0:{port}")
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:9092".parse().expect("default metrics addr"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_render_contains_required_names() {
        let metrics = EngineMetrics::new();

        metrics.observe_detection_latency_us(3000); // 3ms
        metrics.observe_simulation_latency_us(5000); // 5ms
        metrics.inc_cycles_detected(2);
        metrics.inc_simulations_run(3);
        metrics.inc_arbs_published(4);
        metrics.inc_blocks_processed();
        metrics.inc_decode_errors("unknown_topic");
        metrics.inc_decode_errors("malformed_payload");
        metrics.inc_decode_errors("insufficient_topics");

        let output = String::from_utf8(metrics.render()).expect("metrics output utf-8");

        for name in [
            "aether_detection_latency_ms",
            "aether_simulation_latency_ms",
            "aether_cycles_detected_total",
            "aether_simulations_run_total",
            "aether_arbs_published_total",
            "aether_blocks_processed_total",
            "aether_decode_errors_total",
        ] {
            assert!(output.contains(name), "missing metric {name}");
        }

        // Histogram emits _count and _sum
        assert!(output.contains("aether_detection_latency_ms_count 1"));
        assert!(output.contains("aether_detection_latency_ms_sum 3"));
        assert!(output.contains("aether_simulation_latency_ms_count 1"));
        assert!(output.contains("aether_simulation_latency_ms_sum 5"));
        assert!(output.contains("aether_cycles_detected_total 2"));
        assert!(output.contains("aether_simulations_run_total 3"));
        assert!(output.contains("aether_arbs_published_total 4"));
        assert!(output.contains("aether_blocks_processed_total 1"));
        assert!(output.contains(r#"aether_decode_errors_total{reason="unknown_topic"} 1"#));
        assert!(output.contains(r#"aether_decode_errors_total{reason="malformed_payload"} 1"#));
        assert!(output.contains(r#"aether_decode_errors_total{reason="insufficient_topics"} 1"#));
    }
}
