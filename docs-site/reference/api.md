# gRPC API Reference

Complete API reference for Aether's gRPC services, defined in `proto/aether.proto`.

## Connection

```
Socket: /tmp/aether.sock (Unix Domain Socket)
Fallback: localhost:50051 (TCP)
```

All examples use `grpcurl` with TCP for readability. In production, UDS is the default transport.

## Services

### ArbService

#### `SubmitArb`

Submit a validated arbitrage opportunity for execution.

```protobuf
rpc SubmitArb(ValidatedArb) returns (SubmitArbResponse);
```

**Request:** `ValidatedArb` (see [Messages](#validatedarb))

**Response:**

| Field | Type | Description |
|---|---|---|
| `accepted` | bool | Whether the arb was accepted for execution |
| `bundle_hash` | string | Bundle hash (if accepted) |
| `error` | string | Error message (if rejected) |

**Example:**

```bash
grpcurl -plaintext localhost:50051 aether.ArbService/SubmitArb \
    -d '{
        "id": "arb-001",
        "block_number": 18000000,
        "net_profit_wei": "AAAAAAA=",
        "total_gas": 250000
    }'
```

#### `StreamArbs`

Subscribe to a server-side stream of validated arbitrage opportunities.

```protobuf
rpc StreamArbs(StreamArbsRequest) returns (stream ValidatedArb);
```

**Request:**

| Field | Type | Description |
|---|---|---|
| `min_profit_eth` | double | Minimum net profit filter (ETH). Only opportunities above this threshold are streamed. |

**Response:** Stream of `ValidatedArb` messages.

**Example:**

```bash
# Stream all opportunities with >0.01 ETH profit
grpcurl -plaintext localhost:50051 aether.ArbService/StreamArbs \
    -d '{"min_profit_eth": 0.01}'
```

---

### HealthService

#### `Check`

Check the health of the Rust detection engine.

```protobuf
rpc Check(HealthCheckRequest) returns (HealthCheckResponse);
```

**Request:** Empty (no fields).

**Response:**

| Field | Type | Description |
|---|---|---|
| `healthy` | bool | Overall health status |
| `status` | string | Human-readable status message |
| `uptime_seconds` | int64 | Seconds since engine start |
| `last_block` | uint64 | Last processed block number |
| `active_pools` | uint32 | Number of actively monitored pools |

**Example:**

```bash
grpcurl -plaintext localhost:50051 aether.HealthService/Check
```

**Sample response:**

```json
{
  "healthy": true,
  "status": "running",
  "uptimeSeconds": "3600",
  "lastBlock": "18000000",
  "activePools": 4200
}
```

---

### ControlService

#### `SetState`

Set the system state (pause/resume detection).

```protobuf
rpc SetState(SetStateRequest) returns (SetStateResponse);
```

**Request:**

| Field | Type | Description |
|---|---|---|
| `state` | SystemState | Target state |
| `reason` | string | Optional reason for the state change |

**Response:**

| Field | Type | Description |
|---|---|---|
| `success` | bool | Whether the state change succeeded |
| `previous_state` | SystemState | State before the change |

**Examples:**

```bash
# Pause detection
grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
    -d '{"state": "PAUSED", "reason": "manual maintenance"}'

# Resume detection
grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
    -d '{"state": "RUNNING"}'

# Emergency halt
grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
    -d '{"state": "HALTED", "reason": "investigating anomaly"}'
```

#### `ReloadConfig`

Hot-reload the pool configuration from `pools.toml`.

```protobuf
rpc ReloadConfig(ReloadConfigRequest) returns (ReloadConfigResponse);
```

**Request:**

| Field | Type | Description |
|---|---|---|
| `config_path` | string | Optional custom config path (defaults to `config/pools.toml`) |

**Response:**

| Field | Type | Description |
|---|---|---|
| `success` | bool | Whether the reload succeeded |
| `pools_loaded` | uint32 | Number of pools loaded from config |
| `error` | string | Error message (if failed) |

**Example:**

```bash
# Reload default config
grpcurl -plaintext localhost:50051 aether.ControlService/ReloadConfig

# Reload from custom path
grpcurl -plaintext localhost:50051 aether.ControlService/ReloadConfig \
    -d '{"config_path": "/opt/aether/config/pools-staging.toml"}'
```

## Enums

### ProtocolType

| Value | Number | Description |
|---|---|---|
| `PROTOCOL_UNKNOWN` | 0 | Unknown/unset |
| `UNISWAP_V2` | 1 | Uniswap V2 (constant product) |
| `UNISWAP_V3` | 2 | Uniswap V3 (concentrated liquidity) |
| `SUSHISWAP` | 3 | SushiSwap (constant product) |
| `CURVE` | 4 | Curve Finance (StableSwap) |
| `BALANCER_V2` | 5 | Balancer V2 (weighted pools) |
| `BANCOR_V3` | 6 | Bancor V3 (bonding curve) |

### SystemState

| Value | Number | Description |
|---|---|---|
| `STATE_UNKNOWN` | 0 | Unknown/unset |
| `RUNNING` | 1 | Normal operation |
| `DEGRADED` | 2 | Reduced functionality |
| `PAUSED` | 3 | Detection paused |
| `HALTED` | 4 | All operations stopped |

## Messages

### ValidatedArb

A simulation-verified arbitrage opportunity ready for execution.

| Field | Number | Type | Description |
|---|---|---|---|
| `id` | 1 | string | Unique opportunity identifier |
| `hops` | 2 | repeated ArbHop | Hop sequence from detection |
| `total_profit_wei` | 3 | bytes | Gross profit (big-endian U256) |
| `total_gas` | 4 | uint64 | Estimated total gas usage |
| `gas_cost_wei` | 5 | bytes | Gas cost (big-endian U256) |
| `net_profit_wei` | 6 | bytes | Net profit after gas + premium |
| `block_number` | 7 | uint64 | Target execution block |
| `timestamp_ns` | 8 | int64 | Detection timestamp (ns since epoch) |
| `flashloan_token` | 9 | bytes | Token address to borrow |
| `flashloan_amount` | 10 | bytes | Amount to borrow (big-endian U256) |
| `steps` | 11 | repeated SwapStep | Execution steps with calldata |
| `calldata` | 12 | bytes | Pre-built AetherExecutor calldata |

### ArbHop

A single hop in the detected arbitrage path.

| Field | Number | Type | Description |
|---|---|---|---|
| `protocol` | 1 | ProtocolType | DEX protocol |
| `pool_address` | 2 | bytes | Pool contract address (20 bytes) |
| `token_in` | 3 | bytes | Input token address |
| `token_out` | 4 | bytes | Output token address |
| `amount_in` | 5 | bytes | Input amount (big-endian U256) |
| `expected_out` | 6 | bytes | Expected output amount |
| `estimated_gas` | 7 | uint64 | Estimated gas for this hop |

### SwapStep

A single swap step for on-chain execution.

| Field | Number | Type | Description |
|---|---|---|---|
| `protocol` | 1 | ProtocolType | DEX protocol |
| `pool_address` | 2 | bytes | Pool contract address |
| `token_in` | 3 | bytes | Input token address |
| `token_out` | 4 | bytes | Output token address |
| `amount_in` | 5 | bytes | Input amount |
| `min_amount_out` | 6 | bytes | Minimum output (slippage protection) |
| `calldata` | 7 | bytes | Protocol-specific swap calldata |
