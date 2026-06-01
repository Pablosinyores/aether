//! Hot-token discovery orchestration (Part 1).
//!
//! Periodically pulls the hottest mempool-observed token pairs from the
//! [`HotTokenTracker`], enriches each with on-chain venue facts (factory
//! lookups + a WETH-liquidity proxy), runs the fee-on-transfer / honeypot
//! round-trip screen in revm, applies the admission gate, and appends the
//! qualifying pools to `pools.toml` — then triggers an in-process registry
//! reload so the new pools take effect without a restart.
//!
//! The loop is **opt-in** (`AETHER_DISCOVERY=1`) and runs entirely off the
//! decode hot path on its own bounded interval. It admits to a live registry,
//! so it is capped (`AETHER_MAX_ADMITTED_POOLS`) and idempotent (the append
//! dedups by address). Intended for the shadow/analytical run — it never
//! submits anything.
//!
//! Scope notes for v1:
//! - Only WETH-paired candidates are screened (the round-trip screen needs the
//!   base token's balance slot; WETH = slot 3). Non-WETH pairs are rejected
//!   (`no_weth_base`) rather than admitted unscreened.
//! - The round-trip screen needs a UniV2/Sushi pair; a token with only V3
//!   venues cannot pass the screen and is left for a follow-up.
//! - Per-venue liquidity is a proxy: `2 × WETH_held_by_pool × AETHER_ETH_PRICE_USD`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy::network::Ethereum;
use alloy::primitives::aliases::U24;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::sol;
use alloy::sol_types::SolCall;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use aether_common::types::ProtocolType;
use aether_grpc_server::hot_token::HotTokenTracker;
use aether_grpc_server::pool_admission::{
    self, AdmissionConfig, AdmissionOutcome, CandidateVenue,
};
use aether_grpc_server::EngineMetrics;
use aether_simulator::fee_on_transfer::{self, FotConfig, RoundTripVerdict};
use aether_simulator::fork::RpcForkedState;

use crate::engine::AetherEngine;

sol! {
    function getPair(address tokenA, address tokenB) external view returns (address);
    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
    function balanceOf(address account) external view returns (uint256);
}

// Canonical mainnet factories + WETH.
const UNIV2_FACTORY: Address = address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
const SUSHI_FACTORY: Address = address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");
const UNIV3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// WETH's ERC20 `balances` mapping storage slot (used as the base in the screen).
const WETH_BALANCE_SLOT: u64 = 3;
/// UniswapV2 / SushiSwap pool fee in bps.
const V2_FEE_BPS: u32 = 30;
/// UniswapV3 fee tiers `(fee_1e-6, tick_spacing)`. `fee_bps = fee_1e-6 / 100`.
const V3_FEE_TIERS: [(u32, i32); 3] = [(500, 10), (3000, 60), (10000, 200)];

/// Tunables for the discovery loop. All overridable via env; defaults suit the
/// shadow run.
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    /// How often to run a discovery tick.
    pub interval: Duration,
    /// Top-N hot candidates considered per tick.
    pub max_candidates: usize,
    /// Hard ceiling on total pools in `pools.toml`; admission stops at the cap.
    pub max_admitted_pools: usize,
    /// Wall-clock budget for one fee-on-transfer round-trip screen. Generous on
    /// purpose — the screen is off the hot path and forks live state per call.
    pub screen_timeout: Duration,
    /// Round-trip recovery loss tolerance (bps) budgeting for price impact.
    pub max_loss_bps: u32,
    /// USD price of 1 ETH for the liquidity proxy.
    pub eth_price_usd: f64,
    /// Path to the hot-reloadable pool registry.
    pub pools_path: String,
    pub admission: AdmissionConfig,
    pub fot: FotConfig,
}

impl DiscoveryConfig {
    pub fn from_env(pools_path: String) -> Self {
        Self {
            interval: Duration::from_secs(env_u64("AETHER_DISCOVERY_INTERVAL_SECS", 60).max(1)),
            max_candidates: env_u64("AETHER_DISCOVERY_MAX_CANDIDATES", 10).max(1) as usize,
            max_admitted_pools: env_u64("AETHER_MAX_ADMITTED_POOLS", 500) as usize,
            screen_timeout: Duration::from_millis(env_u64("AETHER_FOT_SCREEN_TIMEOUT_MS", 8_000).max(100)),
            max_loss_bps: env_u64("AETHER_DISCOVERY_MAX_LOSS_BPS", 300) as u32,
            eth_price_usd: std::env::var("AETHER_ETH_PRICE_USD")
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(3_000.0),
            pools_path,
            admission: AdmissionConfig::default(),
            fot: FotConfig::default(),
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Spawn the discovery loop. Returns the task handle; the loop runs until the
/// `shutdown` watch flips to `true`.
pub fn spawn_discovery(
    engine: Arc<AetherEngine>,
    hot_tokens: Arc<HotTokenTracker>,
    provider: DynProvider<Ethereum>,
    metrics: Arc<EngineMetrics>,
    cfg: DiscoveryConfig,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cfg.interval);
        // Consume the immediate first tick so the tracker can accumulate before
        // the first real discovery pass.
        ticker.tick().await;
        let mut at_cap_logged = false;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    run_discovery_tick(&engine, &hot_tokens, &provider, &metrics, &cfg, &mut at_cap_logged).await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("discovery loop shutting down");
                        break;
                    }
                }
            }
        }
    })
}

async fn run_discovery_tick(
    engine: &Arc<AetherEngine>,
    hot_tokens: &Arc<HotTokenTracker>,
    provider: &DynProvider<Ethereum>,
    metrics: &Arc<EngineMetrics>,
    cfg: &DiscoveryConfig,
    at_cap_logged: &mut bool,
) {
    let now = now_secs();
    let candidates = hot_tokens.candidates(now, cfg.max_candidates);
    metrics.set_hot_candidates(candidates.len() as i64);
    if candidates.is_empty() {
        return;
    }

    let current = count_pools(&cfg.pools_path);
    if current >= cfg.max_admitted_pools {
        if !*at_cap_logged {
            warn!(
                current,
                cap = cfg.max_admitted_pools,
                "discovery at admitted-pool cap; not admitting more"
            );
            *at_cap_logged = true;
        }
        metrics.inc_pools_rejected("at_cap");
        return;
    }
    *at_cap_logged = false;

    for hp in candidates {
        let (token, base) = match weth_base(hp.token_a, hp.token_b) {
            Some(tb) => tb,
            None => {
                metrics.inc_pools_rejected("no_weth_base");
                continue;
            }
        };

        let venues = enrich_venues(provider, token, base, cfg).await;
        if venues.len() < cfg.admission.min_venues {
            metrics.inc_pools_rejected("insufficient_venues");
            continue;
        }

        // The round-trip screen needs a UniV2-style pair.
        let v2 = venues
            .iter()
            .find(|v| matches!(v.protocol, ProtocolType::UniswapV2 | ProtocolType::SushiSwap));
        let verdict = match v2 {
            Some(v) => run_screen(provider, v.address, token, base, v.fee_bps, cfg).await,
            None => RoundTripVerdict::Inconclusive {
                reason: "no v2 venue for round-trip screen".into(),
            },
        };
        metrics.inc_fot_screen(verdict.metric_label());
        let fot = verdict.as_fot_verdict();

        match pool_admission::evaluate(&venues, &fot, &cfg.admission) {
            AdmissionOutcome::Admit(entries) => {
                match pool_admission::append_admitted_pools(Path::new(&cfg.pools_path), &entries) {
                    Ok(n) if n > 0 => {
                        for _ in 0..n {
                            metrics.inc_pools_admitted("admitted");
                        }
                        info!(
                            token = %token,
                            venues = entries.len(),
                            appended = n,
                            "discovery admitted pools; reloading registry"
                        );
                        engine.bootstrap_pools(&cfg.pools_path).await;
                    }
                    Ok(_) => metrics.inc_pools_rejected("already_present"),
                    Err(e) => {
                        warn!(error = %e, "discovery: append_admitted_pools failed");
                        metrics.inc_pools_rejected("append_error");
                    }
                }
            }
            AdmissionOutcome::Reject { reason } => {
                // Low-cardinality bucket derived from the gate's decision.
                let label = if reason.contains("fee-on-transfer") {
                    "fot_not_clean"
                } else {
                    "insufficient_liquidity"
                };
                debug!(token = %token, %reason, "discovery rejected candidate");
                metrics.inc_pools_rejected(label);
            }
        }
    }
}

/// Returns `(token, base)` with `base == WETH`, or `None` if neither side is WETH.
fn weth_base(a: Address, b: Address) -> Option<(Address, Address)> {
    if a == WETH {
        Some((b, a))
    } else if b == WETH {
        Some((a, b))
    } else {
        None
    }
}

/// Count `[[pools]]` blocks currently in the registry file.
fn count_pools(path: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|c| c.matches("[[pools]]").count())
        .unwrap_or(0)
}

/// Resolve V2/Sushi/V3 venues for `token`/`base` and attach a liquidity proxy.
async fn enrich_venues(
    provider: &DynProvider<Ethereum>,
    token: Address,
    base: Address,
    cfg: &DiscoveryConfig,
) -> Vec<CandidateVenue> {
    // UniV2/V3 canonical token0/token1 ordering is by address.
    let (token0, token1) = if token < base { (token, base) } else { (base, token) };
    let mut venues = Vec::new();

    for (factory, protocol) in [
        (UNIV2_FACTORY, ProtocolType::UniswapV2),
        (SUSHI_FACTORY, ProtocolType::SushiSwap),
    ] {
        let cd = getPairCall {
            tokenA: token,
            tokenB: base,
        }
        .abi_encode();
        if let Some(pair) = call_addr(provider, factory, cd).await {
            if pair != Address::ZERO {
                if let Some(liquidity_usd) = pool_weth_liquidity_usd(provider, pair, cfg).await {
                    venues.push(CandidateVenue {
                        protocol,
                        address: pair,
                        token0,
                        token1,
                        fee_bps: V2_FEE_BPS,
                        tick_spacing: None,
                        liquidity_usd,
                    });
                }
            }
        }
    }

    for (fee, tick_spacing) in V3_FEE_TIERS {
        let cd = getPoolCall {
            tokenA: token,
            tokenB: base,
            fee: U24::try_from(fee).expect("v3 fee tier fits u24"),
        }
        .abi_encode();
        if let Some(pool) = call_addr(provider, UNIV3_FACTORY, cd).await {
            if pool != Address::ZERO {
                if let Some(liquidity_usd) = pool_weth_liquidity_usd(provider, pool, cfg).await {
                    venues.push(CandidateVenue {
                        protocol: ProtocolType::UniswapV3,
                        address: pool,
                        token0,
                        token1,
                        fee_bps: fee / 100,
                        tick_spacing: Some(tick_spacing),
                        liquidity_usd,
                    });
                }
            }
        }
    }

    venues
}

/// Liquidity proxy: `2 × (WETH held by `pool`) × eth_price_usd`.
async fn pool_weth_liquidity_usd(
    provider: &DynProvider<Ethereum>,
    pool: Address,
    cfg: &DiscoveryConfig,
) -> Option<f64> {
    let cd = balanceOfCall { account: pool }.abi_encode();
    let weth_wei = call_u256(provider, WETH, cd).await?;
    Some(2.0 * u256_to_eth_f64(weth_wei) * cfg.eth_price_usd)
}

async fn call_addr(
    provider: &DynProvider<Ethereum>,
    to: Address,
    calldata: Vec<u8>,
) -> Option<Address> {
    let tx = alloy::rpc::types::TransactionRequest::default()
        .to(to)
        .input(calldata.into());
    let out = provider.call(tx).await.ok()?;
    if out.len() < 32 {
        return None;
    }
    Some(Address::from_slice(&out[12..32]))
}

async fn call_u256(
    provider: &DynProvider<Ethereum>,
    to: Address,
    calldata: Vec<u8>,
) -> Option<U256> {
    let tx = alloy::rpc::types::TransactionRequest::default()
        .to(to)
        .input(calldata.into());
    let out = provider.call(tx).await.ok()?;
    if out.len() < 32 {
        return None;
    }
    Some(U256::from_be_slice(&out[..32]))
}

/// Convert a wei `U256` to whole ETH as `f64`. Lossy by design — only used for
/// a coarse liquidity estimate, never for accounting.
fn u256_to_eth_f64(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(0.0) / 1e18
}

/// Run the fee-on-transfer round-trip screen against a fresh `latest` fork,
/// bounded by `cfg.screen_timeout`. The screen is synchronous + CPU/IO-bound,
/// so it runs on a blocking thread; a timeout maps to [`RoundTripVerdict::ScreenTimeout`].
async fn run_screen(
    provider: &DynProvider<Ethereum>,
    pair: Address,
    token: Address,
    base: Address,
    fee_bps: u32,
    cfg: &DiscoveryConfig,
) -> RoundTripVerdict {
    let block_number = provider.get_block_number().await.unwrap_or(0);
    let state = match RpcForkedState::new_at_latest(
        provider.clone(),
        block_number,
        now_secs(),
        1_000_000_000,
    ) {
        Some(s) => s,
        None => {
            return RoundTripVerdict::Inconclusive {
                reason: "fork state unavailable (need multi-thread runtime)".into(),
            }
        }
    };

    let fot = cfg.fot.clone();
    let max_loss = cfg.max_loss_bps;
    let base_slot = U256::from(WETH_BALANCE_SLOT);
    let budget = cfg.screen_timeout;
    let join = tokio::task::spawn_blocking(move || {
        fee_on_transfer::screen_token_v2_round_trip(
            state, pair, token, base, base_slot, fee_bps, &fot, max_loss,
        )
    });
    match tokio::time::timeout(budget, join).await {
        Ok(Ok(v)) => v,
        Ok(Err(_join_err)) => RoundTripVerdict::Inconclusive {
            reason: "screen task panicked".into(),
        },
        Err(_elapsed) => RoundTripVerdict::ScreenTimeout {
            budget_ms: budget.as_millis() as u64,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn weth_base_orders_correctly() {
        let other = address!("00000000000000000000000000000000000000Aa");
        assert_eq!(weth_base(WETH, other), Some((other, WETH)));
        assert_eq!(weth_base(other, WETH), Some((other, WETH)));
        let nonweth = address!("00000000000000000000000000000000000000Bb");
        assert_eq!(weth_base(other, nonweth), None);
    }

    #[test]
    fn count_pools_counts_blocks() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[[pools]]\naddress = \"0x1\"\n\n[[pools]]\naddress = \"0x2\"\n"
        )
        .unwrap();
        assert_eq!(count_pools(f.path().to_str().unwrap()), 2);
        assert_eq!(count_pools("/nonexistent/path/pools.toml"), 0);
    }

    #[test]
    fn config_defaults_are_sane() {
        let cfg = DiscoveryConfig::from_env("config/pools.toml".to_string());
        assert!(cfg.interval >= Duration::from_secs(1));
        assert!(cfg.max_candidates >= 1);
        assert!(cfg.screen_timeout >= Duration::from_millis(100));
        assert!(cfg.eth_price_usd > 0.0);
    }

    #[tokio::test]
    async fn screen_timeout_maps_to_screen_timeout_verdict() {
        // Mirrors run_screen's timeout arm: a blocking job slower than the
        // budget yields Elapsed, which we map to ScreenTimeout.
        let budget = Duration::from_millis(10);
        let join =
            tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(200)));
        let verdict = match tokio::time::timeout(budget, join).await {
            Ok(_) => RoundTripVerdict::Inconclusive {
                reason: "unexpected".into(),
            },
            Err(_) => RoundTripVerdict::ScreenTimeout {
                budget_ms: budget.as_millis() as u64,
            },
        };
        assert_eq!(verdict.metric_label(), "screen_timeout");
        assert!(!verdict.as_fot_verdict().is_admissible());
    }
}
