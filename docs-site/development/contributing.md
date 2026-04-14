# Contributing

Development guidelines, code style, and CI expectations for contributing to Aether.

## Development Setup

See [Getting Started](/guide/getting-started) for prerequisites and build instructions.

## Rust Guidelines

### Code Quality

```bash
# Run before every commit
cargo clippy
cargo test
```

### Hot Path Rules

Code in the detection loop (`crates/detector/`, `crates/state/`) must follow these rules:

- **No heap allocations** — Use arena allocators or stack-allocated buffers
- **Use `#[inline]`** where beneficial for hot functions
- **No `Box<dyn Trait>`** on the hot path — use enums or generics
- **No `String` formatting** in the critical path — use pre-allocated buffers

### ABI Decoding

Always use the `alloy::sol!` macro for compile-time ABI code generation:

```rust
alloy::sol! {
    event Sync(uint112 reserve0, uint112 reserve1);
    event Swap(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );
}
```

Never manually parse ABI — it's error-prone and slower.

### Pool Pricing

Pool pricing functions (`get_amount_out`, `get_amount_in`) **must exactly replicate on-chain math**. Any deviation causes simulation-to-execution mismatches, which means reverted bundles. Test against forked mainnet state.

### Production Builds

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

LTO is enabled in the workspace `Cargo.toml` for release builds.

## Go Guidelines

### Runtime Settings

Production settings:
```
GOMAXPROCS=2    # Limit OS threads
GOGC=200        # Reduce GC frequency
```

### Context Propagation

All goroutines **must** respect `context.Context` for cancellation:

```go
func (s *Submitter) Submit(ctx context.Context, bundle *Bundle) error {
    select {
    case <-ctx.Done():
        return ctx.Err()
    default:
        // proceed
    }
}
```

### Deterministic Bundles

Bundle construction must be deterministic — the same input must always produce the same bundle. This enables reproducible testing and debugging.

## Solidity Guidelines

### Build & Test

```bash
cd contracts
forge build
forge test
```

### Safety Rules

- All external calls must use **`SafeERC20`** for token transfers
- Every swap step must check **`minAmountOut`** for slippage protection (1% default)
- **`onlyOwner`** on all state-changing functions
- **`nonReentrant`** on entry points that handle value

### Testing

Write tests for:
- Successful multi-hop execution
- Revert on unprofitable trades
- Access control enforcement
- Edge cases (zero amounts, single-hop, max hops)

## CI Pipeline

The CI runs on every pull request with these jobs:

### 1. Toolchain Verification
Checks that Rust, Go, Foundry, and protoc are at the correct versions.

### 2. Rust Checks
```bash
cargo clippy -- -D warnings
cargo test
```

### 3. Go Checks
```bash
go vet ./...
go test ./...
```

### 4. Solidity Checks
```bash
cd contracts && forge test
```

All four jobs must pass before merging.

## Commit Style

- Use conventional commits: `feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`
- Keep commits focused — one logical change per commit
- Write meaningful commit messages that explain *why*, not just *what*

## Pull Requests

- Keep PRs focused and reviewable (< 500 lines when possible)
- Include test coverage for new functionality
- Update documentation if changing public interfaces or behavior
- Link related issues in the PR description
