use std::net::SocketAddr;
use std::sync::Arc;

use prometheus::{Encoder, IntCounter, IntGauge, Registry, TextEncoder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

pub struct EngineMetrics {
    registry: Registry,
    detection_latency_us: IntGauge,
    cycles_detected: IntCounter,
    simulations_run: IntCounter,
    arbs_published: IntCounter,
    blocks_processed: IntCounter,
}

impl EngineMetrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let detection_latency_us = IntGauge::new(
            "detection_latency_us",
            "Detection latency in microseconds",
        )
        .expect("detection_latency_us gauge");
        let cycles_detected = IntCounter::new(
            "cycles_detected",
            "Total negative cycles detected",
        )
        .expect("cycles_detected counter");
        let simulations_run = IntCounter::new(
            "simulations_run",
            "Total simulations executed",
        )
        .expect("simulations_run counter");
        let arbs_published = IntCounter::new(
            "arbs_published",
            "Total validated arbs published",
        )
        .expect("arbs_published counter");
        let blocks_processed = IntCounter::new(
            "blocks_processed",
            "Total blocks processed",
        )
        .expect("blocks_processed counter");

        registry
            .register(Box::new(detection_latency_us.clone()))
            .expect("register detection_latency_us");
        registry
            .register(Box::new(cycles_detected.clone()))
            .expect("register cycles_detected");
        registry
            .register(Box::new(simulations_run.clone()))
            .expect("register simulations_run");
        registry
            .register(Box::new(arbs_published.clone()))
            .expect("register arbs_published");
        registry
            .register(Box::new(blocks_processed.clone()))
            .expect("register blocks_processed");

        Self {
            registry,
            detection_latency_us,
            cycles_detected,
            simulations_run,
            arbs_published,
            blocks_processed,
        }
    }

    pub fn set_detection_latency_us(&self, value: u128) {
        let clamped = if value > i64::MAX as u128 {
            i64::MAX
        } else {
            value as i64
        };
        self.detection_latency_us.set(clamped);
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
    let n = socket.read(&mut buf).await?;
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
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        socket.write_all(response.as_bytes()).await?;
        return Ok(());
    }

    let body = metrics.render();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n",
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
