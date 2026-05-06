#!/usr/bin/env bash
#
# mempool_smoke.sh — verify the Alchemy public-mempool path actually receives
# events on whatever tier the configured ALCHEMY_API_KEY belongs to.
#
# Boots aether-rust with MEMPOOL_TRACKING=1, lets the WS subscription run for
# DURATION seconds, scrapes /metrics twice (mid + end), then kills the binary
# and prints a verdict.
#
# PASS  → at least one pending DEX tx forwarded by the decoder
#         (aether_pending_dex_tx_total > 0)
# FAIL  → zero events seen during the window
#
# No Postgres / Anvil / executor needed — engine boots with NoopLedger when
# DATABASE_URL is unset, and we just want to read the mempool counters.
#
# Usage:
#   ./scripts/mempool_smoke.sh            # 60s window, default
#   DURATION=30 ./scripts/mempool_smoke.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Load .env so ALCHEMY_API_KEY / ETH_RPC_URL are available.
if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

if [ -z "${ALCHEMY_API_KEY:-}" ]; then
  echo "ERROR: ALCHEMY_API_KEY unset (looked in env + .env)" >&2
  exit 2
fi

DURATION="${DURATION:-60}"
METRICS_PORT="${RUST_METRICS_PORT:-9092}"

# Force the WS URL from the API key so we don't accidentally inherit an HTTPS
# ETH_RPC_URL (alchemy mempool needs wss://).
export MEMPOOL_TRACKING=1
export MEMPOOL_WS_URL="wss://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_API_KEY}"
export RUST_METRICS_PORT="$METRICS_PORT"
export RUST_LOG="${RUST_LOG:-info,aether=info,aether::mempool=debug}"
# Keep ledger off the smoke — NoopLedger path. Anything previously set wins.
unset DATABASE_URL || true

LOG_DIR="$ROOT/build/mempool-smoke"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/engine.log"
M30="$LOG_DIR/metrics_30s.txt"
M60="$LOG_DIR/metrics_${DURATION}s.txt"

# Prefer release binary (faster, less log noise).
BIN="target/release/aether-rust"
if [ ! -x "$BIN" ]; then
  BIN="target/debug/aether-rust"
fi
if [ ! -x "$BIN" ]; then
  echo "ERROR: no aether-rust binary found at target/{release,debug}/" >&2
  echo "Run: cargo build --release -p aether-grpc-server" >&2
  exit 2
fi

echo "==> using binary: $BIN"
echo "==> WS URL: wss://eth-mainnet.g.alchemy.com/v2/****${ALCHEMY_API_KEY: -4}"
echo "==> duration: ${DURATION}s, metrics port: $METRICS_PORT"
echo "==> logs: $LOG"

# Boot engine in background.
"$BIN" >"$LOG" 2>&1 &
PID=$!
echo "==> engine pid=$PID"

# Always clean up on exit even if we error out.
cleanup() {
  if kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    sleep 1
    kill -9 "$PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Wait for the metrics endpoint to come up (binary boots, opens port).
echo "==> waiting for /metrics on port $METRICS_PORT ..."
for i in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics" >/dev/null 2>&1; then
    echo "==> /metrics live after ${i}s"
    break
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "ERROR: engine exited during boot — see $LOG" >&2
    tail -30 "$LOG" >&2
    exit 3
  fi
  sleep 1
done

if ! curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics" >/dev/null 2>&1; then
  echo "ERROR: /metrics never came up — see $LOG" >&2
  exit 3
fi

# First sample at half the window, second at full.
HALF=$((DURATION / 2))
echo "==> sampling at +${HALF}s and +${DURATION}s ..."
sleep "$HALF"
curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics" >"$M30"
sleep $((DURATION - HALF))
curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics" >"$M60"

# Tear down.
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

# Parse.
extract() {
  local metric="$1"
  local file="$2"
  awk -v m="$metric" '$1 ~ "^"m"(\\{|$)" { sum += $NF } END { print sum+0 }' "$file"
}

DEX_30=$(extract aether_pending_dex_tx_total "$M30")
DEX_60=$(extract aether_pending_dex_tx_total "$M60")
DEC_30=$(extract aether_pending_decode_errors_total "$M30")
DEC_60=$(extract aether_pending_decode_errors_total "$M60")
LAG_60=$(extract aether_pending_pipeline_lagged_total "$M60")
SKP_60=$(extract aether_pending_arb_sim_skipped_total "$M60")
CAN_60=$(extract aether_pending_arb_candidates_total "$M60")

echo
echo "============================================================"
echo "SMOKE RESULT (window: ${DURATION}s)"
echo "============================================================"
printf "  pending DEX tx forwarded:   t+%2ds %-6s  t+%ds %-6s\n" "$HALF" "$DEX_30" "$DURATION" "$DEX_60"
printf "  pending decode errors:      t+%2ds %-6s  t+%ds %-6s\n" "$HALF" "$DEC_30" "$DURATION" "$DEC_60"
printf "  pipeline lagged events:     %s\n" "$LAG_60"
printf "  arb sim skipped (any reason): %s\n" "$SKP_60"
printf "  arb candidates produced:    %s\n" "$CAN_60"
echo

if [ "$DEX_60" -gt 0 ] || [ "$DEC_60" -gt 0 ]; then
  echo "VERDICT: PASS — Alchemy tier delivers pending txns to the subscription."
  echo "         Free tier sufficient for stage 1 live capture."
  exit 0
else
  echo "VERDICT: FAIL — zero pending DEX txns AND zero decoder failures."
  echo "         Either the WS subscription is silent (tier issue) or the"
  echo "         router-allowlist filter is rejecting everything before"
  echo "         counters increment. Inspect log:"
  echo "           $LOG"
  exit 1
fi
