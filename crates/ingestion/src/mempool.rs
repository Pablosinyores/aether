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
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{keccak256, Address};
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Transaction;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::metrics::MempoolIngestMetrics;
use crate::subscription::{EventChannels, PendingTxEvent};

/// Default reconnect backoff after a transport error.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Returns `true` when `MEMPOOL_TRACKING` is set to a truthy value.
///
/// Accepted truthy values: `1`, `true`, `yes`, `on` (case-insensitive). Any
/// other value (including unset) disables the subscription, so default
/// behaviour on `main` is unchanged.
pub fn is_enabled() -> bool {
    is_enabled_from_str(&std::env::var("MEMPOOL_TRACKING").unwrap_or_default())
}

/// Pure parser used by [`is_enabled`]; split out so unit tests can exercise the
/// truthy-string rules without mutating process-wide env (which is `unsafe` on
/// edition 2024 and race-prone under parallel `cargo test`).
fn is_enabled_from_str(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
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
    /// Ingestion metrics. `None` keeps the source usable in tests / dev
    /// without standing up a Prometheus registry; the re-encode mismatch
    /// gate then surfaces only through the `warn!` log.
    metrics: Option<Arc<MempoolIngestMetrics>>,
}

impl AlchemyMempool {
    /// Construct without metrics. The re-encode mismatch gate still drops
    /// bad events and logs a `warn!`, but no counter is incremented.
    pub fn new(config: AlchemyMempoolConfig) -> Self {
        warn_if_non_alchemy_endpoint(&config.ws_url);
        Self {
            config,
            metrics: None,
        }
    }

    /// Construct with an ingestion metrics handle so the re-encode mismatch
    /// gate increments `aether_mempool_raw_reencode_mismatch_total`.
    pub fn with_metrics(config: AlchemyMempoolConfig, metrics: Arc<MempoolIngestMetrics>) -> Self {
        warn_if_non_alchemy_endpoint(&config.ws_url);
        Self {
            config,
            metrics: Some(metrics),
        }
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
    /// dispatch it. Lossy for the scalar fields by design — anything we don't
    /// surface today (gas limit, access list) is recoverable via the tx hash
    /// later — but the canonical EIP-2718 signed bytes are captured verbatim
    /// in `raw_tx` for the backrun bundle path.
    ///
    /// The raw bytes are re-encoded from the recovered envelope and gated by a
    /// keccak256 round-trip: if `keccak256(raw_tx)` does not equal the
    /// subscription-reported tx hash, the bytes are untrustworthy (we would
    /// place the wrong tx as `txs[0]` in the bundle) so the event is dropped
    /// and `aether_mempool_raw_reencode_mismatch_total` is incremented.
    fn forward(&self, channels: &EventChannels, tx: Transaction) {
        let from = tx.inner.signer();
        let envelope = tx.as_ref();
        let tx_hash = *envelope.tx_hash();
        let to: Option<Address> = envelope.kind().to().copied();
        // Stamp first-seen at the moment we hand the event off so the
        // downstream tracker can compute inclusion latency against
        // wall-clock ingest time. SystemTime is intentional (not
        // Instant): the tracker compares against block timestamps from
        // chain, which are UNIX-time, not process-monotonic.
        let first_seen_unix_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let value = envelope.value();
        let input = envelope.input().to_vec();
        let gas_price = envelope.max_fee_per_gas();

        // Canonical EIP-2718 signed bytes, re-encoded from the recovered
        // envelope, then verified against the reported hash.
        let raw_tx = tx.inner.inner().encoded_2718();
        if !raw_tx_matches_hash(&raw_tx, tx_hash) {
            if let Some(metrics) = &self.metrics {
                metrics.inc_raw_reencode_mismatch();
            }
            warn!(
                target: "aether::mempool",
                tx_hash = %tx_hash,
                raw_len = raw_tx.len(),
                "pending tx dropped: EIP-2718 re-encode did not hash back to reported tx hash"
            );
            return;
        }

        let event = PendingTxEvent {
            tx_hash,
            from,
            to,
            value,
            input,
            gas_price,
            first_seen_unix_nanos,
            raw_tx,
        };
        debug!(
            target: "aether::mempool",
            tx_hash = %event.tx_hash,
            to = ?event.to,
            input_len = event.input.len(),
            raw_len = event.raw_tx.len(),
            "pending tx forwarded"
        );
        channels.dispatch_pending_tx(event);
    }
}

/// Verify that canonical EIP-2718 signed bytes hash back to the expected tx
/// hash. The tx hash of any typed/legacy Ethereum transaction is precisely
/// `keccak256` of its canonical 2718 encoding, so this is a bit-exact gate on
/// whether `raw_tx` is the genuine signed payload for `expected_hash`.
///
/// Extracted as a free function so it can be unit-tested against known
/// signed-tx vectors without a live WebSocket source.
fn raw_tx_matches_hash(raw_tx: &[u8], expected_hash: alloy::primitives::B256) -> bool {
    keccak256(raw_tx) == expected_hash
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
                    // Exponential bounded backoff (cap 30 s) — we do not want
                    // to give up but also do not want to hammer the endpoint.
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }
}

/// Warn loudly when the configured WebSocket endpoint is unlikely to be
/// Alchemy. `alchemy_pendingTransactions` is an Alchemy-proprietary
/// subscribe method — Reth, QuickNode, Infura and self-hosted Geth accept
/// the WS upgrade but never deliver events, so this case otherwise produces
/// zero metrics with no obvious failure mode. Heuristic only: matches the
/// hostnames Alchemy issues for mainnet/sepolia.
fn warn_if_non_alchemy_endpoint(ws_url: &str) {
    let lower = ws_url.to_ascii_lowercase();
    let alchemy_markers = ["alchemy.com", "g.alchemy.com", "alchemyapi.io"];
    if alchemy_markers.iter().any(|m| lower.contains(m)) {
        return;
    }
    warn!(
        target: "aether::mempool",
        ws_url = %ws_url,
        "MEMPOOL_TRACKING enabled but WS endpoint does not look like Alchemy; \
         alchemy_pendingTransactions is Alchemy-only and will return no events \
         on Reth/QuickNode/Infura/Geth — see .env.example"
    );
}

/// Default DEX router addresses on Ethereum mainnet that Aether watches.
///
/// Curated for the testing scaffold: UniswapV2 Router02, UniswapV3
/// SwapRouter, UniswapV3 SwapRouter02, SushiSwap Router02, Curve Router,
/// Balancer Vault, Bancor V3 BancorNetwork, plus the highest-volume Curve
/// pool addresses (the pool-direct `exchange()` path skips the router so
/// the router address alone misses Curve traffic — see
/// `aether-pools::router_decoder::try_curve`). 1inch v6 AggregationRouter
/// is intentionally absent — its multi-step calldata does not decode
/// against a simple `sol!` ABI and would inflate the decode-failure
/// counter without yielding usable hits in the scaffold; revisit once
/// the decoder has the multi-encode path.
pub fn default_router_addresses() -> Vec<Address> {
    use alloy::primitives::address;
    vec![
        address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D"), // UniswapV2 Router02
        address!("E592427A0AEce92De3Edee1F18E0157C05861564"), // UniswapV3 SwapRouter
        address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"), // UniswapV3 SwapRouter02
        address!("d9e1cE17f2641f24aE83637ab66a2cca9C378B9F"), // SushiSwap Router02
        address!("99a58482BD75cbab83b27EC03CA68fF489b5788f"), // Curve Router
        address!("BA12222222228d8Ba445958a75a0704d566BF2C8"), // Balancer V2 Vault
        address!("eEF417e1D5CC832e619ae18D2F140De2999dD4fB"), // Bancor V3 BancorNetwork
        // ── Curve pools (pool-direct `exchange()` traffic) ──
        // Curve calls hit pool addresses directly when the user / aggregator
        // skips the Curve Router. Without these in the `toAddress` filter
        // Alchemy never forwards them and the decoder we just landed
        // (PR #156) stays unreachable for the majority of Curve volume.
        // List is the top-by-volume mainnet pools as of 2026-05; keep
        // synced with `config/pools.toml` Curve entries.
        address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"), // Curve 3pool (DAI/USDC/USDT)
        address!("DC24316b9AE028F1497c275EB9192a3Ea0f67022"), // Curve stETH/ETH
        address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"), // Curve tricrypto2 (USDT/WBTC/WETH)
        address!("a1F8A6807c402E4A15ef4EBa36528A3FED24E577"), // Curve frxETH/ETH
        address!("4eBdF703948ddCEA3B11f675B4D1Fba9d2414A14"), // Curve tricryptoUSDC
        address!("f5f5B97624542D72A9E06f04804Bf81baA15e2B4"), // Curve tricryptoUSDT
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_enabled_respects_truthy_strings() {
        for v in ["1", "true", "TRUE", "True", "yes", "YES", "on", "On"] {
            assert!(is_enabled_from_str(v), "{v} should enable");
        }
        for v in ["", "0", "false", "no", "off", "anything", "  1  "] {
            assert!(!is_enabled_from_str(v), "{v} should not enable");
        }
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
    fn alchemy_marker_detection() {
        // Marker present → no warn (cannot directly assert log absence here,
        // but exercise both branches of the heuristic).
        for url in [
            "wss://eth-mainnet.g.alchemy.com/v2/key",
            "wss://eth-mainnet.alchemyapi.io/v2/key",
            "wss://eth.alchemy.com/v2/key",
        ] {
            // Ensure no panic on any path; explicit assertion lives in the
            // `warn_if_non_alchemy_endpoint` heuristic comment.
            warn_if_non_alchemy_endpoint(url);
        }
        warn_if_non_alchemy_endpoint("wss://reth.local:8546");
        warn_if_non_alchemy_endpoint("wss://eth-mainnet.quiknode.pro/key");
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

    /// Build a real signed EIP-1559 transaction and return its canonical
    /// EIP-2718 bytes alongside the envelope's own tx hash. This is the same
    /// signing path a wallet uses, so the bytes and hash form a genuine vector
    /// for exercising the re-encode gate.
    fn signed_tx_vector() -> (Vec<u8>, alloy::primitives::B256) {
        use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
        use alloy::primitives::{address, TxKind, U256 as AlloyU256};
        use alloy::signers::{local::PrivateKeySigner, SignerSync};

        // Deterministic key so the vector is stable across runs.
        let signer: PrivateKeySigner = "0x4c0883a69102937d6231471b5dbb6204fe512961708279f2e3e8a5d4b8e3e3e3"
            .parse()
            .expect("valid private key hex");

        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 7,
            gas_limit: 21_000,
            max_fee_per_gas: 30_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D")),
            value: AlloyU256::from(1_000_000_000_000_000_000u64),
            access_list: Default::default(),
            input: vec![0xde, 0xad, 0xbe, 0xef].into(),
        };

        let sig = signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("sign tx");
        let envelope: TxEnvelope = tx.into_signed(sig).into();
        let raw = envelope.encoded_2718();
        let hash = *envelope.tx_hash();
        (raw, hash)
    }

    #[test]
    fn raw_tx_matches_hash_accepts_genuine_signed_bytes() {
        let (raw, hash) = signed_tx_vector();
        // The reported tx hash is keccak256 of the canonical 2718 encoding.
        assert_eq!(keccak256(&raw), hash);
        assert!(
            raw_tx_matches_hash(&raw, hash),
            "genuine signed bytes must pass the re-encode gate"
        );
    }

    #[test]
    fn raw_tx_matches_hash_rejects_tampered_payload() {
        let (mut raw, hash) = signed_tx_vector();
        // Flip a byte in the signed payload: the hash no longer matches.
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        assert!(
            !raw_tx_matches_hash(&raw, hash),
            "tampered payload must fail the re-encode gate"
        );
    }

    #[test]
    fn mismatch_gate_bumps_counter_and_drops() {
        // Simulate exactly what `forward` does on a mismatch: a tampered
        // payload fails the gate, so the metrics counter is incremented and
        // the event is dropped (never dispatched). We assert the counter
        // accounting here because `forward` requires a live `Transaction`.
        let registry = prometheus::Registry::new();
        let metrics = MempoolIngestMetrics::register(&registry);

        let (mut raw, hash) = signed_tx_vector();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;

        assert_eq!(metrics.raw_reencode_mismatch_count(), 0);
        if !raw_tx_matches_hash(&raw, hash) {
            metrics.inc_raw_reencode_mismatch();
        }
        assert_eq!(
            metrics.raw_reencode_mismatch_count(),
            1,
            "mismatch must bump aether_mempool_raw_reencode_mismatch_total"
        );
    }
}
