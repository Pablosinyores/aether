#!/usr/bin/env bash
#
# mempool_capture.sh — record a longer live-mainnet run of the mempool path
# under reports/mempool-stage1-proof/. Boots aether-rust with WS subscriptions
# (newHeads + alchemy_pendingTransactions), scrapes /metrics at the midpoint
# and at the end, parses the log into CSVs, and writes a summary.md with
# counters + sample tx hashes verifiable on Etherscan.
#
# Companion of scripts/mempool_smoke.sh — same env + boot, longer window,
# more artefacts, intended to be committed as proof of stage 1.
#
# Usage:
#   ./scripts/mempool_capture.sh              # 600 s window (default)
#   DURATION=300 ./scripts/mempool_capture.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

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

DURATION="${DURATION:-600}"
HALF=$((DURATION / 2))
METRICS_PORT="${RUST_METRICS_PORT:-9092}"

export MEMPOOL_TRACKING=1
export MEMPOOL_WS_URL="wss://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_API_KEY}"
export ETH_RPC_URL="${ETH_RPC_URL:-wss://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_API_KEY}}"
export RUST_METRICS_PORT="$METRICS_PORT"
# Keep mempool at debug for full tx hash trail in the log; everything else info.
export RUST_LOG="${RUST_LOG:-info,aether=info,aether::mempool=debug}"
unset DATABASE_URL || true

OUT_DIR="$ROOT/reports/mempool-stage1-proof"
mkdir -p "$OUT_DIR"
LOG="$OUT_DIR/02_engine.log"
M_MID="$OUT_DIR/03_metrics_${HALF}s.txt"
M_END="$OUT_DIR/04_metrics_${DURATION}s.txt"
ENV_FILE="$OUT_DIR/01_env.txt"

BIN="target/release/aether-rust"
if [ ! -x "$BIN" ]; then
  echo "ERROR: missing $BIN — run: cargo build --release -p aether-grpc-server" >&2
  exit 2
fi

# Redacted env dump — show key prefix/suffix only.
{
  echo "## Capture environment"
  echo "binary:        $BIN"
  echo "ALCHEMY_API_KEY: ${ALCHEMY_API_KEY:0:4}...${ALCHEMY_API_KEY: -4}  (len=${#ALCHEMY_API_KEY})"
  echo "ETH_RPC_URL:   ${ETH_RPC_URL%${ALCHEMY_API_KEY}}<key>"
  echo "MEMPOOL_WS_URL: ${MEMPOOL_WS_URL%${ALCHEMY_API_KEY}}<key>"
  echo "MEMPOOL_TRACKING: $MEMPOOL_TRACKING"
  echo "RUST_LOG:      $RUST_LOG"
  echo "metrics port:  $METRICS_PORT"
  echo "duration:      ${DURATION}s"
  echo "started at:    $(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$ENV_FILE"

echo "==> capture starting (${DURATION}s) → $OUT_DIR"
"$BIN" >"$LOG" 2>&1 &
PID=$!
echo "==> engine pid=$PID"

cleanup() {
  if kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    sleep 1
    kill -9 "$PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Wait for /metrics to come up.
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

echo "==> running for ${DURATION}s, scrape at +${HALF}s and +${DURATION}s"
sleep "$HALF"
curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics" >"$M_MID"
echo "==> mid-run metrics scraped"
sleep $((DURATION - HALF))
curl -fsS "http://127.0.0.1:$METRICS_PORT/metrics" >"$M_END"
echo "==> end-run metrics scraped"

kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

# Strip ANSI from the log once so all parsers see plain text.
PLAIN_LOG="$OUT_DIR/02_engine.log.plain"
perl -pe 's/\e\[[0-9;]*m//g' "$LOG" > "$PLAIN_LOG"

# Parse pending DEX swaps into CSV.
DEX_CSV="$OUT_DIR/05_pending_dex_txs.csv"
{
  echo "ts,tx_hash,router,protocol,token_in,token_out,amount_in,fee_bps"
  # Addresses are emitted in EIP-55 mixed case; the regex must tolerate
  # both cases or it truncates at the first uppercase byte and the CSV
  # ends up with two-character "token_in" values. Use [[:xdigit:]] so the
  # parser is locale-stable.
  grep "PENDING DEX SWAP decoded" "$PLAIN_LOG" \
    | sed -nE 's/^([^ ]+).*tx_hash=(0x[[:xdigit:]]+).*router=(0x[[:xdigit:]]+).*protocol=([A-Za-z0-9]+).*token_in=(0x[[:xdigit:]]+).*token_out=(0x[[:xdigit:]]+).*amount_in=([0-9]+).*fee_bps=([0-9]+).*/\1,\2,\3,\4,\5,\6,\7,\8/p'
} > "$DEX_CSV"

# Parse filter drops + decode errors into a "reasons" CSV by scraping end metrics.
DROPS_CSV="$OUT_DIR/06_filter_drops.csv"
{
  echo "metric,label_reason,count"
  awk '
    /^aether_mempool_filtered_total\{reason=/ {
      match($0, /reason="[^"]+"/); r=substr($0,RSTART+8,RLENGTH-9);
      print "aether_mempool_filtered_total," r "," $NF
    }
    /^aether_pending_decode_errors_total\{reason=/ {
      match($0, /reason="[^"]+"/); r=substr($0,RSTART+8,RLENGTH-9);
      print "aether_pending_decode_errors_total," r "," $NF
    }
    /^aether_pending_arb_sim_skipped_total\{reason=/ {
      match($0, /reason="[^"]+"/); r=substr($0,RSTART+8,RLENGTH-9);
      print "aether_pending_arb_sim_skipped_total," r "," $NF
    }
  ' "$M_END"
} > "$DROPS_CSV"

# Headline counters.
extract() {
  awk -v m="$1" '$1 ~ "^"m"(\\{|$)" { sum += $NF } END { print sum+0 }' "$M_END"
}
DEX=$(extract aether_pending_dex_tx_total)
DEC_ERR=$(extract aether_pending_decode_errors_total)
FILT=$(extract aether_mempool_filtered_total)
LAG=$(extract aether_pending_pipeline_lagged_total)
SKP=$(extract aether_pending_arb_sim_skipped_total)
CAN=$(extract aether_pending_arb_candidates_total)
BLOCKS=$(grep -c "Bellman-Ford detection complete" "$PLAIN_LOG" || true)
DECODED_SAMPLES=$(wc -l < "$DEX_CSV" | awk '{print $1-1}')

# Pull a few real tx hashes for Etherscan cross-check.
SAMPLE_HASHES=$(awk -F, 'NR>1 && NR<=4 {print $2}' "$DEX_CSV")

# WS engagement evidence.
WS_LINES=$(grep -E "transport=WebSocket|Subscriptions active|subscribing to alchemy_pendingTransactions" "$PLAIN_LOG" | head -5 || true)

# Block heights observed.
BLOCK_HEIGHTS=$(grep -oE "block_number=[0-9]+|provider connected block=[0-9]+" "$PLAIN_LOG" \
  | grep -oE "[0-9]+" | sort -u)
FIRST_BLOCK=$(echo "$BLOCK_HEIGHTS" | head -1)
LAST_BLOCK=$(echo "$BLOCK_HEIGHTS" | tail -1)
BLOCK_COUNT=$(echo "$BLOCK_HEIGHTS" | wc -l | awk '{print $1}')

SUMMARY="$OUT_DIR/07_summary.md"
{
  echo "# Mempool stage 1 — live mainnet capture"
  echo
  echo "**Date:** $(date -u +%Y-%m-%d) (UTC)"
  echo "**Duration:** ${DURATION}s"
  echo "**Branch:** \`feat/mempool-tracking-scaffold\` (PR #118)"
  echo
  echo "## Verdict"
  echo
  if [ "$DEX" -gt 0 ] && [ "$BLOCKS" -gt 0 ]; then
    echo "**PASS** — both WS subscriptions delivered live mainnet events end-to-end."
  else
    echo "**FAIL** — see counters below + log."
  fi
  echo
  echo "## WS subscriptions engaged"
  echo
  echo '```'
  echo "$WS_LINES"
  echo '```'
  echo
  echo "## Counters at +${DURATION}s"
  echo
  echo '| metric | value |'
  echo '|---|---|'
  echo "| aether_pending_dex_tx_total | $DEX |"
  echo "| aether_pending_decode_errors_total | $DEC_ERR |"
  echo "| aether_mempool_filtered_total | $FILT |"
  echo "| aether_pending_pipeline_lagged_total | $LAG |"
  echo "| aether_pending_arb_sim_skipped_total | $SKP |"
  echo "| aether_pending_arb_candidates_total | $CAN |"
  echo "| detection cycles run | $BLOCKS |"
  echo "| decoded swap samples in CSV | $DECODED_SAMPLES |"
  echo
  echo "## Block heights observed (verify on https://etherscan.io/block/<n>)"
  echo
  echo '```'
  echo "$BLOCK_HEIGHTS"
  echo '```'
  echo
  echo "first: $FIRST_BLOCK, last: $LAST_BLOCK, total unique: $BLOCK_COUNT"
  echo
  echo "## Sample pending tx hashes (verify on https://etherscan.io/tx/<hash>)"
  echo
  if [ -n "$SAMPLE_HASHES" ]; then
    echo '```'
    echo "$SAMPLE_HASHES"
    echo '```'
  else
    echo "(no decoded swaps in this window — see CSV for raw forwards)"
  fi
  echo
  echo "## Filter drop breakdown (from 06_filter_drops.csv)"
  echo
  echo '```'
  cat "$DROPS_CSV"
  echo '```'
  echo
  echo "## Files in this directory"
  echo
  echo '| file | purpose |'
  echo '|---|---|'
  echo "| 01_env.txt | redacted env vars used for capture |"
  echo "| 02_engine.log | full engine stdout/stderr (with ANSI) |"
  echo "| 02_engine.log.plain | ANSI-stripped copy for grep tooling |"
  echo "| 03_metrics_${HALF}s.txt | /metrics scrape at midpoint |"
  echo "| 04_metrics_${DURATION}s.txt | /metrics scrape at end |"
  echo "| 05_pending_dex_txs.csv | parsed decoded swaps (ts, tx_hash, router, protocol, ...) |"
  echo "| 06_filter_drops.csv | drop counts by metric + reason |"
  echo "| 07_summary.md | this file |"
} > "$SUMMARY"

echo
echo "==> capture done"
echo "==> summary: $SUMMARY"
echo
cat "$SUMMARY"
