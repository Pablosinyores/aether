#!/usr/bin/env bash
# demo.sh — 1-day end-to-end Aether shadow-mode demo.
#
# Boots the full pipeline against live mainnet mempool data:
#
#   Alchemy WS → ingestion → decode → BF cycle scan → revm flashloan sim
#     → gRPC → Go executor → bundle build → Postgres (is_shadow=true)
#
# All recording layers fill: arbs, bundles, pnl_daily, mempool_predictions,
# mempool_reconciliation. Prometheus scrapes every binary; Grafana panels
# render the dashboards pre-provisioned under deploy/docker/grafana.
#
# No on-chain submission. No real ETH spent. Designed to run continuously
# for 24h+; binaries auto-restart on crash, logs size-capped.
#
# Usage:
#   ./demo.sh              # boot + tail forever, Ctrl-C to stop
#   ./demo.sh --fresh      # truncate demo tables before start
#   ./demo.sh --no-grafana # skip auto-open of Grafana in browser
#
# Run profile (env):
#   AETHER_ARM=B (default) full pipeline incl. Go executor + revm backrun
#                          validator.
#   AETHER_ARM=A           analytical/observability only — predictions +
#                          reconciler + profit scorer, NO executor and NO
#                          backrun validator (the Arm-A shadow-run config).
#
# Prereqs (script will fail-fast with a clear error if missing):
#   - .env populated (see DEMO_REQUIRED_ENV below)
#   - docker / docker compose installed
#   - psql client
#   - cargo + go + forge toolchains (for the build-if-missing path)
#   - Postgres reachable at $DATABASE_URL

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────
# Layout + globals
# ─────────────────────────────────────────────────────────────────────────

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log()   { echo -e "${GREEN}[demo]${NC} $*"; }
warn()  { echo -e "${YELLOW}[demo]${NC} $*"; }
err()   { echo -e "${RED}[demo]${NC} $*" >&2; }
step()  { echo -e "${BLUE}[demo $1/$TOTAL_STEPS]${NC} $2"; }

TOTAL_STEPS=10
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_DIR="$ROOT/reports/demo_$TS"
LOG_DIR="$RUN_DIR/logs"
mkdir -p "$LOG_DIR"

# Per-process PID + log file tracking for cleanup. Two parallel arrays
# instead of associative arrays so the script works on macOS bash 3.2.
PID_NAMES=()
PID_VALUES=()
CHILD_LOOPS=()

# ─────────────────────────────────────────────────────────────────────────
# CLI flag parsing
# ─────────────────────────────────────────────────────────────────────────

FRESH=0
OPEN_GRAFANA=1
for arg in "$@"; do
  case "$arg" in
    --fresh) FRESH=1 ;;
    --no-grafana) OPEN_GRAFANA=0 ;;
    -h|--help)
      sed -n '2,28p' "$0"
      exit 0
      ;;
    *) warn "Unknown flag: $arg (ignored)" ;;
  esac
done

# ─────────────────────────────────────────────────────────────────────────
# Cleanup trap — graceful shutdown of every child + final summary
# ─────────────────────────────────────────────────────────────────────────

cleanup() {
  echo ""
  warn "Shutting down. Run dir: $RUN_DIR"

  # Kill restart-loop PIDs first so they don't respawn their children.
  if [ ${#CHILD_LOOPS[@]:-0} -gt 0 ]; then
    for loop_pid in "${CHILD_LOOPS[@]}"; do
      if [ -n "$loop_pid" ] && kill -0 "$loop_pid" 2>/dev/null; then
        kill "$loop_pid" 2>/dev/null || true
      fi
    done
  fi

  # Then kill the binaries themselves.
  if [ ${#PID_NAMES[@]:-0} -gt 0 ]; then
    local i=0
    while [ $i -lt ${#PID_NAMES[@]} ]; do
      local name="${PID_NAMES[$i]}"
      local pid="${PID_VALUES[$i]}"
      if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        log "Stopping $name (PID $pid)"
        kill "$pid" 2>/dev/null || true
      fi
      i=$((i + 1))
    done
  fi
  wait 2>/dev/null || true

  # Final summary query — what did we record?
  if [ -n "${DATABASE_URL:-}" ]; then
    log "Final bundles snapshot (last 10 shadow rows):"
    PSQL "$DATABASE_URL" -c "
      SELECT b.bundle_id, b.target_block, b.gas_used, b.submitted_at, b.is_shadow,
             a.net_profit_wei
      FROM bundles b
      LEFT JOIN arbs a ON a.arb_id = b.arb_id
      WHERE b.is_shadow = true
      ORDER BY b.submitted_at DESC
      LIMIT 10;
    " || true
    log "Counts since demo start:"
    PSQL "$DATABASE_URL" -c "
      SELECT
        (SELECT count(*) FROM bundles WHERE is_shadow = true AND submitted_at >= '$TS'::timestamptz) AS shadow_bundles,
        (SELECT count(*) FROM mempool_predictions WHERE decoded_at >= '$TS'::timestamptz) AS predictions,
        (SELECT count(*) FROM mempool_reconciliation WHERE resolution_ts >= '$TS'::timestamptz) AS reconciliations,
        (SELECT count(*) FROM arbs WHERE ts >= '$TS'::timestamptz) AS arbs_detected;
    " || true
  fi

  log "Log archive at: $RUN_DIR"
  log "Done."
  exit 0
}
trap cleanup SIGINT SIGTERM EXIT

# ─────────────────────────────────────────────────────────────────────────
# Step 1 — preflight
# ─────────────────────────────────────────────────────────────────────────

step 1 "Preflight checks"

if [ -f .env ]; then
  # shellcheck disable=SC1091
  set -a && source .env && set +a
fi

# Only ALCHEMY_API_KEY is user-must-provide. Everything else has a sane
# default for the local-compose demo setup.
if [ -z "${ALCHEMY_API_KEY:-}" ]; then
  err "ALCHEMY_API_KEY is required. Set it in .env:"
  err "  echo 'ALCHEMY_API_KEY=your_key_here' >> .env"
  exit 1
fi

# ETH_RPC_URL — defaults to the Alchemy mainnet HTTPS endpoint built from
# the API key. The Rust mempool subscription falls back to ETH_RPC_URL
# when MEMPOOL_WS_URL is unset and transparently upgrades to WSS, so a
# single var covers both transports.
export ETH_RPC_URL="${ETH_RPC_URL:-https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_API_KEY}}"

# DATABASE_URL — points at the Postgres container the compose file spins
# up. Compose exposes 5432; if a host-side Postgres collides, stop it or
# override DATABASE_URL in .env.
export DATABASE_URL="${DATABASE_URL:-postgres://aether:aether@127.0.0.1:5432/aether}"

# MEMPOOL_LEDGER_DSN — separate logical DSN for the mempool writer; we
# reuse the same physical Postgres for demo simplicity (production
# separates them).
export MEMPOOL_LEDGER_DSN="${MEMPOOL_LEDGER_DSN:-$DATABASE_URL}"

# AETHER_ARM selects the run profile (default B preserves prior behaviour).
AETHER_ARM="${AETHER_ARM:-B}"

if [ "$AETHER_ARM" = "A" ]; then
  # Arm A: leave AETHER_EXECUTOR_ADDRESS UNSET so the engine logs
  # "validator disabled" (main.rs gates the backrun validator on this var)
  # and never burns RPC on per-candidate forks. unset defends against a
  # stray value inherited from .env.
  unset AETHER_EXECUTOR_ADDRESS
  log "AETHER_ARM=A — analytical/observability run (no executor, backrun validator disabled)"
else
  # Arm B — AETHER_EXECUTOR_ADDRESS must point at an address with on-chain
  # bytecode because cmd/executor/main.go runs eth_getCode at startup and
  # aborts on an empty account. For the demo we use UniV3 SwapRouter02 as a
  # placeholder — it has bytecode, so the check passes; revm's per-sim
  # bytecode injection (#165) overrides it inside the actual sim. Shadow mode
  # never broadcasts, so no risk of sending a real tx to the wrong contract.
  export AETHER_EXECUTOR_ADDRESS="${AETHER_EXECUTOR_ADDRESS:-0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45}"
fi

# Hot-token discovery (Part 1) — opt-in via AETHER_DISCOVERY=1. The engine
# gates the loop on this var (main.rs); it runs off ETH_RPC_URL with its own
# longer FoT-screen timeout, independent of the latency hot path. Knob defaults
# (60s cadence, 500-pool cap, 8s screen timeout) already match the run plan.
#
# Intended for the *canary* phase only: discovery APPENDS admitted pools to
# config/pools.toml, which moves the price graph. Per the run methodology,
# discovery and measurement must be SEQUENTIAL — run discovery during the
# canary, then FREEZE pools.toml (unset AETHER_DISCOVERY) for the pure-
# measurement days, else Arm-A profitability inflates as a function of
# admissions rather than real opportunities.
if [ "${AETHER_DISCOVERY:-0}" = "1" ]; then
  export AETHER_DISCOVERY=1
  # Static ETH-price proxy for the per-venue WETH-liquidity estimate.
  export AETHER_ETH_PRICE_USD="${AETHER_ETH_PRICE_USD:-3000}"
  warn "AETHER_DISCOVERY=1 — hot-token discovery ON (canary mode)."
  warn "  -> appends to config/pools.toml; FREEZE it (unset AETHER_DISCOVERY) for"
  warn "     the pure-measurement days or Arm-A profit inflates with admissions."
else
  # Defend against a stray AETHER_DISCOVERY inherited from .env during a
  # frozen measurement run.
  unset AETHER_DISCOVERY
fi

# Bytecode artifact path — produced by `forge build` in step 4.
export AETHER_EXECUTOR_BYTECODE_PATH="${AETHER_EXECUTOR_BYTECODE_PATH:-contracts/out/AetherExecutor.sol/AetherExecutor.json}"

# SEARCHER_KEY — Anvil's well-known test account-0 private key. Shadow
# mode never broadcasts, so this is never used to sign anything that
# leaves the process. Public + intentional.
export SEARCHER_KEY="${SEARCHER_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"

# Force shadow mode + mempool tracking regardless of .env. Demo never submits.
export AETHER_SHADOW=1
export MEMPOOL_TRACKING=1
export AETHER_MEMPOOL_SIM_CONCURRENCY="${AETHER_MEMPOOL_SIM_CONCURRENCY:-16}"
export RECONCILER_METRICS_ADDR="${RECONCILER_METRICS_ADDR:-:9094}"

for cmd in docker cargo go forge jq lsof curl; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    err "Missing required tool: $cmd"
    exit 1
  fi
done

# psql wrapper — uses host psql if installed, else `docker compose exec`
# into the Postgres container so the demo has no extra host dependency.
PSQL_VIA_DOCKER=0
if command -v psql >/dev/null 2>&1; then
  PSQL() { psql "$@"; }
else
  PSQL_VIA_DOCKER=1
  PSQL() {
    # Strip the DSN arg; container psql connects to local socket.
    local args=()
    for a in "$@"; do
      case "$a" in
        postgres://*|postgresql://*) ;;  # drop
        *) args+=("$a") ;;
      esac
    done
    docker compose -f deploy/docker/docker-compose.yml exec -T postgres \
      psql -U aether -d aether "${args[@]}"
  }
fi

# Free ports we'll be binding.
for port in 9090 9092 9094 50051 8080 3000; do
  pid=$(lsof -ti :"$port" 2>/dev/null || true)
  if [ -n "$pid" ]; then
    warn "Killing existing process on port $port (PID $pid)"
    kill "$pid" 2>/dev/null || true
    sleep 1
  fi
done

log "Preflight OK"

# ─────────────────────────────────────────────────────────────────────────
# Step 2 — observability stack
# ─────────────────────────────────────────────────────────────────────────

step 2 "Booting observability stack (Postgres + Prometheus + Grafana + Loki + Promtail)"
# Alertmanager intentionally omitted from the demo profile: it requires
# SLACK_WEBHOOK_URL at startup and the shadow run has nothing to alert
# on (no bundles submitted → no inclusion misses → no PnL halts).
# Re-add it when running against live submission.
docker compose -f deploy/docker/docker-compose.yml up -d \
  postgres prometheus grafana loki promtail 2>&1 | tail -5

# Wait for Postgres readiness.
for i in {1..30}; do
  if PSQL "$DATABASE_URL" -c "SELECT 1" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
PSQL "$DATABASE_URL" -c "SELECT 1" >/dev/null || { err "Postgres still not ready after 30s"; exit 1; }

log "Observability stack up. Grafana: http://localhost:3000 (admin/admin)"

# ─────────────────────────────────────────────────────────────────────────
# Step 3 — DB schema migration
# ─────────────────────────────────────────────────────────────────────────

step 3 "Applying Postgres migrations"
# Apply migrations directly via psql (avoids sqlx-cli dependency). Each
# migration is idempotent (CREATE TABLE IF NOT EXISTS / CREATE INDEX IF
# NOT EXISTS) so re-runs are safe. Path resolves inside the container
# because we mount the repo dir into postgres compose service... actually
# the postgres container doesn't have the repo mounted, so we pipe the
# .sql file via stdin instead.
for sqlfile in migrations/*.sql; do
  log "  applying $(basename "$sqlfile")"
  if [ "$PSQL_VIA_DOCKER" -eq 1 ]; then
    docker compose -f deploy/docker/docker-compose.yml exec -T postgres \
      psql -U aether -d aether < "$sqlfile" >/dev/null 2>&1 || \
      warn "    $(basename "$sqlfile") returned non-zero (likely already applied)"
  else
    psql "$DATABASE_URL" -f "$sqlfile" >/dev/null 2>&1 || \
      warn "    $(basename "$sqlfile") returned non-zero (likely already applied)"
  fi
done

if [ "$FRESH" -eq 1 ]; then
  warn "--fresh: truncating demo tables"
  PSQL "$DATABASE_URL" -c "
    TRUNCATE TABLE bundles, arbs, mempool_predictions, mempool_reconciliation,
                   inclusion_results, pnl_daily RESTART IDENTITY CASCADE;
  " || true
fi

# ─────────────────────────────────────────────────────────────────────────
# Step 4 — build artifacts
# ─────────────────────────────────────────────────────────────────────────

step 4 "Building Rust + Go + Solidity artifacts (skips if already built)"

if [ ! -f target/release/aether-rust ]; then
  log "Building Rust release binaries..."
  cargo build --release --bins 2>&1 | tail -3
fi

# The profit scorer is a Rust bin built by `--bins` above; build it explicitly
# if the rust build was skipped (aether-rust present but scorer absent) so the
# profitability ledger always has a producer to supervise.
if [ ! -f target/release/aether-profit-scorer ]; then
  log "Building aether-profit-scorer..."
  cargo build --release --bin aether-profit-scorer 2>&1 | tail -3
fi

mkdir -p bin
# Build each Go binary independently — earlier `if [ ! -f bin/aether-executor ]`
# wrapper skipped the other two when executor existed, leaving demo with
# missing reconciler / monitor.
for goprog in executor reconciler monitor pooldiscovery; do
  if [ ! -f "bin/aether-$goprog" ]; then
    log "Building bin/aether-$goprog..."
    go build -o "bin/aether-$goprog" "./cmd/$goprog" 2>&1 | tail -3
  fi
done

if [ ! -f contracts/out/AetherExecutor.sol/AetherExecutor.json ]; then
  log "Building Solidity artifacts..."
  (cd contracts && forge build) 2>&1 | tail -3
fi

# The bytecode artifact is only needed when the backrun validator/executor run
# (Arm B). Arm A is analytical-only, so don't hard-fail on a missing artifact.
if [ "$AETHER_ARM" != "A" ] && [ ! -f "$AETHER_EXECUTOR_BYTECODE_PATH" ]; then
  err "Bytecode artifact missing at $AETHER_EXECUTOR_BYTECODE_PATH"
  err "Run 'cd contracts && forge build' to produce it."
  exit 1
fi

log "Artifacts ready"

# ─────────────────────────────────────────────────────────────────────────
# Step 5 — pool registry refresh (one-shot)
# ─────────────────────────────────────────────────────────────────────────

step 5 "Pool registry check (using existing config/pools.toml)"
# Skip overwriting config/pools.toml — pooldiscovery's --output regenerates
# the file with only the top N discovered pools, which is fine for a fresh
# repo but destructive for a curated registry. Operators who want to refresh
# should run pooldiscovery manually before the demo.
pool_count=$(grep -c '^\[\[pools\]\]' config/pools.toml 2>/dev/null || echo 0)
log "Pool registry: $pool_count pools loaded"
if [ "$pool_count" -lt 100 ]; then
  warn "Pool registry has $pool_count pools — sparse. Candidate rate will be low."
  warn "Refresh manually with: bin/aether-pooldiscovery --rpc-url \$ETH_RPC_URL --output config/pools.toml --limit 1000"
fi

# ─────────────────────────────────────────────────────────────────────────
# Step 6 — Rust core (with auto-restart loop)
# ─────────────────────────────────────────────────────────────────────────

# Spawn `cmd ...` under a forever-restart loop with size-capped log file.
# Loop PID is tracked in CHILD_LOOPS so cleanup() can kill the supervisor
# before the binary itself, preventing race-respawn during shutdown.
spawn_with_restart() {
  local name="$1"
  local log_file="$LOG_DIR/$name.log"
  shift
  (
    # Crash-loop guard: count restarts in a rolling 60s window. >=5 restarts
    # in that window is a crash-loop — emit a loud [ALERT] (log + stderr) and
    # back off 60s instead of hammering every 5s. We keep retrying (a long
    # unattended run should self-heal a transient outage) but slowly and
    # visibly, so an operator or a log-based alert rule can catch it instead
    # of a silent 5s respawn storm burning RPC/CPU for days.
    restarts=0
    window_start=$(date +%s)
    while true; do
      echo "[$(date -u +%FT%TZ)] starting $name: $*" >> "$log_file"
      "$@" >> "$log_file" 2>&1
      ec=$?
      now=$(date +%s)
      echo "[$(date -u +%FT%TZ)] $name exited code=$ec" >> "$log_file"
      # Rotate if log >100MB.
      if [ -f "$log_file" ] && [ "$(stat -f%z "$log_file" 2>/dev/null || stat -c%s "$log_file" 2>/dev/null || echo 0)" -gt 104857600 ]; then
        mv "$log_file" "$log_file.$(date -u +%FT%TZ)"
        gzip "$log_file."* &
      fi
      if [ $((now - window_start)) -le 60 ]; then
        restarts=$((restarts + 1))
      else
        restarts=1
        window_start=$now
      fi
      if [ "$restarts" -ge 5 ]; then
        echo "[$(date -u +%FT%TZ)] [ALERT] $name crash-looping: $restarts restarts in <=60s (last exit=$ec); backing off 60s" | tee -a "$log_file" >&2
        sleep 60
        restarts=0
        window_start=$(date +%s)
      else
        sleep 5
      fi
    done
  ) &
  local loop_pid=$!
  CHILD_LOOPS+=("$loop_pid")
  # Give it a beat to spawn the child so we can capture the PID.
  sleep 2
  local child_pid
  child_pid=$(pgrep -P "$loop_pid" -n || true)
  PID_NAMES+=("$name")
  PID_VALUES+=("$child_pid")
  log "$name started (loop PID $loop_pid, child PID $child_pid). Log: $log_file"
}

# Background log janitor — bounds host-process log growth over long
# (multi-day) shadow runs. spawn_with_restart only rotates a log when its
# process *exits and restarts*; a process that stays up for days never hits
# that path, so its log grows without bound. This loop runs independently and
# copytruncate-rotates any oversized $LOG_DIR/*.log, truncating the file in
# place so the live appender fd keeps working — a plain mv/rename would orphan
# it and the process would keep writing to the moved inode while the new file
# stayed empty. Tracked in CHILD_LOOPS so cleanup() stops it first.
#   DEMO_LOG_CAP_BYTES          per-log size cap (default 200MB)
#   DEMO_LOG_JANITOR_INTERVAL_SEC  scan cadence (default 300s)
start_log_janitor() {
  local cap_bytes="${DEMO_LOG_CAP_BYTES:-209715200}"
  local interval="${DEMO_LOG_JANITOR_INTERVAL_SEC:-300}"
  (
    while true; do
      sleep "$interval"
      for f in "$LOG_DIR"/*.log; do
        [ -f "$f" ] || continue
        sz=$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f" 2>/dev/null || echo 0)
        if [ "$sz" -gt "$cap_bytes" ]; then
          # copytruncate: snapshot the last generation, then truncate in
          # place. A few in-flight lines may be lost at the truncate
          # boundary (same trade-off as logrotate's copytruncate); fine for
          # forensic logs. Only one generation (.1.gz) is kept, so disk stays
          # bounded at ~cap + compressed cap per log.
          cp "$f" "$f.1" 2>/dev/null && : > "$f"
          gzip -f "$f.1" 2>/dev/null &
          echo "[$(date -u +%FT%TZ)] log-janitor: rotated $(basename "$f") (was $sz bytes > cap $cap_bytes)" \
            >> "$LOG_DIR/_janitor.log"
        fi
      done
    done
  ) &
  local janitor_pid=$!
  CHILD_LOOPS+=("$janitor_pid")
  log "Log janitor started (PID $janitor_pid, cap $((cap_bytes / 1048576))MB, every ${interval}s)"
}

start_log_janitor

step 6 "Booting Rust core (ingestion + decode + BF + revm sim + gRPC publish)"
spawn_with_restart aether-rust target/release/aether-rust

# Wait for gRPC ready (poll the UDS or HTTP metrics endpoint).
for i in {1..60}; do
  if curl -sf http://localhost:9092/metrics > /dev/null 2>&1; then
    break
  fi
  sleep 1
done

# ─────────────────────────────────────────────────────────────────────────
# Step 7 — Go executor (shadow mode → bundles table)
# ─────────────────────────────────────────────────────────────────────────

if [ "$AETHER_ARM" = "A" ]; then
  step 7 "Arm A — skipping Go executor + backrun validator (analytical only)"
  log "executor not started; engine running with the backrun validator disabled"
else
  step 7 "Booting Go executor in shadow mode"
  spawn_with_restart aether-executor bin/aether-executor

  for i in {1..60}; do
    if curl -sf http://localhost:9090/metrics > /dev/null 2>&1; then
      break
    fi
    sleep 1
  done
fi

# ─────────────────────────────────────────────────────────────────────────
# Step 8 — Reconciler (newHeads watcher → mempool_reconciliation)
# ─────────────────────────────────────────────────────────────────────────

step 8 "Booting Go reconciler (predicted-vs-actual fill)"
spawn_with_restart aether-reconciler bin/aether-reconciler

# Profit scorer (Rust bin) — writes mempool_profitability, the "would-be
# profit" headline of the run. Supervised like the others; without it the
# profitability ledger never fills. Reuses MEMPOOL_LEDGER_DSN + ETH_RPC_URL
# (already exported); metrics on :9095.
export PROFIT_SCORER_METRICS_ADDR="${PROFIT_SCORER_METRICS_ADDR:-:9095}"
export AETHER_GIT_SHA="${AETHER_GIT_SHA:-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)}"
spawn_with_restart aether-profit-scorer target/release/aether-profit-scorer

# ─────────────────────────────────────────────────────────────────────────
# Step 9 — Monitor (HTML dashboard at :8080)
# ─────────────────────────────────────────────────────────────────────────

step 9 "Booting Go monitor (HTML dashboard at :8080)"
spawn_with_restart aether-monitor bin/aether-monitor

# ─────────────────────────────────────────────────────────────────────────
# Step 10 — open Grafana + live monitor loop
# ─────────────────────────────────────────────────────────────────────────

step 10 "Live monitor loop. Ctrl-C to stop."

if [ "$OPEN_GRAFANA" -eq 1 ]; then
  sleep 3
  if command -v open >/dev/null 2>&1; then
    open "http://localhost:3000/d/mempool/mempool-tracking" 2>/dev/null || true
  elif command -v xdg-open >/dev/null 2>&1; then
    xdg-open "http://localhost:3000/d/mempool/mempool-tracking" 2>/dev/null || true
  fi
fi

echo ""
log "═══════════════════════════════════════════════════════════════"
log "  Aether demo running."
log "  Grafana:    http://localhost:3000 (admin/admin)"
log "  Monitor:    http://localhost:8080"
log "  Prometheus: http://localhost:9091"
log "  Rust /metrics:    http://localhost:9092/metrics"
log "  Go /metrics:      http://localhost:9090/metrics"
log "  Reconciler /metrics: http://localhost:9094/metrics"
log "  Run dir:    $RUN_DIR"
log "═══════════════════════════════════════════════════════════════"
log "  Live counts every 10s. Ctrl-C to stop + dump bundles."
echo ""

# Live counts loop — polls Postgres every 10s, prints decoded / candidates /
# shadow-bundles counts since demo start. Lightweight (3 quick aggregates).
while true; do
  read -r predictions candidates bundles_total <<< "$(PSQL -At -F' ' "$DATABASE_URL" -c "
    SELECT
      (SELECT count(*) FROM mempool_predictions WHERE decoded_at >= '$TS'::timestamptz),
      (SELECT count(*) FROM mempool_predictions WHERE profit_factor_predicted IS NOT NULL AND profit_factor_predicted > 1.0 AND decoded_at >= '$TS'::timestamptz),
      (SELECT count(*) FROM bundles WHERE is_shadow = true AND submitted_at >= '$TS'::timestamptz);
  " 2>/dev/null || echo "0 0 0")"
  ts_now=$(date -u +%FT%TZ)
  printf "${BLUE}[%s]${NC} predictions=%-8s candidates=%-6s shadow_bundles=%s\n" \
    "$ts_now" "${predictions:-0}" "${candidates:-0}" "${bundles_total:-0}"
  sleep 10
done
