#!/usr/bin/env bash
set -euo pipefail

# Aether Arbitrage Engine - Deployment Script
#
# Usage:
#   ./scripts/deploy.sh build           # Build all binaries
#   ./scripts/deploy.sh test            # Run all tests
#   ./scripts/deploy.sh deploy staging  # Deploy to staging
#   ./scripts/deploy.sh deploy prod     # Deploy to production
#   ./scripts/deploy.sh rollback prod   # Rollback to previous version
#   ./scripts/deploy.sh status prod     # Check service status
#   ./scripts/deploy.sh logs prod rust  # View service logs

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_ROOT/build"
BIN_DIR="$BUILD_DIR/bin"
VERSION=$(git -C "$PROJECT_ROOT" describe --tags --always --dirty 2>/dev/null || echo "dev")
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info()  { echo -e "${BLUE}[INFO]${NC} $*"; }
log_ok()    { echo -e "${GREEN}[OK]${NC} $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# --- Build ---

build_rust() {
    log_info "Building Rust binaries (release mode with LTO)..."
    cd "$PROJECT_ROOT"
    cargo build --release --workspace
    mkdir -p "$BIN_DIR"
    cp target/release/aether-grpc-server "$BIN_DIR/"
    log_ok "Rust binary: $BIN_DIR/aether-grpc-server"
}

build_go() {
    log_info "Building Go binaries..."
    cd "$PROJECT_ROOT"
    mkdir -p "$BIN_DIR"

    for cmd in executor monitor; do
        CGO_ENABLED=0 go build -ldflags="-s -w -X main.version=$VERSION" \
            -o "$BIN_DIR/aether-$cmd" "./cmd/$cmd/"
        log_ok "Go binary: $BIN_DIR/aether-$cmd"
    done
}

build_contracts() {
    log_info "Building Solidity contracts..."
    cd "$PROJECT_ROOT/contracts"
    forge build
    log_ok "Contracts built"
}

build_all() {
    log_info "Building all components (version: $VERSION)"
    build_rust
    build_go
    build_contracts
    log_ok "All builds complete"
}

# --- Test ---

test_rust() {
    log_info "Running Rust tests..."
    cd "$PROJECT_ROOT"
    cargo clippy --workspace -- -D warnings
    cargo test --workspace
    log_ok "Rust tests passed"
}

test_go() {
    log_info "Running Go tests..."
    cd "$PROJECT_ROOT"
    go vet ./...
    go test ./... -v -count=1
    log_ok "Go tests passed"
}

test_solidity() {
    log_info "Running Solidity tests..."
    cd "$PROJECT_ROOT/contracts"
    forge test -vvv
    log_ok "Solidity tests passed"
}

test_all() {
    log_info "Running all tests"
    test_rust
    test_go
    test_solidity
    log_ok "All tests passed"
}

# --- Deploy ---

deploy() {
    local env="$1"
    local inventory

    case "$env" in
        staging) inventory="$PROJECT_ROOT/deploy/ansible/inventory.yml" ;;
        prod|production) inventory="$PROJECT_ROOT/deploy/ansible/inventory.yml" ;;
        *) log_error "Unknown environment: $env"; exit 1 ;;
    esac

    log_info "Deploying version $VERSION to $env"

    # Pre-flight checks
    if [[ ! -f "$BIN_DIR/aether-grpc-server" ]] || [[ ! -f "$BIN_DIR/aether-executor" ]]; then
        log_warn "Binaries not found, building first..."
        build_all
    fi

    # Create release archive
    local archive="$BUILD_DIR/aether-$VERSION-$TIMESTAMP.tar.gz"
    log_info "Creating release archive: $archive"
    tar -czf "$archive" \
        -C "$PROJECT_ROOT" \
        build/bin/ \
        config/ \
        deploy/systemd/

    log_ok "Archive created: $archive"

    # Run ansible playbook
    local target_group
    if [[ "$env" == "prod" ]] || [[ "$env" == "production" ]]; then
        target_group="production"
    else
        target_group="$env"
    fi

    log_info "Running Ansible playbook for $target_group..."
    if command -v ansible-playbook &>/dev/null; then
        ansible-playbook \
            -i "$inventory" \
            "$PROJECT_ROOT/deploy/ansible/playbook.yml" \
            --limit "$target_group" \
            --extra-vars "aether_version=$VERSION"
    else
        log_warn "ansible-playbook not found. Manual deployment steps:"
        echo "  1. Copy $archive to target server"
        echo "  2. Extract to /opt/aether/"
        echo "  3. sudo systemctl restart aether-rust aether-go"
    fi

    log_ok "Deployment complete: $VERSION -> $env"
}

# --- Rollback ---

rollback() {
    local env="$1"
    log_info "Rolling back $env to previous version..."

    local inventory="$PROJECT_ROOT/deploy/ansible/inventory.yml"
    local target_group
    if [[ "$env" == "prod" ]] || [[ "$env" == "production" ]]; then
        target_group="production"
    else
        target_group="$env"
    fi

    if command -v ansible &>/dev/null; then
        ansible "$target_group" -i "$inventory" -m shell \
            -a "systemctl stop aether-go aether-rust && \
                ln -sfn /opt/aether/releases/previous /opt/aether/current && \
                systemctl start aether-rust aether-go" \
            --become
    else
        log_warn "ansible not found. Manual rollback:"
        echo "  1. SSH to target server"
        echo "  2. sudo systemctl stop aether-go aether-rust"
        echo "  3. ln -sfn /opt/aether/releases/previous /opt/aether/current"
        echo "  4. sudo systemctl start aether-rust aether-go"
    fi

    log_ok "Rollback complete for $env"
}

# --- Status ---

status() {
    local env="$1"
    log_info "Checking service status for $env..."

    local inventory="$PROJECT_ROOT/deploy/ansible/inventory.yml"
    local target_group
    if [[ "$env" == "prod" ]] || [[ "$env" == "production" ]]; then
        target_group="production"
    else
        target_group="$env"
    fi

    if command -v ansible &>/dev/null; then
        ansible "$target_group" -i "$inventory" -m shell \
            -a "systemctl status aether-rust aether-go --no-pager" \
            --become
    else
        log_warn "Checking local services..."
        systemctl status aether-rust aether-go --no-pager 2>/dev/null || \
            log_warn "Services not running locally"
    fi
}

# --- Logs ---

view_logs() {
    local env="$1"
    local service="${2:-all}"
    log_info "Viewing logs for $env / $service..."

    local units=""
    case "$service" in
        rust)  units="-u aether-rust" ;;
        go)    units="-u aether-go" ;;
        all)   units="-u aether-rust -u aether-go" ;;
        *)     log_error "Unknown service: $service"; exit 1 ;;
    esac

    local inventory="$PROJECT_ROOT/deploy/ansible/inventory.yml"
    local target_group
    if [[ "$env" == "prod" ]] || [[ "$env" == "production" ]]; then
        target_group="production"
    else
        target_group="$env"
    fi

    if command -v ansible &>/dev/null; then
        ansible "$target_group" -i "$inventory" -m shell \
            -a "journalctl $units --since '1 hour ago' --no-pager -n 100" \
            --become
    else
        journalctl $units --since "1 hour ago" --no-pager -n 100 2>/dev/null || \
            log_warn "journalctl not available"
    fi
}

# --- Docker ---

docker_up() {
    log_info "Starting Docker Compose services..."
    docker-compose -f "$PROJECT_ROOT/deploy/docker/docker-compose.yml" up -d --build
    log_ok "Docker services started"
}

docker_down() {
    log_info "Stopping Docker Compose services..."
    docker-compose -f "$PROJECT_ROOT/deploy/docker/docker-compose.yml" down
    log_ok "Docker services stopped"
}

# --- Main ---

usage() {
    cat <<USAGE
Aether Arbitrage Engine - Deployment Script

Usage: $(basename "$0") <command> [args]

Commands:
  build                    Build all binaries (Rust + Go + Solidity)
  build rust               Build Rust binaries only
  build go                 Build Go binaries only
  build contracts          Build Solidity contracts only
  test                     Run all tests
  test rust                Run Rust tests only
  test go                  Run Go tests only
  test solidity            Run Solidity tests only
  deploy <env>             Deploy to environment (staging|prod)
  rollback <env>           Rollback to previous version
  status <env>             Check service status
  logs <env> [service]     View logs (service: rust|go|all)
  docker up                Start Docker Compose stack
  docker down              Stop Docker Compose stack

Environment: staging, prod
Version: $VERSION
USAGE
}

case "${1:-}" in
    build)
        case "${2:-all}" in
            all)       build_all ;;
            rust)      build_rust ;;
            go)        build_go ;;
            contracts) build_contracts ;;
            *)         log_error "Unknown build target: $2"; exit 1 ;;
        esac
        ;;
    test)
        case "${2:-all}" in
            all)      test_all ;;
            rust)     test_rust ;;
            go)       test_go ;;
            solidity) test_solidity ;;
            *)        log_error "Unknown test target: $2"; exit 1 ;;
        esac
        ;;
    deploy)
        [[ -z "${2:-}" ]] && { log_error "Specify environment: staging or prod"; exit 1; }
        deploy "$2"
        ;;
    rollback)
        [[ -z "${2:-}" ]] && { log_error "Specify environment: staging or prod"; exit 1; }
        rollback "$2"
        ;;
    status)
        [[ -z "${2:-}" ]] && { log_error "Specify environment: staging or prod"; exit 1; }
        status "$2"
        ;;
    logs)
        [[ -z "${2:-}" ]] && { log_error "Specify environment: staging or prod"; exit 1; }
        view_logs "$2" "${3:-all}"
        ;;
    docker)
        case "${2:-}" in
            up)   docker_up ;;
            down) docker_down ;;
            *)    log_error "Usage: $0 docker up|down"; exit 1 ;;
        esac
        ;;
    help|--help|-h|"")
        usage
        ;;
    *)
        log_error "Unknown command: $1"
        usage
        exit 1
        ;;
esac
