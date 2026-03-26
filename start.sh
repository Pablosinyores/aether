#!/usr/bin/env bash
set -e

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$PROJECT_DIR"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

log() { echo -e "${GREEN}[aether]${NC} $1"; }
warn() { echo -e "${YELLOW}[aether]${NC} $1"; }
err() { echo -e "${RED}[aether]${NC} $1"; }

# Load environment
if [ ! -f .env ]; then
    err ".env file not found. Create one from the template first."
    exit 1
fi
set -a && source .env && set +a
log "Environment loaded from .env"

# Track PIDs for cleanup
RUST_PID=""
EXECUTOR_PID=""
MONITOR_PID=""

cleanup() {
    echo ""
    warn "Shutting down all services..."
    for pid in $RUST_PID $EXECUTOR_PID $MONITOR_PID; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null
        fi
    done
    wait 2>/dev/null
    log "All services stopped."
    exit 0
}
trap cleanup SIGINT SIGTERM

# Kill anything already on our ports
for port in 50051 9090 8080; do
    pid=$(lsof -ti :$port 2>/dev/null || true)
    if [ -n "$pid" ]; then
        warn "Killing existing process on port $port (PID $pid)"
        kill $pid 2>/dev/null || true
        sleep 1
    fi
done

# 1. Start Docker infra
log "Starting infrastructure (Postgres, Redis, Prometheus)..."
docker compose -f deploy/docker/docker-compose.yml up -d postgres redis prometheus 2>&1 | tail -1
sleep 2

# Verify containers
if ! docker ps --format '{{.Names}}' | grep -q aether-postgres; then
    err "PostgreSQL container failed to start"
    exit 1
fi
log "Infrastructure is up"

# 2. Build if needed
if [ ! -f target/release/aether-rust ]; then
    log "Rust binary not found, building..."
    cargo build --release --bin aether-rust
fi
log "Rust binary ready"

# 3. Start Rust core
log "Starting Rust core (gRPC server)..."
./target/release/aether-rust 2>&1 | sed "s/^/  [rust] /" &
RUST_PID=$!
sleep 3

if ! kill -0 "$RUST_PID" 2>/dev/null; then
    err "Rust core failed to start. Check ETH_RPC_URL in .env"
    exit 1
fi
log "Rust core is running (PID $RUST_PID)"

# 4. Start Go executor
log "Starting Go executor..."
go run ./cmd/executor/ 2>&1 | sed "s/^/  [executor] /" &
EXECUTOR_PID=$!
sleep 3

if ! kill -0 "$EXECUTOR_PID" 2>/dev/null; then
    err "Go executor failed to start"
    cleanup
    exit 1
fi
log "Go executor is running (PID $EXECUTOR_PID)"

# 5. Start Go monitor
log "Starting Go monitor..."
go run ./cmd/monitor/ 2>&1 | sed "s/^/  [monitor] /" &
MONITOR_PID=$!
sleep 2
log "Go monitor is running (PID $MONITOR_PID)"

echo ""
log "========================================="
log "  All services are running!"
log "========================================="
log "  Dashboard:  http://localhost:8080"
log "  Metrics:    http://localhost:9090/metrics"
log "  Prometheus: http://localhost:9091"
log "  gRPC:       localhost:50051"
log "========================================="
log "  Press Ctrl+C to stop all services"
echo ""

# Wait for all background processes
wait
