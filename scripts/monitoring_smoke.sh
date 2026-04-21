#!/usr/bin/env bash
# monitoring_smoke.sh — end-to-end smoke test for the Aether monitoring stack.
#
# Dependencies: docker (with compose plugin), curl, jq
#
# Exit codes:
#   0 — all checks passed
#   1 — readiness timeout (service did not become healthy)
#   2 — alert did not fire within timeout
#   3 — teardown failed
#
# Usage: bash scripts/monitoring_smoke.sh
#        Run from the repo root. The script brings up deploy/docker and tears
#        it down automatically. Does NOT require Pushgateway, amtool, or any
#        extra container — only the existing stack + docker cp + curl.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
COMPOSE_DIR="${REPO_ROOT}/deploy/docker"
COMPOSE_FILE="${COMPOSE_DIR}/docker-compose.yml"

PROMETHEUS_URL="http://localhost:9091"
ALERTMANAGER_URL="http://localhost:9093"
GRAFANA_URL="http://localhost:3000"

SYNTHETIC_RULES_FILE="/tmp/aether-synthetic-rules.yml"
TEARDOWN_DONE=0

# ---------------------------------------------------------------------------
# preflight: assert required tools are present
# ---------------------------------------------------------------------------
preflight() {
  local missing=0
  for cmd in docker curl jq; do
    if ! command -v "${cmd}" &>/dev/null; then
      echo "ERROR: required tool not found: ${cmd}" >&2
      missing=1
    fi
  done

  if ! docker compose version &>/dev/null; then
    echo "ERROR: 'docker compose' plugin not available (need Docker 20.10+)" >&2
    missing=1
  fi

  if [[ "${missing}" -eq 1 ]]; then
    exit 1
  fi

  echo "preflight: OK (docker, curl, jq available)"
}

# ---------------------------------------------------------------------------
# readiness_wait <url> <timeout_seconds>
# Polls GET <url> until HTTP 200 or timeout. Exits 1 on timeout.
# ---------------------------------------------------------------------------
readiness_wait() {
  local url="${1}"
  local timeout="${2}"
  local elapsed=0
  local interval=3

  echo "waiting for ${url} (timeout ${timeout}s)..."
  while true; do
    if curl -sf --max-time 2 "${url}" &>/dev/null; then
      echo "  ready: ${url}"
      return 0
    fi
    if [[ "${elapsed}" -ge "${timeout}" ]]; then
      echo "ERROR: timed out waiting for ${url} after ${timeout}s" >&2
      exit 1
    fi
    sleep "${interval}"
    elapsed=$((elapsed + interval))
  done
}

# ---------------------------------------------------------------------------
# fire_synthetic <alertname> <severity>
# Injects a synthetic rule into Prometheus via docker cp + lifecycle reload,
# then asserts the alert is active in Alertmanager within 30s.
# ---------------------------------------------------------------------------
fire_synthetic() {
  local alertname="${1}"
  local severity="${2}"
  local container="aether-prometheus"
  local timeout=60
  local elapsed=0
  local interval=3

  echo "firing synthetic alert: alertname=${alertname} severity=${severity}"

  # Write a temporary rules file with expr that always fires.
  cat > "${SYNTHETIC_RULES_FILE}" <<EOF
groups:
  - name: aether_synthetic_smoke
    rules:
      - alert: ${alertname}
        expr: vector(1) > 0
        for: 0s
        labels:
          severity: ${severity}
          job: aether-go
          synthetic: "true"
        annotations:
          summary: "Synthetic smoke test for ${alertname}"
          description: "Injected by monitoring_smoke.sh"
          runbook_url: "https://github.com/Pablosinyores/aether/blob/main/docs/runbooks/${alertname}.md"
EOF

  # Copy into the running Prometheus container.
  docker cp "${SYNTHETIC_RULES_FILE}" "${container}:/etc/prometheus/synthetic.yml"

  # Reload Prometheus config via the lifecycle API (--web.enable-lifecycle required).
  if ! curl -sf --max-time 5 -XPOST "${PROMETHEUS_URL}/-/reload" &>/dev/null; then
    echo "WARNING: Prometheus reload returned non-200; alerts may still propagate" >&2
  fi

  # Wait for the alert to appear as active in Alertmanager.
  echo "  polling Alertmanager for active alert ${alertname}..."
  while true; do
    local found
    found=$(curl -sf --max-time 5 "${ALERTMANAGER_URL}/api/v2/alerts" 2>/dev/null \
      | jq --arg name "${alertname}" \
          'any(.[]; .labels.alertname == $name and .status.state == "active")' 2>/dev/null \
      || echo "false")

    if [[ "${found}" == "true" ]]; then
      echo "  PASS: ${alertname} is active in Alertmanager"
      # Clean up the synthetic rule immediately.
      docker exec "${container}" rm -f /etc/prometheus/synthetic.yml || true
      curl -sf --max-time 5 -XPOST "${PROMETHEUS_URL}/-/reload" &>/dev/null || true
      return 0
    fi

    if [[ "${elapsed}" -ge "${timeout}" ]]; then
      echo "ERROR: alert ${alertname} did not become active in Alertmanager within ${timeout}s" >&2
      exit 2
    fi

    sleep "${interval}"
    elapsed=$((elapsed + interval))
  done
}

# ---------------------------------------------------------------------------
# teardown: bring down the stack (called via trap)
# ---------------------------------------------------------------------------
teardown() {
  if [[ "${TEARDOWN_DONE}" -eq 1 ]]; then
    return
  fi
  TEARDOWN_DONE=1
  echo "teardown: stopping stack..."
  if ! docker compose -f "${COMPOSE_FILE}" down -v --timeout 30; then
    echo "ERROR: docker compose down failed" >&2
    exit 3
  fi
  rm -f "${SYNTHETIC_RULES_FILE}"
  echo "teardown: done"
}

trap teardown EXIT INT TERM

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  echo "=== Aether Monitoring Smoke Test ==="
  echo "repo root: ${REPO_ROOT}"
  echo ""

  preflight

  echo ""
  echo "--- starting stack ---"
  docker compose -f "${COMPOSE_FILE}" up -d

  echo ""
  echo "--- readiness checks ---"
  readiness_wait "${PROMETHEUS_URL}/-/healthy" 120
  readiness_wait "${ALERTMANAGER_URL}/-/healthy" 60
  readiness_wait "${GRAFANA_URL}/api/health" 60

  echo ""
  echo "--- firing synthetic alerts ---"

  # Fire each of the 12 alert rules in alerts.yml by name + severity. The
  # helper injects a synthetic always-firing rule with the same alertname and
  # severity label, reloads Prometheus, and asserts delivery in Alertmanager.
  fire_synthetic "AetherHalted"                "critical"
  fire_synthetic "AetherInclusionRateLow"      "warning"
  fire_synthetic "AetherE2ELatencyHigh"        "warning"
  fire_synthetic "AetherNoOpportunities"       "warning"
  fire_synthetic "AetherETHBalanceLow"         "critical"
  fire_synthetic "AetherGasHigh"               "info"
  fire_synthetic "AetherBuilderDown"           "critical"
  fire_synthetic "AetherServiceDown"           "critical"
  fire_synthetic "AetherNoBlocksProcessed"     "critical"
  fire_synthetic "AetherHighSimulationLatency" "warning"
  fire_synthetic "AetherNegativeDailyPnL"      "warning"
  fire_synthetic "AetherRiskRejectionStorm"    "warning"

  echo ""
  echo "=== ALL CHECKS PASSED ==="
}

main "$@"
