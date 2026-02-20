# Aether Architecture

## System Overview

Aether is a production-grade MEV arbitrage engine for Ethereum Mainnet. It detects and executes cross-DEX arbitrage opportunities across Uniswap V2/V3, SushiSwap, Curve, Balancer, and Bancor with sub-15ms end-to-end latency.

## Component Architecture

```
Eth Nodes (WS/IPC)
    │
    ▼
┌─────────────────── RUST CORE (Latency-Critical) ──────────────────┐
│                                                                     │
│  ┌──────────┐   ┌──────────┐   ┌───────────┐   ┌───────────────┐  │
│  │Ingestion │──▶│  Pools   │──▶│   State   │──▶│   Detector    │  │
│  │          │   │ Registry │   │Management │   │(Bellman-Ford) │  │
│  └──────────┘   └──────────┘   └───────────┘   └───────┬───────┘  │
│                                                         │          │
│                                                         ▼          │
│                                                 ┌───────────────┐  │
│                                                 │   Simulator   │  │
│                                                 │    (revm)     │  │
│                                                 └───────┬───────┘  │
│                                                         │          │
│  ┌──────────────────────────────────────────────────────┘          │
│  │ gRPC Server (tonic)                                             │
└──┼─────────────────────────────────────────────────────────────────┘
   │ gRPC over UDS (<1μs)
   ▼
┌──────────────────────────────────────────────────────────────────-─┐
│               GO EXECUTION LAYER (Coordination)                    │
│                                                                     │
│  ┌──────────┐   ┌───────────┐   ┌──────────┐   ┌──────────────┐  │
│  │ Executor │   │   Risk    │   │ Monitor  │   │  Gas Oracle  │  │
│  │ (Bundle) │   │ Manager   │   │ (Prom.)  │   │  (EIP-1559)  │  │
│  └────┬─────┘   └───────────┘   └──────────┘   └──────────────┘  │
└───────┼────────────────────────────────────────────────────────────┘
        │ eth_sendBundle
        ▼
   Block Builders (Flashbots, Titan, Beaver, rsync)
        │
        ▼
┌──────────────────── ON-CHAIN (Solidity) ──────────────────────────┐
│  AetherExecutor.sol → Aave V3 Flash Loans → DEX Swaps            │
└───────────────────────────────────────────────────────────────────┘
```

## Rust Crates

### `crates/common/`
Shared types and error definitions used across all crates.
- `ProtocolType` enum (UniswapV2, V3, SushiSwap, Curve, Balancer, Bancor)
- `ArbOpportunity`, `ArbHop`, `SwapStep`, `ValidatedArb`, `SimulationResult`
- `AetherError` with `thiserror` for structured error handling

### `crates/ingestion/`
WebSocket event ingestion from Ethereum nodes.
- **NodePool**: Connection state machine (`Connected → Healthy → Degraded → Reconnecting → Failed`)
- **EventDecoder**: ABI decoding via `alloy::sol!` for `Sync`, `Swap`, `SwapV3`, `TokenExchange`, `PairCreated`
- **Subscription**: Broadcast channel dispatch using `tokio::sync::broadcast`

### `crates/pools/`
DEX pool implementations with the `Pool` trait.
- **UniswapV2**: Constant product AMM — `dy = (dx * 997 * y) / (x * 1000 + dx * 997)`
- **UniswapV3**: Concentrated liquidity with Q96 fixed-point math and tick traversal
- **SushiSwap**: Delegation wrapper over UniV2 (identical math)
- **Curve**: StableSwap invariant solved via Newton's method iteration
- **Balancer**: Weighted constant product with configurable token weights
- **Bancor**: Bonding curve with BNT intermediary token
- **Registry**: Pool discovery, qualification, and tiered management (Hot/Warm/Cold)

### `crates/state/`
Price graph and MVCC state management.
- **PriceGraph**: Directed graph with `-ln(rate)` edge weights and dirty bit tracking
- **SnapshotManager**: Lock-free MVCC via `Arc<ArcSwap<GraphSnapshot>>`
- **TokenIndex**: Bidirectional `Address ↔ usize` mapping

### `crates/detector/`
Arbitrage detection engine.
- **BellmanFord**: SPFA variant with Shortest-Label-First optimization
- **Optimizer**: Ternary search for optimal input amount (~100 iterations)
- **Gas**: Per-protocol gas estimation model
- **Opportunity**: `DetectedCycle`, `RankedOpportunity`, `TopKCollector`

### `crates/simulator/`
EVM simulation using `revm` in fork mode.
- **EvmSimulator**: Fork latest block → execute calldata → validate profit
- **ForkedState**: `CacheDB` with account/storage state management
- **Calldata**: ABI encodes `AetherExecutor.executeArb()` calldata

### `crates/grpc-server/`
tonic gRPC server binary entry point.
- **ArbService**: `SubmitArb`, `StreamArbs` RPCs
- **HealthService**: Engine health checks
- **ControlService**: `SetState`, `ReloadConfig` for operational control

## Go Services

### `cmd/executor/`
Bundle construction and multi-builder submission.
- EIP-1559 transaction construction (arb_tx + tip_tx)
- Goroutine fan-out to Flashbots, Titan, Beaver, rsync builders
- Atomic nonce management with periodic sync

### `cmd/risk/`
Risk management and circuit breakers.
- System state FSM: `Running → Degraded → Paused → Halted`
- Circuit breakers: gas price, consecutive reverts, daily loss, balance
- Position limits: max trade size, daily volume, min profit

### `cmd/monitor/`
Monitoring and observability.
- Prometheus metrics exposition (`/metrics`)
- HTML operational dashboard
- Alert dispatch to PagerDuty, Telegram, Discord

## On-Chain Contract

### `contracts/src/AetherExecutor.sol`
- Implements `IFlashLoanSimpleReceiver` (Aave V3)
- `executeArb()` → `POOL.flashLoanSimple()` → `executeOperation()` callback
- Routes swaps by protocol enum through `_executeSwap()`
- `rescue()` for emergency token withdrawal (onlyOwner)

## Data Flow

1. **Event Ingestion** (<1ms) — WS events → ABI decode → broadcast channels
2. **State Update** — Pool reserve updates → recompute price graph edges
3. **Detection** (<3ms) — Bellman-Ford negative cycle scan on dirty subgraph
4. **Simulation** (<5ms) — Fork block state in revm, execute and validate profit
5. **gRPC Handoff** (<1ms) — `ValidatedArb` sent to Go executor
6. **Bundle Build** (<2ms) — EIP-1559 tx + tip tx, sign with searcher key
7. **Submission** — Fan-out to all configured block builders

## Key Design Decisions

- **Rust for hot path, Go for coordination**: Rust's zero-cost abstractions and control over memory layout are critical for sub-millisecond detection. Go's goroutines and ecosystem are better suited for network coordination and monitoring.
- **`-ln(rate)` edge weights**: Transforms multiplicative exchange rates into additive weights, allowing Bellman-Ford to detect profitable cycles (negative weight = profitable path).
- **MVCC snapshots**: Writers atomically swap new graph versions. Readers get zero-copy immutable references. No locks on the hot path.
- **Flash loan execution**: Zero capital at risk. Unprofitable trades revert atomically on-chain.
- **Multi-builder submission**: Maximizes inclusion probability by submitting to all builders simultaneously.
