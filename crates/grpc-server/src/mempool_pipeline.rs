//! Pipeline that consumes pending-tx events from the mempool subscription
//! and runs them through the router calldata decoder.
//!
//! Today the pipeline is **log-only**: every successfully decoded swap is
//! emitted to `tracing` and counted in `aether_pending_dex_tx_total`, every
//! decode failure goes to `aether_pending_decode_errors_total`. No bundle is
//! constructed, no submission is made, no engine state is mutated. The next
//! follow-up wires post-state revm simulation + Bellman-Ford detection on
//! top of this pipeline; until then this is a coverage / decoder-quality
//! validator.
//!
//! The pipeline runs only when [`aether_ingestion::mempool::is_enabled`]
//! returns `true` (i.e. `MEMPOOL_TRACKING=1` in the environment), so default
//! `main`-branch behaviour is unchanged.

use std::sync::Arc;

use aether_ingestion::subscription::{EventChannels, PendingTxEvent};
use aether_pools::router_decoder::{decode_pending, DecodeError, DecodedSwap, Protocol};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use aether_grpc_server::EngineMetrics;

/// Spawn the mempool decode pipeline as a tokio task.
///
/// Returns the [`tokio::task::JoinHandle`] so callers can await graceful
/// shutdown. The task exits when the broadcast channel closes (engine
/// shutdown) or when `shutdown` flips to `true`.
pub fn spawn_mempool_pipeline(
    channels: Arc<EventChannels>,
    metrics: Arc<EngineMetrics>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = channels.subscribe_pending_txs();
        info!(target: "aether::mempool", "mempool decode pipeline started");
        loop {
            tokio::select! {
                next = rx.recv() => match next {
                    Ok(event) => handle_event(&metrics, event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            target: "aether::mempool",
                            lagged = n,
                            "decode pipeline lagged behind broadcast; events dropped"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(target: "aether::mempool", "broadcast closed; pipeline exiting");
                        return;
                    }
                },
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!(target: "aether::mempool", "shutdown signalled; pipeline exiting");
                        return;
                    }
                }
            }
        }
    })
}

/// Decode one pending tx and update metrics + logs.
///
/// Pulled out as a free function so unit tests can drive it without spawning
/// the full pipeline task.
fn handle_event(metrics: &EngineMetrics, event: PendingTxEvent) {
    let Some(to) = event.to else {
        // Contract creations and other anonymous calls don't have a router
        // to attribute to — bump a generic `no_to` failure and move on.
        metrics.inc_pending_decode_errors("no_to");
        return;
    };
    let router_label = format!("{:#x}", to);

    match decode_pending(to, &event.input) {
        Ok(swap) => emit_decoded(metrics, &router_label, &swap, &event),
        Err(err) => emit_failure(metrics, &router_label, &err),
    }
}

fn emit_decoded(
    metrics: &EngineMetrics,
    router_label: &str,
    swap: &DecodedSwap,
    event: &PendingTxEvent,
) {
    metrics.inc_pending_dex_tx(router_label, protocol_label(swap.protocol), true);
    debug!(
        target: "aether::mempool",
        tx_hash = %event.tx_hash,
        router = %router_label,
        protocol = ?swap.protocol,
        token_in = %swap.token_in,
        token_out = %swap.token_out,
        amount_in = %swap.amount_in,
        fee_bps = swap.fee_bps,
        "PENDING DEX SWAP decoded"
    );
}

fn emit_failure(metrics: &EngineMetrics, router_label: &str, err: &DecodeError) {
    let reason = decode_error_label(err);
    metrics.inc_pending_dex_tx(router_label, "unknown", false);
    metrics.inc_pending_decode_errors(reason);
    debug!(
        target: "aether::mempool",
        router = %router_label,
        reason,
        error = %err,
        "pending tx decode failed"
    );
}

fn protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::UniswapV2 => "uniswap_v2",
        Protocol::UniswapV3 => "uniswap_v3",
        Protocol::SushiSwap => "sushiswap",
        Protocol::BalancerV2 => "balancer_v2",
    }
}

fn decode_error_label(err: &DecodeError) -> &'static str {
    match err {
        DecodeError::TooShort => "too_short",
        DecodeError::UnknownSelector { .. } => "unknown_selector",
        DecodeError::AbiDecode(_) => "abi_decode",
        DecodeError::EmptyPath => "empty_path",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_pools::router_decoder::IUniswapV2Router02::swapExactTokensForTokensCall;
    use alloy::primitives::{address, B256, U256};
    use alloy::sol_types::SolCall;

    fn pending_event(to: Option<alloy::primitives::Address>, input: Vec<u8>) -> PendingTxEvent {
        PendingTxEvent {
            tx_hash: B256::ZERO,
            from: alloy::primitives::Address::ZERO,
            to,
            value: U256::ZERO,
            input,
            gas_price: 0,
        }
    }

    #[test]
    fn protocol_label_is_stable() {
        assert_eq!(protocol_label(Protocol::UniswapV2), "uniswap_v2");
        assert_eq!(protocol_label(Protocol::UniswapV3), "uniswap_v3");
        assert_eq!(protocol_label(Protocol::SushiSwap), "sushiswap");
        assert_eq!(protocol_label(Protocol::BalancerV2), "balancer_v2");
    }

    #[test]
    fn decode_error_label_covers_every_variant() {
        assert_eq!(decode_error_label(&DecodeError::TooShort), "too_short");
        assert_eq!(
            decode_error_label(&DecodeError::UnknownSelector { selector: [0; 4] }),
            "unknown_selector"
        );
        assert_eq!(
            decode_error_label(&DecodeError::AbiDecode("x".into())),
            "abi_decode"
        );
        assert_eq!(decode_error_label(&DecodeError::EmptyPath), "empty_path");
    }

    #[test]
    fn handle_event_decoded_swap_does_not_panic() {
        let metrics = EngineMetrics::new();
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let calldata = swapExactTokensForTokensCall {
            amountIn: U256::from(1_000u64),
            amountOutMin: U256::from(900u64),
            path: vec![weth, usdc],
            to: alloy::primitives::Address::ZERO,
            deadline: U256::ZERO,
        }
        .abi_encode();
        let to = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        handle_event(&metrics, pending_event(Some(to), calldata));
    }

    #[test]
    fn handle_event_unknown_selector_does_not_panic() {
        let metrics = EngineMetrics::new();
        let mut calldata = vec![0xde, 0xad, 0xbe, 0xef];
        calldata.extend(std::iter::repeat_n(0u8, 64));
        let to = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        handle_event(&metrics, pending_event(Some(to), calldata));
    }

    #[test]
    fn handle_event_no_to_does_not_panic() {
        let metrics = EngineMetrics::new();
        handle_event(&metrics, pending_event(None, vec![0x12, 0x34, 0x56, 0x78]));
    }
}
