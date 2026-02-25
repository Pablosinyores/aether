#!/usr/bin/env bash
# Integration test runner for cross-language gRPC tests.
# Builds the Rust binary, then runs Go integration tests against it.
#
# Usage:
#   ./scripts/test_integration.sh
#
# Environment:
#   SKIP_RUST_BUILD=1  Skip building the Rust binary (use existing)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

echo "=== Aether Integration Tests ==="
echo ""

# Step 1: Build Rust binary
if [[ "${SKIP_RUST_BUILD:-}" != "1" ]]; then
    echo "[1/3] Building Rust gRPC server..."
    cargo build --release -p aether-grpc-server 2>&1 | tail -3
    echo "  -> Built: target/release/aether-rust"
else
    echo "[1/3] Skipping Rust build (SKIP_RUST_BUILD=1)"
fi

RUST_BINARY="${PROJECT_ROOT}/target/release/aether-rust"
if [[ ! -f "${RUST_BINARY}" ]]; then
    echo "ERROR: Rust binary not found at ${RUST_BINARY}"
    echo "Build with: cargo build --release -p aether-grpc-server"
    exit 1
fi
export AETHER_RUST_BINARY="${RUST_BINARY}"

# Step 2: Run Go unit + integration tests
echo ""
echo "[2/3] Running Go unit tests..."
go test ./... 2>&1 | tail -10
echo ""

echo "[3/3] Running Go integration tests (cross-language gRPC)..."
go test -tags integration -v -timeout 60s ./cmd/executor/ -run 'TestGRPCCrossLanguage' 2>&1

echo ""
echo "=== All integration tests passed ==="
