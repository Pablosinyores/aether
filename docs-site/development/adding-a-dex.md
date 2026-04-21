# Adding a DEX

This guide walks through adding support for a new DEX protocol to Aether. The system is designed to make this straightforward — no changes to the detection or execution logic are needed.

## Overview

Adding a new DEX requires changes in 6 files across 3 languages:

| Step | File | Language |
|---|---|---|
| 1. Pool implementation | `crates/pools/src/<new_dex>.rs` | Rust |
| 2. Event decoding | `crates/ingestion/src/event_decoder.rs` | Rust |
| 3. Protocol enum | `crates/common/src/types.rs` | Rust |
| 4. Swap routing | `contracts/src/AetherExecutor.sol` | Solidity |
| 5. Gas estimation | `crates/common/src/types.rs` | Rust |
| 6. Pool config | `config/pools.toml` | TOML |

## Step 1: Implement the Pool Trait

Create `crates/pools/src/<new_dex>.rs` and implement the `Pool` trait:

```rust
use crate::Pool;
use common::{ProtocolType, SwapStep};
use alloy::primitives::{Address, Bytes, U256};

pub struct NewDexPool {
    address: Address,
    token0: Address,
    token1: Address,
    fee_bps: u32,
    // Protocol-specific state (reserves, weights, etc.)
}

impl Pool for NewDexPool {
    fn protocol(&self) -> ProtocolType {
        ProtocolType::NewDex
    }

    fn address(&self) -> Address {
        self.address
    }

    fn tokens(&self) -> (Address, Address) {
        (self.token0, self.token1)
    }

    fn fee_bps(&self) -> u32 {
        self.fee_bps
    }

    fn get_amount_out(&self, amount_in: U256, token_in: Address) -> U256 {
        // CRITICAL: Must exactly replicate on-chain math
        // Test against forked mainnet to verify
        todo!()
    }

    fn get_amount_in(&self, amount_out: U256, token_out: Address) -> U256 {
        todo!()
    }

    fn update_state(&mut self, event: &DecodedEvent) {
        // Update internal state from on-chain events
        todo!()
    }

    fn encode_swap(&self, step: &SwapStep) -> Bytes {
        // ABI-encode the swap calldata for on-chain execution
        todo!()
    }

    fn liquidity_depth(&self) -> U256 {
        // Return a measure of pool liquidity (used for qualification)
        todo!()
    }
}
```

::: warning Critical: Pricing Accuracy
`get_amount_out()` and `get_amount_in()` **must exactly replicate on-chain math**. Any deviation causes simulation-to-execution mismatches → reverted bundles → wasted gas. Always test against forked mainnet state.
:::

Don't forget to register the module in `crates/pools/src/lib.rs`:

```rust
pub mod new_dex;
```

## Step 2: Add Event Decoding

In `crates/ingestion/src/event_decoder.rs`, add the new protocol's event signatures:

```rust
alloy::sol! {
    // Add the new DEX's swap event
    event NewDexSwap(
        address indexed sender,
        uint256 amountIn,
        uint256 amountOut,
        address indexed to
    );
}
```

Then add a match arm in the event dispatcher to route these events to the pool update pipeline.

## Step 3: Add Protocol Variant

In `crates/common/src/types.rs`, add the new variant to the `ProtocolType` enum:

```rust
pub enum ProtocolType {
    UniswapV2,
    UniswapV3,
    SushiSwap,
    Curve,
    BalancerV2,
    BancorV3,
    NewDex,  // Add here
}
```

Also update the corresponding Protobuf enum in `proto/aether.proto`:

```protobuf
enum ProtocolType {
    PROTOCOL_UNKNOWN = 0;
    UNISWAP_V2 = 1;
    UNISWAP_V3 = 2;
    SUSHISWAP = 3;
    CURVE = 4;
    BALANCER_V2 = 5;
    BANCOR_V3 = 6;
    NEW_DEX = 7;  // Add here
}
```

## Step 4: Add Swap Routing (Solidity)

In `contracts/src/AetherExecutor.sol`, add a routing case in `_executeSwap()`:

```solidity
uint8 constant NEW_DEX = 7;

function _executeSwap(SwapStep calldata step) internal {
    // ... existing cases ...
    else if (step.protocol == NEW_DEX) {
        // Protocol-specific swap logic
        // Use SafeERC20 for all token transfers
        // Check minAmountOut for slippage protection
    }
}
```

Add corresponding tests in `contracts/test/AetherExecutor.t.sol`.

## Step 5: Add Gas Estimation

In `crates/common/src/types.rs`, add a new arm to the `ProtocolType::base_gas()` method:

```rust
impl ProtocolType {
    pub fn base_gas(&self) -> u64 {
        match self {
            ProtocolType::UniswapV2 => 60_000,
            ProtocolType::UniswapV3 => 180_000, // + 5000 per tick crossed
            ProtocolType::SushiSwap => 60_000,
            ProtocolType::Curve => 130_000,
            ProtocolType::BalancerV2 => 120_000,
            ProtocolType::BancorV3 => 150_000,
            ProtocolType::NewDex => 80_000, // Measure with gas_profiler.py
        }
    }
}
```

::: tip Measuring Gas
Use `scripts/gas_profiler.py` to measure actual gas usage on a forked mainnet. The estimate should be within 10% of actual usage.
:::

## Step 6: Add Pool Configuration

Add pool entries to `config/pools.toml`:

```toml
[[pools]]
protocol = "new_dex"
address = "0x..."
token0 = "0x..."
token1 = "0x..."
fee_bps = 30
tier = "warm"  # Start as warm, promote to hot after validation
```

## Testing Checklist

After implementing all steps:

1. **Unit tests** — Verify pricing math against known on-chain values
   ```bash
   cargo test -p pools
   ```

2. **Integration test** — Verify the full pipeline detects opportunities involving the new DEX
   ```bash
   cargo test -p integration-tests
   ```

3. **Solidity tests** — Verify swap routing works for the new protocol
   ```bash
   cd contracts && forge test
   ```

4. **Gas profiling** — Measure actual gas costs
   ```bash
   python scripts/gas_profiler.py --protocol new_dex
   ```

5. **Mainnet fork test** — Run against forked mainnet to validate end-to-end
   ```bash
   ./scripts/staging_test.sh
   ```

## What You Don't Need to Change

The following components work automatically with any new DEX:

- **Bellman-Ford detection** — Operates on the price graph, protocol-agnostic
- **Input optimization** — Ternary search works for any concave profit function
- **EVM simulation** — Simulates whatever calldata is provided
- **Bundle construction** — Wraps whatever calldata is provided
- **Multi-builder submission** — Protocol-agnostic
- **Risk management** — Monitors profits and gas, protocol-agnostic
- **Monitoring** — Tracks all opportunities and executions
