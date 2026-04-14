# Tooling & Scripts

Utility scripts in the `scripts/` directory for building, testing, deploying, and analyzing Aether.

## `deploy.sh` — Build, Test, Deploy

The main automation script. Handles the complete lifecycle:

```bash
# Build all components (Rust, Go, Solidity)
./scripts/deploy.sh build

# Run all tests
./scripts/deploy.sh test

# Start local stack via Docker Compose
./scripts/deploy.sh docker up
./scripts/deploy.sh docker down

# Deploy to environment
./scripts/deploy.sh deploy staging
./scripts/deploy.sh deploy production

# Check deployment status
./scripts/deploy.sh status production

# Rollback to previous version
./scripts/deploy.sh rollback production
```

### Build Targets

| Command | What it builds |
|---|---|
| `deploy.sh build` | Rust (release + LTO), Go binary, Solidity contracts |
| `deploy.sh build rust` | Rust core only |
| `deploy.sh build go` | Go executor only |
| `deploy.sh build sol` | Solidity contracts only |

## `backtest.py` — Historical Analysis

Analyzes historical blocks for arbitrage opportunities that Aether could have captured:

```bash
python scripts/backtest.py --from-block 18000000 --to-block 18001000
```

Use cases:
- Validate detection accuracy against known historical opportunities
- Measure theoretical profit vs. actual profit
- Identify missed opportunities and their causes
- Benchmark detection latency

Requires an archive node endpoint for historical state access.

## `gas_profiler.py` — Gas Usage Profiling

Profiles actual gas usage per DEX protocol swap:

```bash
python scripts/gas_profiler.py
```

Outputs:
- Gas usage per protocol (UniV2, UniV3, Curve, etc.)
- Gas usage per hop count (2-hop, 3-hop, etc.)
- Comparison against the gas estimation model

Use this to calibrate the gas estimates in `crates/detector/src/gas.rs`. Estimates should be within 10% of actual usage.

## `staging_test.sh` — Staging Validation

End-to-end validation against a staging environment:

```bash
./scripts/staging_test.sh
```

Runs the full pipeline (detection → simulation → bundle construction) against forked mainnet state without submitting to actual builders. Use before production deployments.

## `check_toolchain_versions.sh` — Version Verification

Verifies all required toolchain versions match expectations:

```bash
./scripts/check_toolchain_versions.sh
```

Checks:
- Rust version (1.94.1+)
- Go version (1.26.1+)
- Foundry (forge, cast, anvil) installation
- Protobuf compiler (`protoc`) version
- Docker and Docker Compose availability

Run this on a new development machine or CI to verify the environment.

## `test_integration.sh` — Integration Tests

Runs the full integration test suite:

```bash
./scripts/test_integration.sh
```

Exercises the complete Rust pipeline end-to-end with test data, verifying detection accuracy and latency targets.
