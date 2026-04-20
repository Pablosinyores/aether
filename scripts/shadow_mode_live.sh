#!/usr/bin/env bash
set -euo pipefail

# Aether Shadow Mode — live mainnet, no submissions
#
# Runs the FULL production pipeline against live Ethereum mainnet:
#   Alchemy WS → aether-rust (live subscription) → gRPC → aether-executor
#   (AETHER_SHADOW=1 — bundles built + signed but never sent to Flashbots)
#
# This is the production-readiness gate. It exercises the exact code path
# the bot would run in prod, with one tape holding it off the network: the
# AETHER_SHADOW flag. Every arb it thinks about is logged to
# `reports/shadow_<timestamp>/bundles/<arb_id>.json` for post-run audit.
#
# Usage:
#   ./scripts/shadow_mode_live.sh                    # 1-hour default
#   ./scripts/shadow_mode_live.sh --duration 30m
#   ./scripts/shadow_mode_live.sh --duration 4h
#
# Requires: cargo, go, curl, ETH_RPC_URL (HTTP), ETH_WS_URL (wss://).
# ETH_WS_URL is optional — if unset we derive it from ETH_RPC_URL by
# swapping https:// → wss://.

# ── Args ────────────────────────────────────────────────────────────────

DURATION="60m"
LABEL=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --duration) DURATION="$2"; shift 2 ;;
        --duration=*) DURATION="${1#*=}"; shift ;;
        --label) LABEL="$2"; shift 2 ;;
        --label=*) LABEL="${1#*=}"; shift ;;
        -h|--help)
            grep -E '^#( |$)' "$0" | sed 's/^# \?//' | head -30
            exit 0
            ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
done

# ── Parse duration (e.g. "30m", "4h", "90s") into seconds ───────────────
parse_duration() {
    local d="$1"
    case "$d" in
        *s) echo "${d%s}" ;;
        *m) echo "$((${d%m} * 60))" ;;
        *h) echo "$((${d%h} * 3600))" ;;
        *)  echo "$d" ;;
    esac
}
DURATION_SEC=$(parse_duration "$DURATION")
if ! [[ "$DURATION_SEC" =~ ^[0-9]+$ ]] || [ "$DURATION_SEC" -lt 10 ]; then
    echo "Invalid --duration '$DURATION' (use e.g. 30m, 4h, 3600s)" >&2
    exit 2
fi

# ── Paths + ports ───────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_ROOT/build"
BIN_DIR="$BUILD_DIR/bin"

TS="$(date +%Y%m%d_%H%M%S)${LABEL:+_$LABEL}"
RUN_DIR="$PROJECT_ROOT/reports/shadow_$TS"
LOG_DIR="$RUN_DIR/logs"
BUNDLE_DIR="$RUN_DIR/bundles"
SNAPSHOT_FILE="$RUN_DIR/metrics_snapshot.txt"
mkdir -p "$LOG_DIR" "$BUNDLE_DIR" "$BIN_DIR"

GRPC_PORT="${GRPC_PORT:-50061}"
RUST_METRICS_PORT="${RUST_METRICS_PORT:-9094}"
GO_METRICS_PORT="${GO_METRICS_PORT:-9095}"
DASHBOARD_PORT="${DASHBOARD_PORT:-8080}"
DISABLE_DASHBOARD="${DISABLE_DASHBOARD:-0}"
SKIP_BUILD="${SKIP_BUILD:-0}"

# Anvil default account 0 — searcher stub key (never used for real submission
# because AETHER_SHADOW=1 holds us off the wire).
STAGING_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcda11cb7257a0b8d2"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
log_info()  { echo -e "${BLUE}[INFO]${NC} $*"; }
log_ok()    { echo -e "${GREEN}[ OK ]${NC} $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[FAIL]${NC} $*" >&2; }
log_step()  { echo -e "\n${CYAN}${BOLD}=== $* ===${NC}"; }

# ── Cleanup handling ────────────────────────────────────────────────────

RUST_PID=""
EXECUTOR_PID=""
MONITOR_PID=""
CAFFEINATE_PID=""
cleanup() {
    echo ""
    log_warn "Shutting down shadow pipeline..."
    for pid in $MONITOR_PID $EXECUTOR_PID $RUST_PID $CAFFEINATE_PID; do
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
    local pid; pid=$(lsof -ti ":$port" 2>/dev/null || true)
    if [ -n "$pid" ]; then
        log_warn "Killing stale process on port $port (PID $pid)"
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

scrape_hist_mean() {
    local url="$1" metric="$2"
    local body; body=$(curl -sf "$url" 2>/dev/null) || { echo "NaN"; return; }
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

log_step "Phase 0: Preflight (shadow mode, live mainnet)"
cd "$PROJECT_ROOT"

if [ -f .env ]; then
    set -a && source .env && set +a
    log_info "Loaded .env"
fi
unset RUST_LOG

if [ -z "${ETH_RPC_URL:-}" ]; then
    log_error "ETH_RPC_URL not set — need real Alchemy mainnet endpoint"
    exit 1
fi

# Derive WS URL if not explicitly provided.
if [ -z "${ETH_WS_URL:-}" ]; then
    ETH_WS_URL="${ETH_RPC_URL/https:\/\//wss://}"
    ETH_WS_URL="${ETH_WS_URL/http:\/\//ws://}"
    log_info "ETH_WS_URL derived: ${ETH_WS_URL:0:40}..."
fi

# Hard safety check: refuse to run if the RPC URL points at localhost
# (e.g. Anvil). Shadow mode is for LIVE mainnet only.
case "$ETH_WS_URL" in
    *127.0.0.1*|*localhost*)
        log_error "ETH_WS_URL points at localhost — shadow mode is for LIVE mainnet"
        log_error "  got: $ETH_WS_URL"
        exit 1
        ;;
esac

for cmd in cargo go curl lsof; do
    if ! command -v "$cmd" &>/dev/null; then
        log_error "Missing tool: $cmd"; exit 1
    fi
done

PORTS_TO_CLEAR="$GRPC_PORT $RUST_METRICS_PORT $GO_METRICS_PORT"
if [ "$DISABLE_DASHBOARD" != "1" ]; then
    PORTS_TO_CLEAR="$PORTS_TO_CLEAR $DASHBOARD_PORT"
fi
for port in $PORTS_TO_CLEAR; do
    kill_port "$port"
done

log_info "Run directory:      $RUN_DIR"
log_info "Duration:           $DURATION ($DURATION_SEC seconds)"
log_info "Live WS target:     ${ETH_WS_URL:0:60}..."
log_info "Live HTTP target:   ${ETH_RPC_URL:0:60}..."

# Prevent macOS idle sleep during the run so the poll loop + processes
# don't freeze mid-run. caffeinate auto-exits after the timeout.
if command -v caffeinate &>/dev/null; then
    caffeinate -i -t "$((DURATION_SEC + 120))" &
    CAFFEINATE_PID=$!
    log_info "caffeinate active (PID $CAFFEINATE_PID) — idle sleep blocked"
fi

# ── Phase 1: Build ──────────────────────────────────────────────────────

log_step "Phase 1: Build"
if [ "$SKIP_BUILD" = "1" ]; then
    log_warn "SKIP_BUILD=1 — skipping build step"
else
    log_info "cargo build --release --workspace --bins..."
    cargo build --release --workspace --bins 2>&1 | tail -3
    log_ok "Rust build complete"

    log_info "go build ./cmd/executor/..."
    CGO_ENABLED=0 go build -o "$BIN_DIR/aether-executor" ./cmd/executor/
    log_ok "Go build complete"

    if [ "$DISABLE_DASHBOARD" != "1" ]; then
        log_info "go build ./cmd/monitor/..."
        CGO_ENABLED=0 go build -o "$BIN_DIR/aether-monitor" ./cmd/monitor/
        log_ok "Dashboard build complete"
    fi
fi

AETHER_RUST="$PROJECT_ROOT/target/release/aether-rust"
AETHER_EXECUTOR="$BIN_DIR/aether-executor"
AETHER_MONITOR="$BIN_DIR/aether-monitor"
for bin in "$AETHER_RUST" "$AETHER_EXECUTOR"; do
    if [ ! -x "$bin" ]; then
        log_error "Binary missing: $bin"; exit 1
    fi
done
if [ "$DISABLE_DASHBOARD" != "1" ] && [ ! -x "$AETHER_MONITOR" ]; then
    log_error "Dashboard binary missing: $AETHER_MONITOR"; exit 1
fi

POOLS_CONFIG="$PROJECT_ROOT/config/pools_shadow.toml"
if [ ! -f "$POOLS_CONFIG" ]; then
    log_error "Pool config missing: $POOLS_CONFIG"
    log_error "Shadow mode needs a multi-pool graph; aborting before the"
    log_error "engine falls back to an empty registry + degenerate run."
    exit 1
fi
log_info "Pool config: $POOLS_CONFIG ($(grep -c '^\[\[pools\]\]' "$POOLS_CONFIG") pools)"

# ── Phase 2: Start aether-rust against LIVE mainnet WS ──────────────────

log_step "Phase 2: Start aether-rust (live mainnet subscription)"

ETH_RPC_URL="$ETH_RPC_URL" \
ETH_WS_URL="$ETH_WS_URL" \
GRPC_ADDRESS="127.0.0.1:$GRPC_PORT" \
RUST_LOG="info" \
AETHER_POOLS_CONFIG="$POOLS_CONFIG" \
RUST_METRICS_PORT="$RUST_METRICS_PORT" \
    "$AETHER_RUST" > "$LOG_DIR/rust.log" 2>&1 &
RUST_PID=$!

wait_for_port "$GRPC_PORT" "Rust gRPC" 30
wait_for_port "$RUST_METRICS_PORT" "Rust metrics" 15
log_ok "aether-rust subscribed to live mainnet (PID $RUST_PID)"

# ── Phase 3: Start aether-executor in shadow mode ───────────────────────

log_step "Phase 3: Start aether-executor (AETHER_SHADOW=1)"

ETH_RPC_URL="$ETH_RPC_URL" \
GRPC_ADDRESS="127.0.0.1:$GRPC_PORT" \
SEARCHER_KEY="$STAGING_KEY" \
METRICS_PORT="$GO_METRICS_PORT" \
AETHER_SHADOW="1" \
AETHER_SHADOW_DUMP_DIR="$BUNDLE_DIR" \
    "$AETHER_EXECUTOR" > "$LOG_DIR/executor.log" 2>&1 &
EXECUTOR_PID=$!

wait_for_port "$GO_METRICS_PORT" "Executor metrics" 30
log_ok "aether-executor running in SHADOW mode (PID $EXECUTOR_PID)"

# ── Phase 3.5: Start monitor dashboard ──────────────────────────────────

if [ "$DISABLE_DASHBOARD" != "1" ]; then
    log_step "Phase 3.5: Start monitor dashboard"

    RUST_METRICS_PORT="$RUST_METRICS_PORT" \
    GO_METRICS_PORT="$GO_METRICS_PORT" \
    DASHBOARD_PORT="$DASHBOARD_PORT" \
        "$AETHER_MONITOR" > "$LOG_DIR/monitor.log" 2>&1 &
    MONITOR_PID=$!

    wait_for_port "$DASHBOARD_PORT" "Dashboard" 10
    log_ok "Dashboard up at http://localhost:$DASHBOARD_PORT/ (PID $MONITOR_PID)"
else
    log_warn "DISABLE_DASHBOARD=1 — skipping dashboard"
fi

sleep 3

# ── Phase 4: Run for the duration, polling metrics ──────────────────────

log_step "Phase 4: Shadow run — $DURATION"
log_info "Polling metrics every 30s. Ctrl-C to stop early."

RUST_METRICS_URL="http://127.0.0.1:$RUST_METRICS_PORT/metrics"
GO_METRICS_URL="http://127.0.0.1:$GO_METRICS_PORT/metrics"

START=$(date +%s)
END_AT=$((START + DURATION_SEC))
ITERS=0

while [ "$(date +%s)" -lt "$END_AT" ]; do
    # Check processes are alive.
    for name_pid in "Rust:$RUST_PID" "Executor:$EXECUTOR_PID"; do
        local_pid="${name_pid##*:}"
        local_name="${name_pid%%:*}"
        if ! kill -0 "$local_pid" 2>/dev/null; then
            log_error "$local_name (PID $local_pid) crashed!"
            [ -f "$LOG_DIR/${local_name,,}.log" ] && tail -10 "$LOG_DIR/${local_name,,}.log" >&2
            exit 1
        fi
    done

    ELAPSED=$(( $(date +%s) - START ))
    REMAINING=$(( END_AT - $(date +%s) ))

    BLOCKS=$(scrape_metric "$RUST_METRICS_URL" "aether_blocks_processed_total" | cut -d. -f1)
    CYCLES=$(scrape_metric "$RUST_METRICS_URL" "aether_cycles_detected_total" | cut -d. -f1)
    SIMS=$(scrape_metric "$RUST_METRICS_URL" "aether_simulations_run_total" | cut -d. -f1)
    ARBS=$(scrape_metric "$RUST_METRICS_URL" "aether_arbs_published_total" | cut -d. -f1)
    BUNDLES=$(scrape_metric "$GO_METRICS_URL" "aether_executor_bundles_submitted_total" | cut -d. -f1)
    SHADOW=$(scrape_metric "$GO_METRICS_URL" "aether_executor_shadow_bundles_total" | cut -d. -f1)
    REJ=$(scrape_metric "$GO_METRICS_URL" "aether_executor_risk_rejections_total" | cut -d. -f1)

    log_info "[${ELAPSED}s/${DURATION_SEC}s, ${REMAINING}s left] blocks=$BLOCKS cycles=$CYCLES sims=$SIMS arbs=$ARBS shadow=$SHADOW submitted=$BUNDLES rejected=$REJ"

    ITERS=$((ITERS + 1))
    # Sleep 30s or until end, whichever is shorter.
    NEXT_SLEEP=$(( REMAINING < 30 ? REMAINING : 30 ))
    if [ "$NEXT_SLEEP" -le 0 ]; then break; fi
    sleep "$NEXT_SLEEP"
done

# ── Phase 5: Final metric snapshot ──────────────────────────────────────

log_step "Phase 5: Final metric snapshot"

{
    echo "# Shadow run snapshot"
    echo "# timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# duration: $DURATION ($DURATION_SEC s)"
    echo ""
    echo "## Rust engine ($RUST_METRICS_URL)"
    curl -sf "$RUST_METRICS_URL" 2>/dev/null || echo "(scrape failed)"
    echo ""
    echo "## Go executor ($GO_METRICS_URL)"
    curl -sf "$GO_METRICS_URL" 2>/dev/null || echo "(scrape failed)"
} > "$SNAPSHOT_FILE"
log_ok "Snapshot written to $SNAPSHOT_FILE"

BLOCKS=$(scrape_metric "$RUST_METRICS_URL" "aether_blocks_processed_total" | cut -d. -f1)
CYCLES=$(scrape_metric "$RUST_METRICS_URL" "aether_cycles_detected_total" | cut -d. -f1)
SIMS=$(scrape_metric "$RUST_METRICS_URL" "aether_simulations_run_total" | cut -d. -f1)
ARBS=$(scrape_metric "$RUST_METRICS_URL" "aether_arbs_published_total" | cut -d. -f1)
BUNDLES=$(scrape_metric "$GO_METRICS_URL" "aether_executor_bundles_submitted_total" | cut -d. -f1)
SHADOW=$(scrape_metric "$GO_METRICS_URL" "aether_executor_shadow_bundles_total" | cut -d. -f1)
REJ=$(scrape_metric "$GO_METRICS_URL" "aether_executor_risk_rejections_total" | cut -d. -f1)
DETECT_MEAN=$(scrape_hist_mean "$RUST_METRICS_URL" "aether_detection_latency_ms")
SIM_MEAN=$(scrape_hist_mean "$RUST_METRICS_URL" "aether_simulation_latency_ms")
E2E_MEAN=$(scrape_hist_mean "$GO_METRICS_URL" "aether_end_to_end_latency_ms")
BUNDLE_JSON_COUNT=$(ls "$BUNDLE_DIR" 2>/dev/null | wc -l | tr -d ' ')

TOTAL_ELAPSED=$(( $(date +%s) - START ))

echo ""
echo -e "${BOLD}============================================${NC}"
echo -e "${BOLD}       SHADOW RUN — LIVE MAINNET            ${NC}"
echo -e "${BOLD}============================================${NC}"
printf "  %-32s %s\n"  "Duration:"                 "${TOTAL_ELAPSED}s (requested ${DURATION_SEC}s)"
printf "  %-32s %s\n"  "Run dir:"                  "$RUN_DIR"
if [ "$DISABLE_DASHBOARD" != "1" ]; then
    printf "  %-32s %s\n"  "Dashboard:"            "http://localhost:$DASHBOARD_PORT/  (still up — Ctrl-C to stop)"
fi
echo ""
echo -e "${BOLD}  Rust engine (live mainnet)${NC}"
printf "  %-32s %s\n"  "Blocks processed:"         "$BLOCKS"
printf "  %-32s %s\n"  "Cycles detected:"          "$CYCLES"
printf "  %-32s %s\n"  "Simulations run:"          "$SIMS"
printf "  %-32s %s\n"  "Arbs published → executor:" "$ARBS"
printf "  %-32s %s ms\n" "Detection latency (mean):"  "$DETECT_MEAN"
printf "  %-32s %s ms\n" "Simulation latency (mean):" "$SIM_MEAN"
echo ""
echo -e "${BOLD}  Executor (shadow mode)${NC}"
printf "  %-32s %s\n"  "Shadow bundles built:"     "$SHADOW"
printf "  %-32s %s\n"  "Bundle JSON files:"        "$BUNDLE_JSON_COUNT"
printf "  %-32s %s\n"  "Actually submitted:"       "$BUNDLES"
printf "  %-32s %s\n"  "Risk preflight rejections:" "$REJ"
printf "  %-32s %s ms\n" "End-to-end latency (mean):"  "$E2E_MEAN"
echo ""
echo -e "${BOLD}  Artefacts${NC}"
printf "  %-32s %s\n"  "Bundles:"                  "$BUNDLE_DIR/"
printf "  %-32s %s\n"  "Rust log:"                 "$LOG_DIR/rust.log"
printf "  %-32s %s\n"  "Executor log:"             "$LOG_DIR/executor.log"
printf "  %-32s %s\n"  "Metric snapshot:"          "$SNAPSHOT_FILE"
echo -e "${BOLD}============================================${NC}"

if [ "$BUNDLES" -gt 0 ]; then
    log_error "Bundles were ACTUALLY submitted — shadow mode failed!"
    exit 1
fi

log_ok "Shadow run complete. Inspect: jq . $BUNDLE_DIR/*.json | head"
