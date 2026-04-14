# Configuration

All runtime configuration lives in the `config/` directory. This page provides an overview of each config file and how to modify them.

## Config Files Overview

| File | Purpose | Hot-Reload |
|---|---|---|
| `config/pools.toml` | Pool registry — monitored DEX pools | **Yes** (via `ControlService.ReloadConfig()`) |
| `config/risk.yaml` | Risk parameters & circuit breaker thresholds | No (requires Go restart) |
| `config/nodes.yaml` | Ethereum node provider endpoints (WS/IPC) | No (requires Rust restart) |
| `config/builders.yaml` | Block builder API endpoints & auth | No (requires Go restart) |

## Pool Registry (`pools.toml`)

This is the most frequently modified config. It defines which DEX pools Aether monitors.

```toml
[[pools]]
protocol = "uniswap_v2"
address = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"
token0 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token1 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
fee_bps = 30
tier = "hot"

[[pools]]
protocol = "uniswap_v3"
address = "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
token0 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token1 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
fee_bps = 5
tier = "hot"
tick_spacing = 10
```

**Fields:**
- `protocol` — One of: `uniswap_v2`, `uniswap_v3`, `sushiswap`, `curve`, `balancer_v2`, `bancor_v3`
- `address` — Pool contract address
- `token0`, `token1` — Token addresses in the pair
- `fee_bps` — Pool fee in basis points (30 = 0.3%)
- `tier` — `hot` (checked every block), `warm` (periodic), `cold` (on-demand)
- `tick_spacing` — Required for Uniswap V3 pools

### Hot Reload

Pool config can be reloaded without restarting any services:

```bash
# Edit the config
vim config/pools.toml

# Trigger reload via gRPC
grpcurl -plaintext localhost:50051 aether.ControlService/ReloadConfig
```

## Risk Parameters (`risk.yaml`)

Defines circuit breaker thresholds and position limits.

```yaml
circuit_breakers:
  max_gas_gwei: 300
  consecutive_reverts_pause: 10
  revert_window_minutes: 10
  daily_loss_halt_eth: 0.5
  min_eth_balance: 0.1
  max_node_latency_ms: 500
  bundle_miss_rate_alert_pct: 80
  bundle_miss_rate_window_minutes: 60
  competitive_revert_alert_pct: 90

position_limits:
  max_single_trade_eth: 50.0
  max_daily_volume_eth: 500.0
  min_profit_eth: 0.001
  min_tip_share_pct: 50
  max_tip_share_pct: 95

system:
  initial_state: "running"
  manual_reset_required_from_halted: true
```

::: warning
Changes to `risk.yaml` require restarting the Go executor: `sudo systemctl restart aether-go`
:::

See [Risk Parameters Reference](/reference/risk-parameters) for detailed documentation of each field.

## Node Providers (`nodes.yaml`)

Configures Ethereum node connections.

```yaml
nodes:
  - name: "alchemy-ws"
    url: "wss://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_API_KEY}"
    type: "websocket"
    priority: 1
  - name: "local-reth"
    url: "/tmp/reth.ipc"
    type: "ipc"
    priority: 0

min_healthy_nodes: 2
```

- `type` — `websocket` or `ipc`
- `priority` — Lower = preferred. IPC is fastest (same machine), WS is remote fallback.
- `min_healthy_nodes` — System degrades if fewer than this many nodes are healthy.

Environment variables (like `${ALCHEMY_API_KEY}`) are expanded at startup.

## Block Builders (`builders.yaml`)

Configures the block builders for bundle submission.

```yaml
builders:
  - name: "flashbots"
    url: "https://relay.flashbots.net"
    enabled: true
    timeout_ms: 2000
    auth_type: "flashbots"
  - name: "titan"
    url: "https://rpc.titanbuilder.xyz"
    enabled: true
    timeout_ms: 2000
    auth_type: "none"

submission:
  fan_out: true
  max_retries: 2
```

- `fan_out: true` — Submit to all enabled builders simultaneously (recommended)
- `auth_type` — `flashbots` (signed header), `api_key` (bearer token), or `none`

## Next Steps

- [Configuration Reference](/reference/configuration) — Full annotated reference for every config field
- [Risk Parameters](/reference/risk-parameters) — Detailed circuit breaker documentation
- [Runbook](/operations/runbook) — Operational procedures for config changes
