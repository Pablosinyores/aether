# Cargo Release Profile — Baseline

Tracks the impact of the tuned `[profile.release]` added in workspace
`Cargo.toml` (issue #68).

## Profile

```toml
[profile.release]
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "debuginfo"
debug = 1
```

## Rationale

- `lto = "fat"` — cross-crate link-time optimization; enables inlining across
  crate boundaries (critical for `crates/pools` math called from
  `crates/detector`).
- `codegen-units = 1` — single LLVM codegen unit allows global optimization.
- `panic = "abort"` — removes unwind tables; smaller binary, faster panic
  path (we never catch panics on the hot path).
- `strip = "debuginfo"` — smaller production binary.
- `debug = 1` — retains line-number info only, enough for `perf` /
  `cargo flamegraph` without bloating the binary with full DWARF.

## Measurements

Host: Darwin 23.2.0, Apple Silicon, `cargo clean` followed by
`cargo build --release --workspace`. Binary: `target/release/aether-rust`.

| Metric                             | Baseline (stock release) | Tuned               | Delta   |
|------------------------------------|--------------------------|---------------------|---------|
| Binary size                        | 14,796,448 B (14.11 MiB) | 8,614,416 B (8.21 MiB) | −41.8%  |
| Clean build time                   | 1m20s                    | 2m18s               | +72.5%  |
| `cargo test --workspace --release` | —                        | passes (exit 0)     | —       |

Build time increases because `lto = "fat"` + `codegen-units = 1` forces a
single-threaded, whole-program optimization pass at link time. This is a
one-time cost per clean build; incremental rebuilds are unaffected in
practice because most development uses the `dev` profile.

## Flamegraphs

Deferred. Capturing meaningful before/after flamegraphs for the hot path
(event decode → pool update → Bellman-Ford → revm simulation) requires
driving the full pipeline under load — either the `scripts/staging_test.sh`
harness on Linux with `perf` + `samply -p <pid>` (macOS `samply` does not
support PID attach), or a dedicated criterion microbenchmark for the
detector. Both are out of scope for this profile-tuning change and will be
handled in a follow-up.
