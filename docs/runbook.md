# Aether Operations Runbook

## Quick Reference

| Action | Command |
|---|---|
| Start services | `sudo systemctl start aether-rust aether-go` |
| Stop services | `sudo systemctl stop aether-go aether-rust` |
| Restart all | `sudo systemctl restart aether-rust && sleep 2 && sudo systemctl restart aether-go` |
| View logs | `journalctl -u aether-rust -u aether-go -f` |
| Check status | `systemctl status aether-rust aether-go` |
| Dashboard | `http://<host>:8080` |
| Metrics | `http://<host>:9090/metrics` |

## Service Management

### Starting the System

Always start Rust core before Go executor:

```bash
sudo systemctl start aether-rust
# Wait for gRPC server to be ready
sleep 2
sudo systemctl start aether-go
```

### Stopping the System

Stop Go executor first to prevent in-flight bundle submissions:

```bash
sudo systemctl stop aether-go
# Wait for pending bundles to complete
sleep 3
sudo systemctl stop aether-rust
```

### Graceful Restart

```bash
# Stop Go first
sudo systemctl stop aether-go
sleep 2

# Restart Rust core
sudo systemctl restart aether-rust
sleep 2

# Start Go executor
sudo systemctl start aether-go
```

### Service Dependencies

```
aether-go depends on: aether-rust
```

## Configuration

### Hot Reload Pool Config

Pool configuration can be reloaded without restart:

```bash
# Edit pool config
vim /opt/aether/config/pools.toml

# Trigger reload via gRPC
grpcurl -plaintext localhost:50051 aether.ControlService/ReloadConfig
```

### Risk Parameter Changes

Risk parameters require Go executor restart:

```bash
vim /opt/aether/config/risk.yaml
sudo systemctl restart aether-go
```

### Node Provider Changes

```bash
vim /opt/aether/config/nodes.yaml
sudo systemctl restart aether-rust
```

## Monitoring

### Key Metrics to Watch

| Metric | Normal Range | Action if Abnormal |
|---|---|---|
| `aether_opportunities_detected_total` rate | >10/min | Check node connectivity |
| `aether_bundles_included_total` rate | >20% inclusion | Check builder endpoints |
| `aether_detection_latency_ms` p99 | <10ms | Check CPU load, price graph size |
| `aether_simulation_latency_ms` p99 | <50ms | Check RPC latency |
| `aether_end_to_end_latency_ms` p99 | <100ms | Check all components |
| `aether_gas_price_gwei` | <100 gwei | System auto-halts at 300 |
| `aether_daily_pnl_eth` | >0 | Review strategy, check for MEV competition |
| `aether_eth_balance` | >0.1 ETH | Top up searcher wallet |

### Prometheus Queries

```promql
# Opportunity detection rate (5min window)
rate(aether_opportunities_detected_total[5m])

# Bundle inclusion rate
rate(aether_bundles_included_total[5m]) / rate(aether_bundles_submitted_total[5m])

# Detection latency p99
histogram_quantile(0.99, rate(aether_detection_latency_ms_bucket[5m]))

# Daily PnL
aether_daily_pnl_eth

# Gas price trend
aether_gas_price_gwei
```

### Log Analysis

```bash
# Recent errors
journalctl -u aether-rust -u aether-go --since "1 hour ago" -p err

# Search for revert reasons
journalctl -u aether-rust --since "1 hour ago" | grep -i "revert"

# Bundle submission results
journalctl -u aether-go --since "1 hour ago" | grep "bundle"

# Node connection issues
journalctl -u aether-rust --since "1 hour ago" | grep -i "reconnect\|disconnect\|timeout"
```

## System States

The risk manager operates a state machine:

| State | Meaning | Automatic Recovery |
|---|---|---|
| **Running** | Normal operation | N/A |
| **Degraded** | Reduced functionality (e.g., node latency) | Yes — returns to Running when condition clears |
| **Paused** | Arb detection paused (e.g., consecutive reverts) | Yes — after cooldown period (10 min default) |
| **Halted** | All operations stopped | **No** — requires manual intervention |

### Manual State Reset

To resume from Halted state:

```bash
# Check why system halted
journalctl -u aether-go --since "1 hour ago" | grep -i "halt"

# Fix the underlying issue, then:
grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
    -d '{"state": "RUNNING"}'
```

## Common Operational Scenarios

### High Gas Price

**Symptom**: System auto-halts when gas >300 gwei.

```bash
# Check current gas
curl -s http://localhost:9090/metrics | grep aether_gas_price_gwei

# Wait for gas to drop, then resume
grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
    -d '{"state": "RUNNING"}'
```

### Low ETH Balance

**Symptom**: System halts when searcher balance <0.1 ETH.

```bash
# Check balance
curl -s http://localhost:9090/metrics | grep aether_eth_balance

# Top up the searcher wallet, then resume
grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
    -d '{"state": "RUNNING"}'
```

### Node Connectivity Issues

**Symptom**: Degraded state, no new opportunities detected.

```bash
# Check node health
journalctl -u aether-rust --since "10 minutes ago" | grep -i "node\|connect"

# Verify node endpoints
curl -s -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    http://localhost:8545

# If persistent, update node config
vim /opt/aether/config/nodes.yaml
sudo systemctl restart aether-rust
```

### Consecutive Reverts

**Symptom**: System pauses after 3 reverts in 10 minutes.

```bash
# Check revert reasons
journalctl -u aether-rust --since "30 minutes ago" | grep -i "revert"

# Common causes:
# 1. Stale state (block reorg) — usually self-resolving
# 2. MEV competition — someone else extracting the same arb
# 3. Pool state changed between detection and execution

# System auto-resumes after cooldown. To force resume:
grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
    -d '{"state": "RUNNING"}'
```

### Deployment

```bash
# Build, test, deploy
./scripts/deploy.sh build
./scripts/deploy.sh test
./scripts/deploy.sh deploy prod

# Rollback if issues
./scripts/deploy.sh rollback prod
```

## Health Checks

### Manual Health Check Sequence

```bash
# 1. Service status
systemctl status aether-rust aether-go

# 2. gRPC health
grpcurl -plaintext localhost:50051 aether.HealthService/Check

# 3. Metrics endpoint
curl -s http://localhost:9090/metrics | head -20

# 4. Dashboard
curl -s http://localhost:8080/ | head -5

# 5. Node connectivity
curl -s -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    http://localhost:8545
```

## Maintenance

### Log Rotation

Logs are managed by journald. Configure retention:

```bash
# /etc/systemd/journald.conf
SystemMaxUse=2G
MaxRetentionSec=7d
```

### Profit Sweep

Profits are automatically swept from searcher wallet to cold wallet every 100 blocks. To trigger manual sweep:

```bash
# Check current searcher balance
curl -s http://localhost:9090/metrics | grep aether_eth_balance

# Manual sweep is handled by the Go executor
# Ensure cold wallet address is configured in risk.yaml
```

## Emergency Pause

`AetherExecutor.setPaused(bool)` is the fast-path circuit breaker for halting on-chain execution without touching ownership. It is separate from the Rust-side `ControlService/SetState` (which pauses opportunity detection in the Rust core) — pausing the contract stops `executeArb` from executing, while pausing the Rust side stops bundles from being submitted at all. Both may need to be toggled independently.

### Pause execution (owner only)

```bash
cast send <EXECUTOR_ADDRESS> "setPaused(bool)" true \
    --private-key <OWNER_KEY> --rpc-url <RPC_URL>
```

Any `executeArb` call will revert with `Paused()` while the flag is set.

### Resume execution

```bash
cast send <EXECUTOR_ADDRESS> "setPaused(bool)" false \
    --private-key <OWNER_KEY> --rpc-url <RPC_URL>
```

### Verify current state

```bash
cast call <EXECUTOR_ADDRESS> "paused()(bool)" --rpc-url <RPC_URL>
```

### Distinction from Rust-side pause

| What it stops | Command |
|---|---|
| On-chain `executeArb` (contract layer) | `cast send ... "setPaused(bool)" true` |
| Bundle submission (Go executor) | `grpcurl ... ControlService/SetState -d '{"state": "PAUSED"}'` |
| Opportunity detection (Rust core) | `grpcurl ... ControlService/SetState -d '{"state": "PAUSED"}'` |

For a full emergency stop, pause both layers. For MEV-risk scenarios where detection should continue but execution must stop, pause only the contract.

## Adding a DEX

The executor supports two pathways, matched to what's actually changing:

### (a) New instance of an existing protocol

Use this when a protocol ships a new Vault/Router address or we want to point at a fork that reuses the same interface (e.g., a new Balancer Vault deployment, an alternate Bancor network contract).

1. Owner multisig calls `setDexRouter(<protocolId>, <newAddress>)` on the executor.
   ```bash
   cast send <EXECUTOR_ADDRESS> "setDexRouter(uint8,address)" <PID> <NEW_ROUTER> \
       --private-key <OWNER_KEY> --rpc-url <RPC_URL>
   ```
   Protocol IDs: `UNISWAP_V2=1, UNISWAP_V3=2, SUSHISWAP=3, CURVE=4, BALANCER_V2=5, BANCOR_V3=6`.
   Only `5` and `6` currently store a router — the AMM protocols use per-swap pool addresses.
2. Update `config/pools.toml` with the new instance's pool entries.
3. Trigger Rust pool registry hot-reload:
   ```bash
   grpcurl -plaintext localhost:50051 aether.ControlService/ReloadConfig
   ```
4. No redeploy, no downtime.

Disabling a compromised DEX is the same mechanism:
```bash
cast send <EXECUTOR_ADDRESS> "setDexEnabled(uint8,bool)" <PID> false \
    --private-key <OWNER_KEY> --rpc-url <RPC_URL>
```
`executeArb` reverts pre-flashloan for any step whose protocol is disabled.

### (b) New protocol type

Use this when adding an AMM with novel swap semantics (e.g., Maverick, a new concentrated-liquidity variant) — the inline `_swapX` branch is hand-coded per protocol type, so a redeploy is unavoidable.

1. Implement the `Pool` trait in `crates/pools/src/<new>.rs`.
2. Add the event signature to `crates/ingestion/src/event_decoder.rs`.
3. Add the `ProtocolType` variant with its `uint8` ID and gas estimate in `crates/common/src/types.rs` and `crates/detector/src/gas.rs`.
4. Add a calldata builder in `crates/simulator/src/calldata.rs`.
5. Add a `_swap<New>()` helper + branch in `AetherExecutor._executeSwap()`; bump the `UnknownProtocol` upper bound in `executeArb`'s pre-flight loop and in `setDexRouter`/`setDexEnabled`.
6. Extend the constructor seeding loop so the new protocol is enabled at deploy.
7. Deploy:
   ```bash
   forge script script/Deploy.s.sol --rpc-url $MAINNET_RPC --broadcast
   ```
8. Update `EXECUTOR_ADDRESS` env on Rust and Go services, restart both, update `config/pools.toml`.
