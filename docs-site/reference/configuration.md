# Configuration Reference

Complete reference for all Aether configuration files.

## `config/pools.toml`

**Hot-reloadable:** Yes (via `grpcurl -plaintext localhost:50051 aether.ControlService/ReloadConfig`)

Defines the DEX pools that Aether monitors for arbitrage opportunities.

### Pool Entry

```toml
[[pools]]
protocol = "uniswap_v2"
address = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"
token0 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token1 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
fee_bps = 30
tier = "hot"
```

| Field | Type | Required | Description |
|---|---|---|---|
| `protocol` | string | Yes | Protocol identifier (see below) |
| `address` | string | Yes | Pool contract address (checksummed) |
| `token0` | string | Yes | First token address |
| `token1` | string | Yes | Second token address |
| `fee_bps` | integer | Yes | Pool fee in basis points (30 = 0.3%) |
| `tier` | string | Yes | Update frequency tier |
| `tick_spacing` | integer | UniV3 only | Tick spacing for Uniswap V3 pools |

### Protocol Values

| Value | Protocol |
|---|---|
| `uniswap_v2` | Uniswap V2 |
| `uniswap_v3` | Uniswap V3 |
| `sushiswap` | SushiSwap |
| `curve` | Curve Finance |
| `balancer_v2` | Balancer V2 |
| `bancor_v3` | Bancor V3 |

### Tier Values

| Value | Update Frequency | Use For |
|---|---|---|
| `hot` | Every block | High-liquidity, frequently-traded pools |
| `warm` | Every N blocks | Medium-activity pools |
| `cold` | On-demand | Low-activity pools, checked only when graph suggests opportunity |

### Pool Qualification Criteria

Pools must meet these thresholds to be added:
- Liquidity >$10,000
- 24h volume >$1,000
- Age >100 blocks
- Rug-pull score <0.3

---

## `config/risk.yaml`

**Hot-reloadable:** No (requires `sudo systemctl restart aether-go`)

### Circuit Breakers

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
```

| Field | Type | Default | Description |
|---|---|---|---|
| `max_gas_gwei` | integer | 300 | Gas price threshold → HALT |
| `consecutive_reverts_pause` | integer | 10 | Bug revert count → PAUSE |
| `revert_window_minutes` | integer | 10 | Window for counting reverts |
| `daily_loss_halt_eth` | float | 0.5 | Daily loss threshold → HALT |
| `min_eth_balance` | float | 0.1 | Min searcher balance → HALT |
| `max_node_latency_ms` | integer | 500 | Node latency threshold → DEGRADE |
| `bundle_miss_rate_alert_pct` | integer | 80 | Bundle miss rate → ALERT |
| `bundle_miss_rate_window_minutes` | integer | 60 | Window for miss rate calculation |
| `competitive_revert_alert_pct` | integer | 90 | Competitive revert rate → ALERT |

::: tip
Only bug reverts count toward `consecutive_reverts_pause`. Competitive/MEV reverts (where another searcher captured the same opportunity) are excluded.
:::

### Position Limits

```yaml
position_limits:
  max_single_trade_eth: 50.0
  max_daily_volume_eth: 500.0
  min_profit_eth: 0.001
  min_tip_share_pct: 50
  max_tip_share_pct: 95
```

| Field | Type | Default | Description |
|---|---|---|---|
| `max_single_trade_eth` | float | 50.0 | Maximum ETH value per trade |
| `max_daily_volume_eth` | float | 500.0 | Maximum daily trading volume |
| `min_profit_eth` | float | 0.001 | Minimum net profit to execute |
| `min_tip_share_pct` | integer | 50 | Minimum % of profit sent to builder |
| `max_tip_share_pct` | integer | 95 | Maximum % of profit sent to builder |

### System

```yaml
system:
  initial_state: "running"
  manual_reset_required_from_halted: true
```

| Field | Type | Default | Description |
|---|---|---|---|
| `initial_state` | string | `running` | State on startup (`running`, `paused`) |
| `manual_reset_required_from_halted` | boolean | true | Require manual intervention to exit HALTED |

---

## `config/nodes.yaml`

**Hot-reloadable:** No (requires `sudo systemctl restart aether-rust`)

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

### Node Entry

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | Yes | Human-readable identifier |
| `url` | string | Yes | Connection URL (WS URL or IPC path) |
| `type` | string | Yes | `websocket` or `ipc` |
| `priority` | integer | Yes | Lower = preferred (0 is highest priority) |

### Global Settings

| Field | Type | Default | Description |
|---|---|---|---|
| `min_healthy_nodes` | integer | 2 | Minimum healthy nodes before degrading |

### Environment Variables

URLs support `${VAR_NAME}` syntax for environment variable expansion. Variables are expanded at startup.

---

## `config/builders.yaml`

**Hot-reloadable:** No (requires `sudo systemctl restart aether-go`)

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

### Builder Entry

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | Yes | Human-readable identifier |
| `url` | string | Yes | Builder API endpoint URL |
| `enabled` | boolean | Yes | Whether to submit to this builder |
| `timeout_ms` | integer | Yes | Request timeout in milliseconds |
| `auth_type` | string | Yes | Authentication type (see below) |
| `auth_key` | string | No | API key (for `api_key` auth type) |

### Auth Types

| Value | Description |
|---|---|
| `flashbots` | Flashbots-style signed header (uses searcher private key) |
| `api_key` | Bearer token authentication (set via `auth_key` field) |
| `none` | No authentication |

### Submission Settings

| Field | Type | Default | Description |
|---|---|---|---|
| `fan_out` | boolean | true | Submit to all enabled builders simultaneously |
| `max_retries` | integer | 2 | Max retry attempts per builder per bundle |
