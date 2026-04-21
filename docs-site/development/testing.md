# Testing

Aether's test strategy spans three languages and multiple test levels.

## Quick Start

```bash
# Run everything
./scripts/deploy.sh test

# Or individually:
cargo test           # Rust
go test ./...        # Go
cd contracts && forge test  # Solidity
```

## Rust Tests

### Unit Tests

Each crate has tests alongside the source code:

```bash
# All Rust tests
cargo test

# Specific crate
cargo test -p detector
cargo test -p pools
cargo test -p simulator

# Specific test
cargo test -p detector bellman_ford::tests::detects_negative_cycle
```

Key test areas:
- **`pools`** — Pricing math accuracy against known on-chain values
- **`detector`** — Bellman-Ford cycle detection, optimizer convergence
- **`simulator`** — EVM simulation results against forked state
- **`state`** — Price graph construction, dirty bit tracking, MVCC correctness
- **`ingestion`** — Event decoding for all supported protocols

### Integration Tests

The `crates/integration-tests/` crate runs end-to-end tests that exercise the full Rust pipeline:

```bash
cargo test -p integration-tests
```

These tests:
- Set up a mock price graph with known arbitrage opportunities
- Verify the detector finds them
- Verify the simulator validates them
- Check latency metrics

### Linting

```bash
cargo clippy -- -D warnings
```

All clippy warnings are treated as errors in CI.

## Go Tests

```bash
# All Go tests
go test ./...

# With verbose output
go test -v ./...

# Specific package
go test -v ./cmd/executor/...
go test -v ./cmd/risk/...

# With race detection
go test -race ./...
```

Key test areas:
- **`cmd/executor`** — Bundle construction determinism, nonce management
- **`cmd/risk`** — Circuit breaker state transitions, position limit enforcement
- **`cmd/monitor`** — Metric registration, alert dispatch

### Go Vet

```bash
go vet ./...
```

Run alongside tests to catch common issues.

## Solidity Tests

Uses Foundry's testing framework:

```bash
cd contracts

# Run all tests
forge test

# With verbosity (show traces)
forge test -vvv

# Specific test
forge test --match-test testExecuteArbSuccess

# Gas report
forge test --gas-report
```

Test file: `contracts/test/AetherExecutor.t.sol`

Key test cases:
- Successful multi-hop arbitrage execution
- Revert on unprofitable trades (flash loan repayment failure)
- Access control (`onlyOwner` enforcement)
- Reentrancy protection
- Slippage protection (`minAmountOut`)
- Emergency `rescue()` function
- Each protocol's swap routing

### Fork Testing

Test against forked Ethereum mainnet:

```bash
forge test --fork-url https://eth-mainnet.g.alchemy.com/v2/$ALCHEMY_API_KEY
```

## CI Pipeline

The CI workflow (`.github/workflows/ci.yml`) runs on every pull request:

| Job | What it runs | Must pass |
|---|---|---|
| Toolchain | Version checks (Rust, Go, Foundry, protoc) | Yes |
| Rust | `cargo clippy`, `cargo test` | Yes |
| Go | `go vet`, `go test` | Yes |
| Solidity | `forge test` | Yes |

All four jobs must pass before merging.

## Staging Validation

Before production deployment, run the staging test suite:

```bash
./scripts/staging_test.sh
```

This runs against a staging environment with:
- Forked mainnet state
- Real node connections (testnet or forked)
- Full pipeline execution (detection → simulation → bundle construction)
- No actual submission to builders

## Gas Profiling

Profile gas usage per protocol:

```bash
python scripts/gas_profiler.py
```

Outputs gas usage per swap type, helping calibrate the per-protocol gas constants in `crates/common/src/types.rs` (`ProtocolType::base_gas()`).

## Backtesting

Analyze historical arbitrage opportunities:

```bash
python scripts/backtest.py --from-block 18000000 --to-block 18001000
```

Useful for:
- Validating detection accuracy against known historical opportunities
- Measuring theoretical vs. actual profit
- Benchmarking detection latency against historical data
