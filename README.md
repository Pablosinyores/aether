# Aether

**Production-grade, cross-DEX arbitrage engine for Ethereum Mainnet.**

Sub-millisecond opportunity detection across Uniswap V2/V3, SushiSwap, Curve, Balancer, Bancor, and 1inch — with Flashbots-native bundle execution, on-chain simulation via `revm`, and extensible pool registry.

---

## Tech Stack

| Layer | Language | Key Libraries |
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
| Infrastructure | — | Prometheus, Grafana, Loki |

---

## Architecture

The system is organized into **7 distinct layers** with clear ownership boundaries:

```
Eth Nodes (WS/IPC)
    │
    ▼
┌─────────────────── RUST CORE (Latency-Critical) ──────────────────┐
│  Data Ingestion → DEX Pool Registry → State Management            │
│       → Arbitrage Detection (Bellman-Ford) → EVM Simulator (revm) │
└──────────────────────────┬────────────────────────────────────────-┘
                           │ gRPC over UDS (<1μs)
┌──────────────────────────▼────────────────────────────────────────-┐
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
│   ├── pools/                    # DEX pool implementations (6 protocols)
│   ├── state/                    # State management & price graph
│   ├── detector/                 # Arbitrage detection engine (Bellman-Ford)
│   ├── simulator/                # EVM simulation (revm)
│   ├── grpc-server/              # tonic gRPC server (Rust binary entry point)
│   └── common/                   # Shared types, utils, errors
│
├── cmd/                          # ── Go Services ──
│   ├── executor/                 # Bundle construction & multi-builder submission
│   ├── risk/                     # Risk management & circuit breakers
│   └── monitor/                  # Prometheus metrics, dashboard, alerting
│
├── contracts/                    # ── Solidity ──
│   ├── src/AetherExecutor.sol    # Flashloan receiver + multi-DEX swap router
│   ├── test/AetherExecutor.t.sol
│   └── foundry.toml
│
├── config/                       # Runtime configuration
│   ├── pools.toml                # Pool registry (hot-reloadable)
│   ├── risk.yaml                 # Risk parameters & circuit breaker thresholds
│   ├── nodes.yaml                # Ethereum node provider endpoints
│   └── builders.yaml             # Block builder API endpoints
│
├── deploy/
│   ├── systemd/                  # aether-rust.service, aether-go.service
│   ├── ansible/                  # Server provisioning playbooks
│   └── docker/                   # Docker Compose, Dockerfiles, Prometheus config
│
├── scripts/
│   ├── backtest.py               # Historical opportunity analysis
│   ├── gas_profiler.py           # Gas usage profiling
│   └── deploy.sh                 # Build, test, deploy automation
│
└── docs/
    ├── architecture.md           # Detailed architecture documentation
    ├── runbook.md                # Operational procedures
    └── incident-response.md      # Incident response playbook
```

---

## Prerequisites

- **Rust** 1.94.1 (via [rustup](https://rustup.rs/))
- **Go** 1.26.1
- **Foundry** ([forge, cast, anvil](https://getfoundry.sh/))
- **Protobuf compiler** (`protoc`)
- **Docker & Docker Compose** (for local infrastructure)

---

## Build

### Rust Core

```bash
cargo build --release
```

For production with LTO:

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

### Go Executor

```bash
go build -o bin/aether-executor ./cmd/executor
```

### Solidity Contracts

```bash
cd contracts && forge build
```

### All at once (via deploy script)

```bash
./scripts/deploy.sh build
```

---

## Test

```bash
# Rust tests
cargo test

# Go tests
go test ./...

# Solidity tests
cd contracts && forge test

# All tests
./scripts/deploy.sh test
```

---

## Configuration

All configuration lives in the `config/` directory:

| File | Purpose | Hot-Reload |
|---|---|---|
| `config/pools.toml` | Pool registry — monitored DEX pools | Yes (via `ControlService.ReloadConfig()`) |
| `config/risk.yaml` | Risk parameters & circuit breaker thresholds | No (requires Go restart) |
| `config/nodes.yaml` | Ethereum node provider endpoints (WS/IPC) | No |
| `config/builders.yaml` | Block builder API endpoints & auth | No |

---

## Running

### Local Development (Docker Compose)

Start the full stack locally with infrastructure services:

```bash
./scripts/deploy.sh docker up
```

This starts: `aether-rust`, `aether-go`, Prometheus.

### Manual Start

```bash
# 1. Start infrastructure
docker compose -f deploy/docker/docker-compose.yml up -d prometheus

# 2. Start Rust core (gRPC server)
cargo run --release --bin aether-grpc-server

# 3. Start Go executor
go run ./cmd/executor
```

### Production Deployment

```bash
# Deploy to staging
./scripts/deploy.sh deploy staging

# Deploy to production
./scripts/deploy.sh deploy production

# Check status
./scripts/deploy.sh status production

# Rollback if needed
./scripts/deploy.sh rollback production
```

See [`docs/runbook.md`](docs/runbook.md) for detailed operational procedures.

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
| Pools monitored | 5,000+ |
| Simulations per second | 200+ |
| Rust core memory | <2 GB RSS |
| Go executor memory | <512 MB RSS |

---

## Risk Management

The system enforces automatic circuit breakers:

| Condition | Action |
|---|---|
| Gas price >300 gwei | **HALT** |
| 3 consecutive reverts in 10min | **PAUSE** |
| Daily loss >0.5 ETH | **HALT** |
| ETH balance <0.1 ETH | **HALT** |
| Node latency >500ms | **DEGRADE** |
| Bundle miss rate >80% in 1h | **ALERT** |

System states: `Running → Degraded → Paused → Halted` (manual reset required from Halted).

---

## Monitoring

Prometheus metrics are exposed on port 9090 (`/metrics`). Key metrics:

- `aether_opportunities_detected_total` — Arbitrage opportunities found
- `aether_bundles_included_total` — Bundles included on-chain
- `aether_detection_latency_ms` — Detection pipeline latency
- `aether_end_to_end_latency_ms` — Full pipeline latency
- `aether_gas_price_gwei` — Current gas price
- `aether_daily_pnl_eth` — Daily profit & loss
- `aether_eth_balance` — Searcher wallet balance

See [`docs/architecture.md`](docs/architecture.md) for the full metrics table.

---

## Adding a New DEX

1. Implement the `Pool` trait in `crates/pools/src/<new_dex>.rs`
2. Add event signature to `crates/ingestion/src/event_decoder.rs`
3. Add protocol variant to `ProtocolType` enum in `crates/common/src/types.rs`
4. Add swap routing in `contracts/src/AetherExecutor.sol` `_executeSwap()`
5. Add gas estimate in `crates/detector/src/gas.rs`
6. Add pool config entry in `config/pools.toml`
7. No changes needed to detection or execution logic

---

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — System architecture deep dive
- [`docs/runbook.md`](docs/runbook.md) — Operational procedures and service management
- [`docs/incident-response.md`](docs/incident-response.md) — Incident response playbook (SEV1-SEV4)

---

## Security

- All arb transactions are flashloan-backed — zero capital at risk
- Searcher EOA (hot wallet) holds minimal ETH (~0.5 ETH for gas)
- Profits swept to cold wallet every 100 blocks
- Private keys loaded from HSM/KMS at startup, never stored on disk
- All outbound connections pinned to known IP ranges
- Network hardened: iptables, WireGuard VPN, mTLS for gRPC

See [`docs/incident-response.md`](docs/incident-response.md) for security incident procedures.
