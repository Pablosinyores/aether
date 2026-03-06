//! Historical block scanner: scan blocks for real arb opportunities using read-only RPC.
//!
//! No Anvil needed — uses `eth_call` at historical block numbers to query reserves.
//! Requires an archive node (or recent blocks within non-archive node retention).
//!
//! Prerequisites:
//! - `ETH_RPC_URL` environment variable (Ethereum mainnet RPC, ideally archive)
//!
//! Run with:
//!   ETH_RPC_URL="https://..." \
//!     cargo test -p aether-integration-tests --test historical_block_scanner_test -- --nocapture

mod common;

use std::time::Instant;

use alloy::providers::{Provider, ProviderBuilder};

use common::{
    build_price_graph, check_rpc_available, default_pool_set, fetch_all_reserves,
    run_detection, PoolDef,
};

// ── Scanner types ───────────────────────────────────────────────────

struct ScanConfig {
    pools: Vec<PoolDef>,
    max_hops: usize,
    bf_timeout_us: u64,
    _min_profit_factor: f64,
    rate_limit_delay_ms: u64,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            pools: default_pool_set(),
            max_hops: 4,
            bf_timeout_us: 5_000_000,
            _min_profit_factor: 0.001,
            rate_limit_delay_ms: 100,
        }
    }
}

#[derive(Debug)]
struct BlockScanResult {
    block_number: u64,
    pools_fetched: usize,
    cycles_detected: usize,
    best_profit_factor: f64,
    detection_us: u128,
}

// ── Scanner functions ───────────────────────────────────────────────

async fn scan_block(
    provider: &impl Provider,
    config: &ScanConfig,
    block_number: u64,
) -> BlockScanResult {
    let reserves = fetch_all_reserves(provider, &config.pools, Some(block_number)).await;

    let t_detect = Instant::now();
    let (graph, _token_index) = build_price_graph(&config.pools, &reserves);
    let cycles = run_detection(&graph, config.max_hops, config.bf_timeout_us);
    let detection_us = t_detect.elapsed().as_micros();

    let best_profit_factor = cycles
        .iter()
        .map(|c| c.profit_factor())
        .fold(0.0f64, f64::max);

    BlockScanResult {
        block_number,
        pools_fetched: reserves.len(),
        cycles_detected: cycles.len(),
        best_profit_factor,
        detection_us,
    }
}

async fn scan_block_range(
    provider: &impl Provider,
    config: &ScanConfig,
    start: u64,
    end: u64,
) -> Vec<BlockScanResult> {
    let mut results = Vec::new();

    for bn in start..=end {
        let result = scan_block(provider, config, bn).await;

        let arb_marker = if result.cycles_detected > 0 {
            format!(
                " ** {} cycles, best={:.4}%",
                result.cycles_detected,
                result.best_profit_factor * 100.0
            )
        } else {
            String::new()
        };

        eprintln!(
            "  Block {}: {} pools, detect={}us{}",
            result.block_number, result.pools_fetched, result.detection_us, arb_marker
        );

        results.push(result);

        if config.rate_limit_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(config.rate_limit_delay_ms)).await;
        }
    }

    results
}

// ── Tests ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_scan_recent_blocks() {
    if !check_rpc_available() {
        return;
    }

    let rpc_url = std::env::var("ETH_RPC_URL").unwrap();
    let parsed: url::Url = rpc_url.parse().expect("valid RPC URL");
    let provider = ProviderBuilder::new().connect_http(parsed);

    let latest = match provider.get_block_number().await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Failed to get latest block: {e}. Skipping.");
            return;
        }
    };

    let scan_count: u64 = 5;
    let start = latest.saturating_sub(scan_count - 1);

    eprintln!("=== Historical Block Scanner: Recent Blocks ===");
    eprintln!("Scanning blocks {} to {} (latest={})", start, latest, latest);

    let config = ScanConfig {
        rate_limit_delay_ms: 150,
        ..Default::default()
    };

    let t_total = Instant::now();
    let results = scan_block_range(&provider, &config, start, latest).await;
    let total_ms = t_total.elapsed().as_millis();

    let blocks_with_arbs = results.iter().filter(|r| r.cycles_detected > 0).count();
    let total_cycles: usize = results.iter().map(|r| r.cycles_detected).sum();
    let avg_detect_us: f64 = if results.is_empty() {
        0.0
    } else {
        results.iter().map(|r| r.detection_us as f64).sum::<f64>() / results.len() as f64
    };

    eprintln!("\n=== Scan Summary ===");
    eprintln!("  Blocks scanned:     {}", results.len());
    eprintln!("  Blocks with arbs:   {}", blocks_with_arbs);
    eprintln!("  Total cycles:       {}", total_cycles);
    eprintln!("  Avg detection:      {:.0}us", avg_detect_us);
    eprintln!("  Total time:         {}ms", total_ms);

    // Validate infrastructure — we should have fetched reserves for most pools
    for result in &results {
        assert!(
            result.pools_fetched >= 3,
            "Block {} should fetch at least 3 pools, got {}",
            result.block_number,
            result.pools_fetched
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scan_custom_range() {
    if !check_rpc_available() {
        return;
    }

    // Configurable via env vars (useful for testing specific ranges)
    let start = match std::env::var("SCAN_START_BLOCK") {
        Ok(s) => match s.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("Invalid SCAN_START_BLOCK, skipping custom range test");
                return;
            }
        },
        Err(_) => {
            eprintln!("SCAN_START_BLOCK not set, skipping custom range test");
            return;
        }
    };

    let end = match std::env::var("SCAN_END_BLOCK") {
        Ok(s) => s.parse::<u64>().unwrap_or(start + 4),
        Err(_) => start + 4,
    };

    let rpc_url = std::env::var("ETH_RPC_URL").unwrap();
    let parsed: url::Url = rpc_url.parse().expect("valid RPC URL");
    let provider = ProviderBuilder::new().connect_http(parsed);

    eprintln!("=== Historical Block Scanner: Custom Range ===");
    eprintln!("Scanning blocks {} to {}", start, end);

    let config = ScanConfig::default();

    let t_total = Instant::now();
    let results = scan_block_range(&provider, &config, start, end).await;
    let total_ms = t_total.elapsed().as_millis();

    let blocks_with_arbs = results.iter().filter(|r| r.cycles_detected > 0).count();

    eprintln!("\n=== Custom Range Scan Summary ===");
    eprintln!("  Blocks scanned:     {}", results.len());
    eprintln!("  Blocks with arbs:   {}", blocks_with_arbs);
    eprintln!("  Total time:         {}ms", total_ms);

    // Find the best block
    if let Some(best) = results.iter().max_by(|a, b| {
        a.best_profit_factor
            .partial_cmp(&b.best_profit_factor)
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        if best.cycles_detected > 0 {
            eprintln!(
                "  Best block:         {} ({} cycles, {:.4}% profit)",
                best.block_number,
                best.cycles_detected,
                best.best_profit_factor * 100.0
            );
        }
    }
}
