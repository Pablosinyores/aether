//! Mempool tracking — pending-transaction subscription layer.
//!
//! This module subscribes to a node's pending-tx stream and fans out
//! [`PendingTxEvent`]s through the existing [`EventChannels`] broadcast.
//! Today the only supported source is Alchemy's `alchemy_pendingTransactions`
//! WebSocket method, which is the lowest-friction free option for plumbing
//! validation. A `MempoolSource` trait isolates that choice so future paid
//! feeds (Chainbound Fiber gRPC, bloXroute, self-hosted Reth `txpool` IPC)
//! can be added without touching downstream consumers.
//!
//! The subscription is **opt-in** via the `MEMPOOL_TRACKING` env var. With it
//! unset the module compiles in but never runs, so binaries on `main` keep
//! their current startup shape.
//!
//! # Privacy and scope
//!
//! - We filter by `toAddress` so only txs aimed at the configured DEX
//!   router set reach the broadcast channel; mempool decoding lives in a
//!   downstream module (`aether-pools::router_decoder`) and is not invoked
//!   here. This module is purely transport.
//! - No bundle is constructed, no submission is performed. The Go executor
//!   never sees these events. The rule "log-only until further notice"
//!   exists to keep the testing scaffold isolated from execution risk.

use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::Transaction as TransactionTrait;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Transaction;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::subscription::{EventChannels, PendingTxEvent};

/// Default reconnect backoff after a transport error.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Returns `true` when `MEMPOOL_TRACKING` is set to a truthy value.
///
/// Accepted truthy values: `1`, `true`, `yes`, `on` (case-insensitive). Any
/// other value (including unset) disables the subscription, so default
/// behaviour on `main` is unchanged.
pub fn is_enabled() -> bool {
    matches!(
        std::env::var("MEMPOOL_TRACKING")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Configuration for the Alchemy pending-tx subscription.
#[derive(Debug, Clone)]
pub struct AlchemyMempoolConfig {
    /// Full WebSocket URL including the `wss://` scheme and Alchemy API key.
    /// Reuse the same `ETH_RPC_URL` value when it already points at Alchemy
    /// over WebSocket; otherwise pass an explicit `ETH_WS_URL`.
    pub ws_url: String,
    /// Filter set: only txs whose `to` field is in this list are emitted.
    /// Empty means "no filter" — emit every pending tx Alchemy sees, which
    /// is firehose-grade and not recommended for production wiring.
    pub router_filter: Vec<Address>,
}

impl AlchemyMempoolConfig {
    /// Build the JSON params for the `alchemy_pendingTransactions` subscribe
    /// call, applying the configured `toAddress` filter when non-empty.
    fn subscribe_params(&self) -> serde_json::Value {
        if self.router_filter.is_empty() {
            json!(["alchemy_pendingTransactions"])
        } else {
            let to_addresses: Vec<String> = self
                .router_filter
                .iter()
                .map(|a| format!("{:#x}", a))
                .collect();
            json!([
                "alchemy_pendingTransactions",
                { "toAddress": to_addresses }
            ])
        }
    }
}

/// Trait for any source that produces a stream of [`PendingTxEvent`]s.
///
/// Implementations own their own reconnection / backoff logic and dispatch
/// directly to [`EventChannels::dispatch_pending_tx`]. Returning from `run`
/// indicates the source has shut down; callers may restart it.
#[async_trait::async_trait]
pub trait MempoolSource: Send + Sync {
    /// Run the subscription loop until shutdown is signalled.
    async fn run(&self, channels: Arc<EventChannels>, shutdown: watch::Receiver<bool>);

    /// Human-readable identifier for logs / metrics.
    fn name(&self) -> &'static str;
}

/// Alchemy `alchemy_pendingTransactions` WebSocket subscription.
pub struct AlchemyMempool {
    config: AlchemyMempoolConfig,
}

impl AlchemyMempool {
    pub fn new(config: AlchemyMempoolConfig) -> Self {
        Self { config }
    }

    /// One subscription attempt: connect, subscribe, drain, return on error.
    /// Errors are returned to the outer reconnect loop in [`run`].
    async fn subscribe_once(
        &self,
        channels: &EventChannels,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let ws = WsConnect::new(self.config.ws_url.clone());
        let provider = ProviderBuilder::new().connect_ws(ws).await?;

        let params = self.config.subscribe_params();
        info!(
            target: "aether::mempool",
            params = %params,
            "subscribing to alchemy_pendingTransactions"
        );

        // alchemy_pendingTransactions is a non-standard subscription; route
        // through the raw `eth_subscribe` path with the method-specific
        // params object.
        let sub = provider
            .subscribe::<_, Transaction>(params)
            .await?;
        let mut stream = sub.into_stream();

        loop {
            tokio::select! {
                next = stream.next() => {
                    match next {
                        Some(tx) => self.forward(channels, tx),
                        None => {
                            warn!(
                                target: "aether::mempool",
                                "alchemy pending stream closed by remote; will reconnect"
                            );
                            return Err("stream closed".into());
                        }
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!(
                            target: "aether::mempool",
                            "shutdown signalled; exiting alchemy mempool subscription"
                        );
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Map an alloy [`Transaction`] into the workspace [`PendingTxEvent`] and
    /// dispatch it. Lossy by design — any field we don't surface today (gas
    /// limit, type, access list) is recoverable via the tx hash later.
    fn forward(&self, channels: &EventChannels, tx: Transaction) {
        let from = tx.inner.signer();
        let envelope = tx.as_ref();
        let to: Option<Address> = envelope.kind().to().copied();
        let event = PendingTxEvent {
            tx_hash: *envelope.tx_hash(),
            from,
            to,
            value: envelope.value(),
            input: envelope.input().to_vec(),
            gas_price: envelope.max_fee_per_gas(),
        };
        debug!(
            target: "aether::mempool",
            tx_hash = %event.tx_hash,
            to = ?event.to,
            input_len = event.input.len(),
            "pending tx forwarded"
        );
        channels.dispatch_pending_tx(event);
    }
}

#[async_trait::async_trait]
impl MempoolSource for AlchemyMempool {
    fn name(&self) -> &'static str {
        "alchemy"
    }

    async fn run(&self, channels: Arc<EventChannels>, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = RECONNECT_BACKOFF;
        loop {
            if *shutdown.borrow() {
                info!(target: "aether::mempool", "alchemy source shutting down");
                return;
            }

            match self.subscribe_once(&channels, &mut shutdown).await {
                Ok(()) => return, // clean shutdown
                Err(e) => {
                    error!(
                        target: "aether::mempool",
                        error = %e,
                        backoff_secs = backoff.as_secs(),
                        "alchemy mempool subscribe failed; reconnecting"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return;
                            }
                        }
                    }
                    // Linear bounded backoff; we do not want to give up but
                    // also do not want to hammer the endpoint.
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }
}

/// Default DEX router addresses on Ethereum mainnet that Aether watches.
///
/// Curated for the testing scaffold: UniswapV2 Router02, UniswapV3
/// SwapRouter, UniswapV3 SwapRouter02, SushiSwap Router02, Curve Router,
/// Balancer Vault. 1inch v6 AggregationRouter is intentionally absent — its
/// multi-step calldata does not decode against a simple `sol!` ABI and would
/// inflate the decode-failure counter without yielding usable hits in the
/// scaffold; revisit once the decoder has the multi-encode path.
pub fn default_router_addresses() -> Vec<Address> {
    use alloy::primitives::address;
    vec![
        address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D"), // UniswapV2 Router02
        address!("E592427A0AEce92De3Edee1F18E0157C05861564"), // UniswapV3 SwapRouter
        address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"), // UniswapV3 SwapRouter02
        address!("d9e1cE17f2641f24aE83637ab66a2cca9C378B9F"), // SushiSwap Router02
        address!("99a58482BD75cbab83b27EC03CA68fF489b5788f"), // Curve Router
        address!("BA12222222228d8Ba445958a75a0704d566BF2C8"), // Balancer V2 Vault
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_enabled_respects_truthy_strings() {
        // Each thread sees its own env, so we set + unset within the test.
        std::env::set_var("MEMPOOL_TRACKING", "1");
        assert!(is_enabled());
        std::env::set_var("MEMPOOL_TRACKING", "TRUE");
        assert!(is_enabled());
        std::env::set_var("MEMPOOL_TRACKING", "yes");
        assert!(is_enabled());
        std::env::set_var("MEMPOOL_TRACKING", "off");
        assert!(!is_enabled());
        std::env::remove_var("MEMPOOL_TRACKING");
        assert!(!is_enabled());
    }

    #[test]
    fn subscribe_params_omit_filter_when_empty() {
        let cfg = AlchemyMempoolConfig {
            ws_url: "wss://example".into(),
            router_filter: vec![],
        };
        let v = cfg.subscribe_params();
        assert_eq!(v, json!(["alchemy_pendingTransactions"]));
    }

    #[test]
    fn subscribe_params_apply_lowercase_addresses() {
        use alloy::primitives::address;
        let cfg = AlchemyMempoolConfig {
            ws_url: "wss://example".into(),
            router_filter: vec![address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D")],
        };
        let v = cfg.subscribe_params();
        let expected = json!([
            "alchemy_pendingTransactions",
            {
                "toAddress": ["0x7a250d5630b4cf539739df2c5dacb4c659f2488d"]
            }
        ]);
        assert_eq!(v, expected);
    }

    #[test]
    fn default_router_set_is_non_empty_and_uniqued() {
        let v = default_router_addresses();
        assert!(!v.is_empty());
        let mut sorted = v.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), v.len(), "duplicate addresses in default set");
    }
}
