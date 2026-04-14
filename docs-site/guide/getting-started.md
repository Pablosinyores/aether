# Getting Started

This guide covers building, testing, and running Aether locally.

## Prerequisites

- **Rust** 1.94.1+ (via [rustup](https://rustup.rs/))
- **Go** 1.26.1+
- **Foundry** — [forge, cast, anvil](https://getfoundry.sh/)
- **Protobuf compiler** (`protoc`)
- **Docker & Docker Compose** (for local infrastructure)

## Build

### Rust Core

```bash
cargo build --release
```

For production builds with LTO and native CPU optimizations:

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

### All Components (via deploy script)

```bash
./scripts/deploy.sh build
```

## Test

```bash
# Rust tests
cargo test

# Go tests
go test ./...

# Solidity tests
cd contracts && forge test

# All tests at once
./scripts/deploy.sh test
```

## Running

### Option 1: Docker Compose (Recommended for local dev)

Start the full stack with infrastructure services:

```bash
./scripts/deploy.sh docker up
```

This starts `aether-rust`, `aether-go`, and Prometheus.

### Option 2: Manual Start

```bash
# 1. Start infrastructure (Prometheus)
docker compose -f deploy/docker/docker-compose.yml up -d prometheus

# 2. Start Rust core (gRPC server)
cargo run --release --bin aether-grpc-server

# 3. Start Go executor (in a separate terminal)
go run ./cmd/executor
```

::: tip Start Order
Always start the Rust core **before** the Go executor. The Go executor connects to the Rust gRPC server on startup.
:::

### Option 3: Production Deployment

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

See [Deployment](/operations/deployment) for full production deployment instructions.

## Verify It's Working

Once running, check these endpoints:

| Endpoint | URL | Purpose |
|---|---|---|
| Prometheus metrics | `http://localhost:9090/metrics` | Raw metrics |
| Dashboard | `http://localhost:8080` | Operational dashboard |
| gRPC health | `grpcurl -plaintext localhost:50051 aether.HealthService/Check` | Engine health |

## Repository Structure

```
aether/
├── Cargo.toml                    # Rust workspace root
├── go.mod                        # Go module root
├── proto/aether.proto            # Shared Protobuf schema
├── crates/                       # Rust crates (7 crates)
│   ├── ingestion/                # Data ingestion & node pool
│   ├── pools/                    # DEX pool implementations
│   ├── state/                    # State management & price graph
│   ├── detector/                 # Arbitrage detection (Bellman-Ford)
│   ├── simulator/                # EVM simulation (revm)
│   ├── grpc-server/              # tonic gRPC server
│   └── common/                   # Shared types & errors
├── cmd/                          # Go services
│   ├── executor/                 # Bundle construction & submission
│   ├── risk/                     # Risk management & circuit breakers
│   └── monitor/                  # Prometheus, dashboard, alerting
├── contracts/                    # Solidity (Foundry)
│   └── src/AetherExecutor.sol    # Flash loan executor
├── config/                       # Runtime configuration
├── deploy/                       # Docker, systemd, Ansible
└── scripts/                      # Build, test, deploy automation
```

## Next Steps

- [Configuration](/guide/configuration) — Configure pools, risk parameters, and node providers
- [How It Works](/guide/how-it-works) — Understand the detection pipeline
- [Contributing](/development/contributing) — Development guidelines and code style
