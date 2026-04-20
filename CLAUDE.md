# Aether MEV Arbitrage Bot

Production-grade, cross-DEX arbitrage engine for Ethereum Mainnet.
Sub-millisecond opportunity detection across Uniswap V2/V3, SushiSwap, Curve, Balancer, Bancor, and 1inch — with Flashbots-native bundle execution, on-chain simulation via `revm`, and extensible pool registry.

**Architecture Specification v1.0** | Classification: CONFIDENTIAL

---

## Tech Stack

| Layer | Language | Key Libraries / Frameworks |
|---|---|---|
| Data Ingestion & ABI Parsing | **Rust** | `tokio`, `alloy`, WebSocket |
| Pool State Management | **Rust** | `DashMap`, arena allocators |
| Arbitrage Detection | **Rust** | Bellman-Ford (SPFA), SIMD math |
| EVM Simulation | **Rust** | `revm` (fork mode) |
| Bundle Construction & Submission | **Go** | `go-ethereum`, `flashbotsrpc` |
| Risk Management & Circuit Breakers | **Go** | Stateful controllers, `sync/atomic` |
| Monitoring & API | **Go** | Prometheus, gRPC, `net/http` |
| On-Chain Executor | **Solidity** | Aave V3 Flash Loans, OpenZeppelin |
| Inter-Service Communication | Both | gRPC + Protobuf over Unix Domain Sockets |
| Infrastructure | — | PostgreSQL (ledger), Redis (cache), Prometheus, Grafana, Loki |

---

## Architecture Overview

The system has **7 distinct layers** with clear ownership boundaries:

```
Eth Nodes (WS/IPC)
    │
    ▼
┌─────────────────── RUST CORE (Latency-Critical) ──────────────────┐
│  Data Ingestion → DEX Pool Registry → State Management            │
│       → Arbitrage Detection (Bellman-Ford) → EVM Simulator (revm) │
└──────────────────────────┬────────────────────────────────────────-┘
                           │ gRPC over UDS (<1μs)
┌──────────────────────────▼───────────────────────────────────────-─┐
│               GO EXECUTION LAYER (Coordination)                    │
│  Bundle Constructor → Multi-Builder Submitter                      │
│  Risk Manager & Circuit Breakers → Monitoring & Observability      │
└──────────────────────────┬─────────────────────────────────────────┘
                           │ eth_sendBundle
                           ▼
              Flashbots Relay / Block Builders
                           │
                           ▼
┌──────────────────── ON-CHAIN (Solidity) ──────────────────────────┐
│  AetherExecutor.sol → Aave V3 Flash Loans → DEX Swaps            │
└───────────────────────────────────────────────────────────────────┘
```

### Hot Path (target <15ms end-to-end)

1. **Event Ingestion** (<1ms) — WebSocket `newHeads`/`logs`/`pendingTx` → ABI decode via `alloy`
2. **State Update** — Update pool reserves → recompute affected edges in price graph
3. **Detection** (<3ms) — Bellman-Ford negative cycle scan on affected subgraph
4. **Simulation** (<5ms) — Fork latest block state in `revm`, execute calldata, verify profit
5. **gRPC Handoff** (<1ms) — `ValidatedArb` sent to Go executor over UDS
6. **Bundle Build + Sign** (<2ms) — EIP-1559 tx + tip tx, sign with searcher key
7. **Submission** — Fan-out `eth_sendBundle` to Flashbots, Titan, Beaver, rsync builders

---

## Core Design Principles

- **Zero-Copy Hot Path** — Lock-free data structures, arena allocators, zero-copy deserialization. No heap allocation on the hot path.
- **Separation of Concerns** — Rust for all latency-critical work; Go for coordination/execution.
- **Fail-Safe by Default** — Circuit breakers, retry budgets, graceful degradation at every component.
- **Extensible Pool Registry** — New DEX = implement one Rust `Pool` trait. Hot-reloadable TOML config.
- **Atomic Execution** — All arbs are flashloan-backed. Unprofitable trades revert atomically. Zero capital at risk.
- **MEV-Aware Submission** — Bundles via Flashbots Protect, MEV-Share, and direct builder APIs. Private mempool by default.

---

## Repository Structure

```
aether/
├── Cargo.toml                    # Rust workspace root
├── go.mod                        # Go module root
├── proto/
│   └── aether.proto              # Shared Protobuf schema (gRPC contract)
│
├── crates/                       # ── Rust Crates ──
│   ├── ingestion/                # Data ingestion & node pool
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── node_pool.rs      # WS connection mgmt, health state machine
│   │       ├── event_decoder.rs  # ABI decoding via alloy sol! macro
│   │       └── subscription.rs   # Event subscription demux
│   │
│   ├── pools/                    # DEX pool implementations
│   │   └── src/
│   │       ├── lib.rs            # Pool trait definition
│   │       ├── uniswap_v2.rs     # Constant product: dy = (dx*997*y)/(x*1000+dx*997)
│   │       ├── uniswap_v3.rs     # Concentrated liquidity, tick traversal
│   │       ├── sushiswap.rs      # Same AMM math as Uni V2
│   │       ├── curve.rs          # StableSwap invariant (Newton's method)
│   │       ├── balancer.rs       # Weighted constant product
│   │       ├── bancor.rs         # Bonding curve with BNT intermediary
│   │       └── registry.rs       # Pool discovery & qualification pipeline
│   │
│   ├── state/                    # State management & price graph
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── price_graph.rs    # Directed graph, -ln(rate) edge weights, BitVec dirty flags
│   │       ├── snapshot.rs       # MVCC via Arc<ArcSwap<GraphSnapshot>>
│   │       └── token_index.rs    # Token address → graph index mapping
│   │
│   ├── detector/                 # Arbitrage detection engine
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── bellman_ford.rs   # SPFA with SLF optimization, early exit
│   │       ├── optimizer.rs      # Ternary search for optimal input amount
│   │       ├── gas.rs            # Per-protocol gas estimation model
│   │       └── opportunity.rs    # ArbOpportunity / ArbHop structs
│   │
│   ├── simulator/                # EVM simulation (revm)
│   │   └── src/
│   │       ├── lib.rs            # EvmSimulator: fork → execute → validate
│   │       ├── fork.rs           # CacheDB + EthersDB block state forking
│   │       └── calldata.rs       # AetherExecutor.executeArb() calldata builder
│   │
│   ├── grpc-server/              # tonic gRPC server (Rust binary entry point)
│   │   └── src/
│   │       ├── main.rs
│   │       └── service.rs        # ArbService, HealthService, ControlService
│   │
│   └── common/                   # Shared types, utils, errors
│       └── src/
│           ├── lib.rs
│           ├── types.rs
│           └── error.rs
│
├── cmd/                          # ── Go Services ──
│   ├── executor/
│   │   ├── main.go               # Go binary entry point
│   │   ├── bundle.go             # Bundle construction (arb_tx + tip_tx)
│   │   ├── submitter.go          # Goroutine fan-out to multiple builders
│   │   ├── nonce.go              # Atomic nonce counter + periodic sync
│   │   └── gas_oracle.go         # EIP-1559 base fee prediction + priority fee
│   │
│   ├── risk/
│   │   ├── manager.go            # PreflightCheck, circuit breakers, limits
│   │   └── state.go              # SystemState: Running/Degraded/Paused/Halted
│   │
│   └── monitor/
│       ├── metrics.go            # Prometheus /metrics exposition
│       ├── dashboard.go          # HTTP dashboard server
│       └── alerter.go            # Alert dispatch (PagerDuty/Telegram/Discord)
│
├── contracts/                    # ── Solidity ──
│   ├── src/
│   │   └── AetherExecutor.sol    # Flashloan receiver + multi-DEX swap router
│   ├── test/
│   │   └── AetherExecutor.t.sol
│   └── foundry.toml
│
├── config/
│   ├── pools.toml                # Pool registry (hot-reloadable)
│   ├── risk.yaml                 # Risk parameters & circuit breaker thresholds
│   ├── nodes.yaml                # Ethereum node provider endpoints
│   └── builders.yaml             # Block builder API endpoints
│
├── deploy/
│   ├── systemd/                  # aether-rust.service, aether-go.service
│   ├── ansible/                  # Server provisioning playbooks
│   └── docker/                   # Dev/test containers
│
├── scripts/
│   ├── backtest.py               # Historical opportunity analysis
│   ├── gas_profiler.py           # Gas usage profiling
│   └── deploy.sh                 # Deployment automation
│
└── docs/
    ├── architecture.md
    ├── runbook.md
    └── incident-response.md
```

---

## Key Modules Deep Dive

### 1. Data Ingestion Layer (`crates/ingestion/`)

- **Node Pool**: 3+ providers (Alchemy WS, QuickNode WS, local Reth IPC). State machine per connection: `Connected → Healthy → Degraded → Reconnecting → Failed`. Exponential backoff, min 2 healthy nodes required.
- **Event Decoder**: Compile-time ABI via `alloy::sol!` macro. Handles `Sync`, `Swap` (V2), `SwapV3`, `TokenExchange` (Curve), `BalancerSwap`, `TokensTraded` (Bancor). Topic matching ~4 CPU cycles, full decode <200ns/event.
- **Dispatch**: Lock-free `tokio::sync::broadcast` channels — `pool_updates_tx`, `new_block_tx`, `pending_tx_tx`.

### 2. DEX Pool Registry (`crates/pools/`)

- **`Pool` trait**: `protocol()`, `address()`, `tokens()`, `fee_bps()`, `get_amount_out()`, `get_amount_in()`, `update_state()`, `encode_swap()`, `liquidity_depth()`.
- **Protocol adapters**: UniswapV2 O(1), UniswapV3 O(n_ticks), Curve O(iterations), Balancer O(1), Bancor O(1), SushiSwap O(1).
- **Discovery pipeline**: Factory event monitor (`PairCreated`/`PoolCreated`) + static TOML registry + on-chain scan. Qualification: liquidity >$10K, 24h volume >$1K, age >100 blocks, rug-pull score <0.3. Tiered: Hot/Warm/Cold pools.

### 3. State Management (`crates/state/`)

- **In-Memory Store**: `DashMap<Address, Box<dyn Pool>>` for pool states, `HashMap<(Token, Token), Vec>` pair index.
- **Price Graph**: Directed graph with **negative log-transformed exchange rates** as edge weights (`-ln(rate)`). Profitable cycle = negative weight cycle (sum < 0). Only dirty edges recomputed per update.
- **MVCC Snapshots**: `Arc<ArcSwap<GraphSnapshot>>` — writers atomically swap new versions; readers get zero-copy immutable references.

### 4. Arbitrage Detection (`crates/detector/`)

- **Algorithm**: Modified Bellman-Ford (SPFA variant with SLF optimization). ~2-3x faster than standard BF. Detects negative cycles (node relaxed N times). Early exit on first find.
- **Input Optimization**: Ternary search across profit function (~100 iterations, converges in ~60 for U256).
- **Gas Model**: Per-protocol base gas (UniV2: 60K, UniV3: 100K+5K/tick, Curve: 130K, Balancer: 120K, Bancor: 150K) + 80K flashloan + 21K base + 30K executor overhead.
- **Output**: Top-K `ArbOpportunity` sorted by net profit descending.

### 5. EVM Simulation (`crates/simulator/`)

- **Engine**: `revm` in fork mode — `CacheDB` + `EthersDB` backed by RPC.
- **Flow**: Build exact `AetherExecutor.executeArb()` calldata → execute in forked EVM → check `Success`/`Revert`/`Halt` → extract profit, gas used → emit `SimulationResult`.
- **Critical rule**: Simulation MUST use same block state as execution target. Stale simulations → reverted bundles.

### 6. Transaction Execution (`cmd/executor/`)

- **Bundle**: `[arb_tx, tip_tx]` — arb tx calls `AetherExecutor.executeArb()`, tip tx sends 90% of profit to builder coinbase.
- **Tx Type**: EIP-1559 `DynamicFeeTx` with current base fee + suggested priority fee.
- **Submission**: Goroutine fan-out to all configured builders (Flashbots, Titan, Beaver, rsync) simultaneously.
- **Nonce Manager**: Atomic local counter + periodic `eth_getTransactionCount` sync + pending tx tracker.

### 7. Smart Contract (`contracts/src/AetherExecutor.sol`)

- Implements `IFlashLoanSimpleReceiver` (Aave V3).
- `executeArb(SwapStep[] steps, address flashloanToken, uint256 flashloanAmount)` — entry point, calls `POOL.flashLoanSimple()`.
- `executeOperation()` — Aave callback: loops `_executeSwap()` per step, repays loan + premium, transfers profit to owner.
- `_executeSwap()` — routes to protocol-specific swap logic based on `step.protocol` enum.
- `rescue()` — emergency token withdrawal, `onlyOwner` only.
- Protocol constants: `UNISWAP_V2=1, UNISWAP_V3=2, SUSHISWAP=3, CURVE=4, BALANCER_V2=5, BANCOR_V3=6`.

### 8. Risk Management (`cmd/risk/`)

- **System States**: `Running → Degraded → Paused → Halted` (manual reset to resume from Halted).
- **Circuit Breakers**: Gas >300 gwei → HALT, 3 consecutive reverts in 10m → PAUSE, daily loss >0.5 ETH → HALT, ETH balance <0.1 ETH → HALT, node latency >500ms → DEGRADE, bundle miss rate >80%/1h → ALERT.
- **Position Limits**: Max single trade 50 ETH, max daily volume 500 ETH, min profit 0.001 ETH, max tip share 95%.

---

## Inter-Service Communication

- **Protocol**: gRPC + Protobuf over Unix Domain Sockets (sub-microsecond transport).
- **Schema**: `proto/aether.proto` — single source of truth between Rust (tonic) and Go (google.golang.org/grpc).
- **Services**:
  - `ArbService.SubmitArb()` — Rust → Go: submit validated arb for execution
  - `ArbService.StreamArbs()` — Rust → Go: server-side streaming of opportunities
  - `HealthService.Check()` — Go → Rust: engine health check
  - `ControlService.SetState()` — Go → Rust: pause/resume detection
  - `ControlService.ReloadConfig()` — Go → Rust: hot-reload pool config

---

## Infrastructure & Deployment

- **Server**: Bare metal, co-located at Equinix NY5 (same DC as major Eth node providers).
- **CPU Pinning**: Rust core on CPU 0-3 (Nice -20, realtime IO scheduling), Go executor on CPU 4-5 (Nice -15, GOMAXPROCS=2).
- **Kernel Tuning**: `tcp_nodelay=1`, `tcp_low_latency=1`, `swappiness=0`, 1024 huge pages, `sched_rt_runtime_us=-1`.
- **Failover**: Backup server with standby Reth node + Rust core + Go executor (Consul health checks).
- **Monitoring**: Prometheus (15s scrape) + Grafana dashboards + Loki log aggregation + AlertManager → PagerDuty/Telegram/Discord.
- **Storage**: PostgreSQL for trade ledger, Redis for state cache.

---

## Performance Targets

| Metric | Target |
|---|---|
| Event decode + state update | <1ms |
| Bellman-Ford detection | <3ms |
| EVM simulation (revm) | <5ms |
| gRPC Rust → Go | <1ms |
| Bundle build + sign | <2ms |
| **Total end-to-end** | **<15ms** |
| Events processed per block | 10,000+ |
| Pools monitored simultaneously | 5,000+ |
| Opportunities evaluated per block | 500+ |
| Simulations per second | 200+ |
| Rust core memory | <2 GB RSS |
| Go executor memory | <512 MB RSS |

---

## Security Invariants

1. Searcher EOA (hot wallet) holds only ~0.5 ETH for gas. Profits swept to cold wallet every 100 blocks.
2. Private keys loaded from HSM/KMS at startup, held only in memory. Never on disk in plaintext.
3. `AetherExecutor.rescue()` callable only by owner cold wallet, never the searcher hot wallet.
4. All outbound connections pinned to known IP ranges (Flashbots, builder endpoints). No arbitrary outbound traffic.
5. Network hardened: iptables firewall, WireGuard VPN for admin, mTLS for gRPC if over network.

---

## Key Observability Metrics

| Metric | Type | Alert Threshold |
|---|---|---|
| `aether_opportunities_detected_total` | Counter | <10/min → warn |
| `aether_bundles_included_total` | Counter | inclusion rate <20% → alert |
| `aether_detection_latency_ms` | Histogram | p99 >10ms → warn |
| `aether_simulation_latency_ms` | Histogram | p99 >50ms → warn |
| `aether_end_to_end_latency_ms` | Histogram | p99 >100ms → alert |
| `aether_gas_price_gwei` | Gauge | >300 → halt |
| `aether_daily_pnl_eth` | Gauge | <-0.5 ETH → halt |
| `aether_eth_balance` | Gauge | <0.1 ETH → halt |
| `aether_decode_errors_total` | Counter | sustained >10/min → warn (ABI drift or malformed event floods) |

---

## Development Guidelines

### Rust Crates
- Use `cargo build --release` with LTO for production builds.
- All hot-path code must be `#[inline]` where beneficial. Avoid heap allocations in the detection loop.
- Pool pricing functions must exactly replicate on-chain math — any deviation causes simulation-to-execution mismatches.
- Use `alloy::sol!` macro for compile-time ABI codegen. Never manually parse ABI.
- Run `cargo clippy` and `cargo test` before committing.

### Go Services
- `GOMAXPROCS=2`, `GOGC=200` in production.
- Use `context.Context` for cancellation propagation. All goroutines must respect context cancellation.
- Bundle construction must be deterministic — same input always produces same bundle.

### Solidity Contracts
- Use Foundry (`forge`) for build, test, and deployment.
- All external calls must use `SafeERC20` for token transfers.
- Every swap step must check `minAmountOut` for slippage protection (1% default).
- `onlyOwner` on all state-changing functions.

### Configuration
- `config/pools.toml` — Hot-reloadable via `ControlService.ReloadConfig()`. No restart needed.
- `config/risk.yaml` — Risk parameters. Changes require Go executor restart.
- `config/nodes.yaml` — Node provider endpoints (WS URLs, IPC paths).
- `config/builders.yaml` — Block builder API endpoints and auth keys.

### Adding a New DEX
1. Implement the `Pool` trait in `crates/pools/src/<new_dex>.rs`.
2. Add event signature to `crates/ingestion/src/event_decoder.rs` (new `sol!` event + match arm).
3. Add protocol variant to `ProtocolType` enum in `crates/common/src/types.rs`.
4. Add swap routing in `contracts/src/AetherExecutor.sol` `_executeSwap()`.
5. Add gas estimate in `crates/detector/src/gas.rs`.
6. Add pool config entry in `config/pools.toml`.
7. No changes needed to detection or execution logic.
