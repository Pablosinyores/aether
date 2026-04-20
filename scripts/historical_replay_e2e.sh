#!/usr/bin/env bash
set -euo pipefail

# Aether Historical Block End-to-End Replay
#
# Runs the FULL production pipeline against a single historical block:
#   Anvil (forked at N-1) → aether-rust (live WS) → gRPC → aether-executor
#   (AETHER_SHADOW=1, skips eth_sendBundle) → metrics → Grafana dashboard.
#
# Unlike scripts/staging_test.sh (which seeds synthetic arbs against Anvil
# at the chain tip), this replays real mainnet txs of a chosen block via
# `aether-replay --full-block --anvil-attach` so the engine sees REAL state
# transitions and the executor sees REAL arbs that formed in that block.
#
# Usage:
#   ./scripts/historical_replay_e2e.sh --block 24643151
#   KEEP_RUNNING=1 ./scripts/historical_replay_e2e.sh --block 24643151
#       (leaves pipeline up after replay so you can open Grafana)
#
# Requires: anvil, cargo, go, curl, ETH_RPC_URL set in .env or environment.

# ── Args ────────────────────────────────────────────────────────────────

BLOCK=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --block) BLOCK="$2"; shift 2 ;;
        --block=*) BLOCK="${1#*=}"; shift ;;
        -h|--help)
            grep -E '^#( |$)' "$0" | sed 's/^# \?//' | head -30
            exit 0
            ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$BLOCK" ]; then
    echo "Missing --block N" >&2
    exit 2
fi

# ── Paths & Configuration ───────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_ROOT/build"
BIN_DIR="$BUILD_DIR/bin"
LOG_DIR="$BUILD_DIR/e2e-replay-logs"
REPORTS_DIR="$PROJECT_ROOT/reports"

ANVIL_PORT="${ANVIL_PORT:-8547}"
GRPC_PORT="${GRPC_PORT:-50052}"
RUST_METRICS_PORT="${RUST_METRICS_PORT:-9093}"
GO_METRICS_PORT="${GO_METRICS_PORT:-9091}"
DASHBOARD_PORT="${DASHBOARD_PORT:-8081}"
KEEP_RUNNING="${KEEP_RUNNING:-0}"
SKIP_BUILD="${SKIP_BUILD:-0}"
POLL_INTERVAL_SEC="${POLL_INTERVAL_SEC:-3}"

# Anvil default account 0 — deployer and stub searcher key.
STAGING_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcda11cb7257a0b8d2"

# ── Logging helpers ─────────────────────────────────────────────────────

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'

log_info()  { echo -e "${BLUE}[INFO]${NC} $*"; }
log_ok()    { echo -e "${GREEN}[ OK ]${NC} $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[FAIL]${NC} $*" >&2; }
log_step()  { echo -e "\n${CYAN}${BOLD}=== $* ===${NC}"; }

# ── Process tracking + cleanup ──────────────────────────────────────────

ANVIL_PID=""
RUST_PID=""
EXECUTOR_PID=""

cleanup() {
    echo ""
    if [ "$KEEP_RUNNING" = "1" ]; then
        log_warn "KEEP_RUNNING=1 — leaving Anvil + pipeline up."
        log_info "  Anvil:    http://127.0.0.1:$ANVIL_PORT  (PID $ANVIL_PID)"
        log_info "  Rust:     metrics http://127.0.0.1:$RUST_METRICS_PORT/metrics  (PID $RUST_PID)"
        log_info "  Executor: metrics http://127.0.0.1:$GO_METRICS_PORT/metrics  (PID $EXECUTOR_PID)"
        log_info "  Stop manually:  kill $ANVIL_PID $RUST_PID $EXECUTOR_PID"
        return
    fi
    log_warn "Shutting down pipeline..."
    for pid in $EXECUTOR_PID $RUST_PID $ANVIL_PID; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    wait 2>/dev/null || true
    log_info "Cleanup complete"
}
trap cleanup EXIT INT TERM

wait_for_port() {
    local port="$1" name="$2" timeout="${3:-30}"
    local elapsed=0
    while ! lsof -i ":$port" -sTCP:LISTEN >/dev/null 2>&1; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge "$timeout" ]; then
            log_error "$name did not start on port $port within ${timeout}s"
            return 1
        fi
    done
    log_ok "$name listening on port $port (${elapsed}s)"
}

kill_port() {
    local port="$1"
    local pid
    pid=$(lsof -ti ":$port" 2>/dev/null || true)
    if [ -n "$pid" ]; then
        log_warn "Killing existing process on port $port (PID $pid)"
        kill $pid 2>/dev/null || true
        sleep 1
    fi
}

scrape_metric() {
    local url="$1" metric="$2"
    curl -sf "$url" 2>/dev/null \
        | grep "^${metric} " \
        | awk '{print $2}' \
        | head -1 || echo "0"
}

# Compute a mean from a Prometheus histogram's `_sum` and `_count` series.
# The `prometheus` Rust crate exports histograms as buckets + _sum + _count
# only — no pre-aggregated quantile series. Mean is the honest summary we
# can read without client-side bucket interpolation.
scrape_hist_mean() {
    local url="$1" metric="$2"
    local body
    body=$(curl -sf "$url" 2>/dev/null) || { echo "NaN"; return; }
    local sum count
    sum=$(echo "$body" | awk -v m="${metric}_sum"   '$1==m{print $2; exit}')
    count=$(echo "$body" | awk -v m="${metric}_count" '$1==m{print $2; exit}')
    if [ -z "$sum" ] || [ -z "$count" ] || [ "$count" = "0" ]; then
        echo "NaN"
    else
        awk -v s="$sum" -v c="$count" 'BEGIN{printf "%.2f", s/c}'
    fi
}

# ── Phase 0: Preflight ──────────────────────────────────────────────────

log_step "Phase 0: Preflight"
cd "$PROJECT_ROOT"

if [ -f .env ]; then
    set -a && source .env && set +a
    log_info "Loaded .env"
fi
# Never pass .env's RUST_LOG through — the aether-rust binary's own default
# is saner for our purposes.
unset RUST_LOG

if [ -z "${ETH_RPC_URL:-}" ]; then
    log_error "ETH_RPC_URL required (archive RPC — Alchemy free tier is fine)"
    exit 1
fi

for cmd in anvil cargo go curl lsof; do
    if ! command -v "$cmd" &>/dev/null; then
        log_error "Missing tool: $cmd"
        exit 1
    fi
done
log_ok "All required tools found"

# Clear any stale ports
for port in $ANVIL_PORT $GRPC_PORT $RUST_METRICS_PORT $GO_METRICS_PORT $DASHBOARD_PORT; do
    kill_port "$port"
done

mkdir -p "$LOG_DIR" "$BIN_DIR" "$REPORTS_DIR"

# ── Phase 1: Build ──────────────────────────────────────────────────────

log_step "Phase 1: Build"
if [ "$SKIP_BUILD" = "1" ]; then
    log_warn "SKIP_BUILD=1 — skipping build step"
else
    log_info "Building Rust workspace (release)..."
    cargo build --release --workspace --bins 2>&1 | tail -3
    log_ok "Rust build complete"

    log_info "Building Go executor..."
    CGO_ENABLED=0 go build -o "$BIN_DIR/aether-executor" ./cmd/executor/
    log_ok "Go build complete"
fi

# Resolve binary paths (post-build).
AETHER_RUST="$PROJECT_ROOT/target/release/aether-rust"
AETHER_REPLAY="$PROJECT_ROOT/target/release/aether-replay"
AETHER_EXECUTOR="$BIN_DIR/aether-executor"
for bin in "$AETHER_RUST" "$AETHER_REPLAY" "$AETHER_EXECUTOR"; do
    if [ ! -x "$bin" ]; then
        log_error "Binary missing: $bin"
        exit 1
    fi
done

# ── Phase 2: Spawn Anvil at block-1 ─────────────────────────────────────

log_step "Phase 2: Spawn Anvil forked at $((BLOCK - 1))"

ANVIL_RPC="http://127.0.0.1:$ANVIL_PORT"
ANVIL_WS="ws://127.0.0.1:$ANVIL_PORT"

log_info "Forking mainnet at block $((BLOCK - 1))..."
anvil \
    --fork-url "$ETH_RPC_URL" \
    --fork-block-number $((BLOCK - 1)) \
    --port "$ANVIL_PORT" \
    --auto-impersonate \
    --chain-id 1 \
    --silent \
    > "$LOG_DIR/anvil.log" 2>&1 &
ANVIL_PID=$!

wait_for_port "$ANVIL_PORT" "Anvil" 60

FORK_BLOCK=$(curl -sf -X POST -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "$ANVIL_RPC" | python3 -c "import json,sys;print(int(json.load(sys.stdin)['result'],16))")
log_ok "Anvil forked at block $FORK_BLOCK (expected $((BLOCK - 1)))"

# ── Phase 3: Start Rust gRPC server pointed at Anvil ────────────────────

log_step "Phase 3: Start aether-rust (WS subscribed to Anvil)"

# IMPORTANT: use the HTTP endpoint — the simulator's RpcForkedState wraps
# alloy's AlloyDB which only supports HTTP transport. WS works for the block
# subscription but breaks every RPC-backed simulation. With HTTP the engine
# falls back to HTTP polling for new blocks (acceptable overhead for replay).
ETH_RPC_URL="$ANVIL_RPC" \
GRPC_ADDRESS="127.0.0.1:$GRPC_PORT" \
RUST_LOG="info" \
AETHER_POOLS_CONFIG="$PROJECT_ROOT/config/pools_historical_replay.toml" \
RUST_METRICS_PORT="$RUST_METRICS_PORT" \
    "$AETHER_RUST" > "$LOG_DIR/rust.log" 2>&1 &
RUST_PID=$!

wait_for_port "$GRPC_PORT" "Rust gRPC" 30
wait_for_port "$RUST_METRICS_PORT" "Rust metrics" 15
log_ok "aether-rust running (PID $RUST_PID)"

# ── Phase 4: Start Go executor in SHADOW mode ───────────────────────────

log_step "Phase 4: Start aether-executor (AETHER_SHADOW=1)"

BUNDLE_DUMP_DIR="$REPORTS_DIR/bundles_$BLOCK"
rm -rf "$BUNDLE_DUMP_DIR"
mkdir -p "$BUNDLE_DUMP_DIR"

ETH_RPC_URL="$ANVIL_RPC" \
GRPC_ADDRESS="127.0.0.1:$GRPC_PORT" \
SEARCHER_KEY="$STAGING_KEY" \
METRICS_PORT="$GO_METRICS_PORT" \
DASHBOARD_PORT="$DASHBOARD_PORT" \
AETHER_SHADOW="1" \
AETHER_SHADOW_DUMP_DIR="$BUNDLE_DUMP_DIR" \
    "$AETHER_EXECUTOR" > "$LOG_DIR/executor.log" 2>&1 &
EXECUTOR_PID=$!

wait_for_port "$GO_METRICS_PORT" "Executor metrics" 30
log_ok "aether-executor running in SHADOW mode (PID $EXECUTOR_PID)"

# Let the services finish their startup (nonce sync, gas oracle fetch, etc.)
sleep 3

# ── Phase 5: Replay the target block through Anvil ──────────────────────

log_step "Phase 5: Replay block $BLOCK transactions"

REPLAY_CSV="$REPORTS_DIR/e2e_replay_$BLOCK.csv"
log_info "Driving replay via aether-replay --full-block..."
log_info "  CSV: $REPLAY_CSV"

# Use the EXISTING aether-replay binary, pointed at our already-spawned Anvil
# via `--anvil-attach --anvil-port $ANVIL_PORT`. The binary skips its own
# Anvil spawn and drives tx replay through our long-lived fork, so every tx
# mined hits the aether-rust event loop attached to this same Anvil.
AETHER_REPLAY_LOG="warn" "$AETHER_REPLAY" \
    --block "$BLOCK" \
    --full-block \
    --anvil-attach \
    --anvil-port "$ANVIL_PORT" \
    --csv "$REPLAY_CSV" \
    > "$LOG_DIR/replay.log" 2>&1 &
REPLAY_PID=$!

# ── Phase 6: Live metric scraping while replay runs ─────────────────────

log_step "Phase 6: Watching pipeline metrics"

RUST_METRICS_URL="http://127.0.0.1:$RUST_METRICS_PORT/metrics"
GO_METRICS_URL="http://127.0.0.1:$GO_METRICS_PORT/metrics"

START=$(date +%s)
PEAK_CYCLES=0; PEAK_SIMS=0; PEAK_ARBS=0
PEAK_BUNDLES=0; PEAK_SHADOW=0; PEAK_RISK_REJ=0

while kill -0 $REPLAY_PID 2>/dev/null; do
    ELAPSED=$(( $(date +%s) - START ))
    C=$(scrape_metric "$RUST_METRICS_URL" "aether_cycles_detected_total" | cut -d. -f1)
    S=$(scrape_metric "$RUST_METRICS_URL" "aether_simulations_run_total" | cut -d. -f1)
    A=$(scrape_metric "$RUST_METRICS_URL" "aether_arbs_published_total" | cut -d. -f1)
    B=$(scrape_metric "$GO_METRICS_URL" "aether_executor_bundles_submitted_total" | cut -d. -f1)
    SH=$(scrape_metric "$GO_METRICS_URL" "aether_executor_shadow_bundles_total" | cut -d. -f1)
    R=$(scrape_metric "$GO_METRICS_URL" "aether_executor_risk_rejections_total" | cut -d. -f1)
    PEAK_CYCLES=$((C > PEAK_CYCLES ? C : PEAK_CYCLES))
    PEAK_SIMS=$((S > PEAK_SIMS ? S : PEAK_SIMS))
    PEAK_ARBS=$((A > PEAK_ARBS ? A : PEAK_ARBS))
    PEAK_BUNDLES=$((B > PEAK_BUNDLES ? B : PEAK_BUNDLES))
    PEAK_SHADOW=$((SH > PEAK_SHADOW ? SH : PEAK_SHADOW))
    PEAK_RISK_REJ=$((R > PEAK_RISK_REJ ? R : PEAK_RISK_REJ))
    log_info "[${ELAPSED}s] cycles=$C sims=$S arbs=$A bundles=$B shadow=$SH rejected=$R"
    sleep "$POLL_INTERVAL_SEC"
done
wait $REPLAY_PID 2>/dev/null || true

# Final scrape after replay exits.
sleep 1
C=$(scrape_metric "$RUST_METRICS_URL" "aether_cycles_detected_total" | cut -d. -f1)
S=$(scrape_metric "$RUST_METRICS_URL" "aether_simulations_run_total" | cut -d. -f1)
A=$(scrape_metric "$RUST_METRICS_URL" "aether_arbs_published_total" | cut -d. -f1)
B=$(scrape_metric "$GO_METRICS_URL" "aether_executor_bundles_submitted_total" | cut -d. -f1)
SH=$(scrape_metric "$GO_METRICS_URL" "aether_executor_shadow_bundles_total" | cut -d. -f1)
R=$(scrape_metric "$GO_METRICS_URL" "aether_executor_risk_rejections_total" | cut -d. -f1)
PEAK_CYCLES=$((C > PEAK_CYCLES ? C : PEAK_CYCLES))
PEAK_SIMS=$((S > PEAK_SIMS ? S : PEAK_SIMS))
PEAK_ARBS=$((A > PEAK_ARBS ? A : PEAK_ARBS))
PEAK_BUNDLES=$((B > PEAK_BUNDLES ? B : PEAK_BUNDLES))
PEAK_SHADOW=$((SH > PEAK_SHADOW ? SH : PEAK_SHADOW))
PEAK_RISK_REJ=$((R > PEAK_RISK_REJ ? R : PEAK_RISK_REJ))

DETECT_MEAN=$(scrape_hist_mean "$RUST_METRICS_URL" "aether_detection_latency_ms")
SIM_MEAN=$(scrape_hist_mean "$RUST_METRICS_URL" "aether_simulation_latency_ms")
E2E_MEAN=$(scrape_hist_mean "$GO_METRICS_URL" "aether_end_to_end_latency_ms")

# ── Phase 7: Final report ───────────────────────────────────────────────

log_step "Phase 7: Replay Summary"

DURATION=$(( $(date +%s) - START ))
echo ""
echo -e "${BOLD}============================================${NC}"
echo -e "${BOLD}  HISTORICAL E2E REPLAY — BLOCK $BLOCK  ${NC}"
echo -e "${BOLD}============================================${NC}"
echo ""
printf "  %-36s %s\n" "Duration:"                      "${DURATION}s"
printf "  %-36s %s\n" "Fork block:"                    "$FORK_BLOCK"
echo ""
echo -e "${BOLD}  Detection (Rust engine)${NC}"
printf "  %-36s %s\n" "Cycles detected:"               "$PEAK_CYCLES"
printf "  %-36s %s\n" "Simulations run:"               "$PEAK_SIMS"
printf "  %-36s %s\n" "Arbs published → executor:"     "$PEAK_ARBS"
printf "  %-36s %s ms\n" "Detection latency (mean):"   "$DETECT_MEAN"
printf "  %-36s %s ms\n" "Simulation latency (mean):"  "$SIM_MEAN"
echo ""
echo -e "${BOLD}  Execution (Go executor, SHADOW mode)${NC}"
printf "  %-36s %s\n" "Bundles built (would-submit):"  "$PEAK_SHADOW"
printf "  %-36s %s\n" "Bundles actually submitted:"    "$PEAK_BUNDLES"
printf "  %-36s %s\n" "Risk preflight rejections:"     "$PEAK_RISK_REJ"
printf "  %-36s %s ms\n" "End-to-end latency (mean):" "$E2E_MEAN"
echo ""
echo -e "${BOLD}  Artifacts${NC}"
printf "  %-36s %s\n" "Per-event CSV:" "$REPLAY_CSV"
printf "  %-36s %s\n" "Logs directory:" "$LOG_DIR"
if [ "$KEEP_RUNNING" = "1" ]; then
    printf "  %-36s %s\n" "Rust metrics:" "$RUST_METRICS_URL"
    printf "  %-36s %s\n" "Executor metrics:" "$GO_METRICS_URL"
    printf "  %-36s %s\n" "Dashboard:" "http://127.0.0.1:$DASHBOARD_PORT"
fi
echo -e "${BOLD}============================================${NC}"

if [ "$PEAK_BUNDLES" -gt 0 ]; then
    log_error "Bundles were ACTUALLY submitted — shadow mode failed!"
    exit 1
fi

# ── Phase 8: Ground-truth vs pipeline comparison ────────────────────────

log_step "Phase 8: Arb Catch Report"

python3 - "$REPLAY_CSV" "$BUNDLE_DUMP_DIR" <<'PY' || log_warn "comparison failed (non-fatal)"
import csv
import json
import os
import sys
from collections import defaultdict
from glob import glob

replay_csv, bundles_dir = sys.argv[1], sys.argv[2]

replay_rows = []
if os.path.exists(replay_csv):
    with open(replay_csv, newline="") as f:
        for row in csv.DictReader(f):
            # Normalise path string for matching; aether-replay uses
            # "WETH -> AAVE -> WETH" with spaces, Go mirrors this.
            row["path"] = row.get("path", "").strip()
            replay_rows.append(row)

bundles = []
for p in sorted(glob(os.path.join(bundles_dir, "*.json"))):
    try:
        with open(p) as f:
            bundles.append(json.load(f))
    except Exception as e:
        print(f"  (skipping {p}: {e})")

def path_str(b):
    return " -> ".join(b.get("path", []))

# ── Per-path hit rate ────────────────────────────────────────────────
replay_paths = defaultdict(int)
for r in replay_rows:
    replay_paths[r["path"]] += 1

bundle_paths = defaultdict(int)
for b in bundles:
    bundle_paths[path_str(b)] += 1

print()
print("  Replay (ground truth) detection events:  {}".format(len(replay_rows)))
print("  Pipeline shadow bundles built:            {}".format(len(bundles)))
print()

# ── Hit rate per path ────────────────────────────────────────────────
all_paths = sorted(set(replay_paths) | set(bundle_paths))
if all_paths:
    print("  By path (replay / pipeline):")
    for p in all_paths:
        r = replay_paths.get(p, 0)
        b = bundle_paths.get(p, 0)
        hr = (100.0 * b / r) if r else 0.0
        print(f"    {p:<40} {r:>4} / {b:>4}    ({hr:5.1f}%)")

# ── Top bundles by profit ────────────────────────────────────────────
if bundles:
    top = sorted(bundles, key=lambda b: b.get("net_profit_eth", 0.0), reverse=True)[:5]
    print()
    print("  Top pipeline bundles by net_profit_eth:")
    for b in top:
        print(
            "    id={id:<32} path={path:<28} profit={p:>14.6f} ETH  gas={gas}".format(
                id=b.get("arb_id", "?"),
                path=path_str(b),
                p=b.get("net_profit_eth", 0.0),
                gas=b.get("total_gas", 0),
            )
        )

# ── Biggest arb in replay that didn't make it to a bundle ────────────
if replay_rows and bundles:
    bundle_path_set = set(bundle_paths)
    missed = [r for r in replay_rows if r["path"] not in bundle_path_set]
    if missed:
        def profit(r):
            try:
                return float(r.get("sim_net_profit_eth") or r.get("sim_gross_profit_eth") or 0)
            except ValueError:
                return 0.0
        biggest = max(missed, key=profit)
        print()
        print("  Largest replay-detected arb with NO matching pipeline bundle:")
        print(f"    path:     {biggest.get('path')}")
        print(f"    tx_index: {biggest.get('tx_index')}")
        print(f"    profit:   {profit(biggest):.6f} ETH (replay estimate)")
    else:
        print()
        print("  Every replay-detected path produced at least one pipeline bundle ✓")

print()
print("  Per-bundle JSONs: {}/".format(bundles_dir))
print("  Inspect any bundle:  jq . {}/<arb_id>.json".format(bundles_dir))
PY

log_ok "E2E replay completed successfully (shadow mode held; no real submissions)"
