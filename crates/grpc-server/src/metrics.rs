use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, Registry, TextEncoder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

pub struct EngineMetrics {
    registry: Registry,
    detection_latency_ms: Histogram,
    cycles_detected: IntCounter,
    simulations_run: IntCounter,
    arbs_published: IntCounter,
    blocks_processed: IntCounter,
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
        let cycles_detected = IntCounter::new(
            "aether_cycles_detected_total",
            "Total negative cycles detected",
        )
        .expect("aether_cycles_detected_total counter");
        let simulations_run = IntCounter::new(
            "aether_simulations_run_total",
            "Total simulations executed",
        )
        .expect("aether_simulations_run_total counter");
        let arbs_published = IntCounter::new(
            "aether_arbs_published_total",
            "Total validated arbs published",
        )
        .expect("aether_arbs_published_total counter");
        let blocks_processed = IntCounter::new(
            "aether_blocks_processed_total",
            "Total blocks processed",
        )
        .expect("aether_blocks_processed_total counter");

        registry
            .register(Box::new(detection_latency_ms.clone()))
            .expect("register aether_detection_latency_ms");
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

        Self {
            registry,
            detection_latency_ms,
            cycles_detected,
            simulations_run,
            arbs_published,
            blocks_processed,
        }
    }

    pub fn observe_detection_latency_us(&self, us: u128) {
        let ms = us as f64 / 1000.0;
        self.detection_latency_ms.observe(ms);
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

    fn render(&self) -> Vec<u8> {
        let metric_families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        if encoder.encode(&metric_families, &mut buffer).is_err() {
            return b"".to_vec();
        }
        buffer
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
        metrics.inc_cycles_detected(2);
        metrics.inc_simulations_run(3);
        metrics.inc_arbs_published(4);
        metrics.inc_blocks_processed();

        let output = String::from_utf8(metrics.render()).expect("metrics output utf-8");

        for name in [
            "aether_detection_latency_ms",
            "aether_cycles_detected_total",
            "aether_simulations_run_total",
            "aether_arbs_published_total",
            "aether_blocks_processed_total",
        ] {
            assert!(output.contains(name), "missing metric {name}");
        }

        // Histogram emits _count and _sum
        assert!(output.contains("aether_detection_latency_ms_count 1"));
        assert!(output.contains("aether_detection_latency_ms_sum 3"));
        assert!(output.contains("aether_cycles_detected_total 2"));
        assert!(output.contains("aether_simulations_run_total 3"));
        assert!(output.contains("aether_arbs_published_total 4"));
        assert!(output.contains("aether_blocks_processed_total 1"));
    }
}
