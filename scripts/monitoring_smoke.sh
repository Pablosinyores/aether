#!/usr/bin/env bash
# monitoring_smoke.sh — end-to-end smoke test for the Aether monitoring stack.
#
# Brings up the deploy/docker stack, waits for readiness, then asserts:
#   1. Prometheus loaded every expected alert rule (via /api/v1/rules).
#   2. Every alert rule has the required annotations (summary, description,
#      runbook_url) and a severity label.
#   3. Prometheus discovered both scrape targets (aether-go, aether-rust).
#   4. Alertmanager /-/ready is healthy and its config was accepted
#      (/api/v2/status → configYAML non-empty).
#   5. Grafana provisioned every expected dashboard UID.
#
# Injection-based firing (docker cp + /-/reload) was intentionally removed:
# main's prometheus.yml pins rule_files to a single path and the compose
# stack does not pass --web.enable-lifecycle, so both legs of that approach
# were no-ops. Asserting rule loadedness and Alertmanager config acceptance
# gives deterministic coverage without modifying files the PR must not touch.
#
# Dependencies: docker (with compose plugin), curl, jq
#
# Exit codes:
#   0 — all checks passed
#   1 — readiness timeout (service did not become healthy)
#   2 — a required assertion failed (missing rule, missing annotation,
#       missing dashboard, missing scrape target, empty alertmanager config)
#   3 — teardown failed
#
# Usage: bash scripts/monitoring_smoke.sh
#        Run from the repo root.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
COMPOSE_DIR="${REPO_ROOT}/deploy/docker"
COMPOSE_FILE="${COMPOSE_DIR}/docker-compose.yml"

PROMETHEUS_URL="${PROMETHEUS_URL:-http://localhost:9091}"
ALERTMANAGER_URL="${ALERTMANAGER_URL:-http://localhost:9093}"
GRAFANA_URL="${GRAFANA_URL:-http://localhost:3000}"

TEARDOWN_DONE=0
FAIL_COUNT=0

# Expected alert rules — must match deploy/docker/prometheus/alerts.yml.
EXPECTED_ALERTS=(
  AetherHalted
  AetherInclusionRateLow
  AetherE2ELatencyHigh
  AetherNoOpportunities
  AetherETHBalanceLow
  AetherGasHigh
  AetherBuilderDown
  AetherServiceDown
  AetherNoBlocksProcessed
  AetherHighSimulationLatency
  AetherNegativeDailyPnL
  AetherRiskRejectionStorm
)

# Expected Prometheus scrape jobs — must match deploy/docker/prometheus.yml.
EXPECTED_TARGETS=(aether-go aether-rust)

# Expected Grafana dashboard UIDs — one per JSON in grafana/dashboards/.
EXPECTED_DASHBOARDS=(aether-overview aether-latency aether-builders aether-risk)

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
  [[ "${missing}" -eq 1 ]] && exit 1
  echo "preflight: OK (docker, curl, jq available)"
}

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

fail() {
  echo "  FAIL: $*" >&2
  FAIL_COUNT=$((FAIL_COUNT + 1))
}

pass() {
  echo "  PASS: $*"
}

# ---------------------------------------------------------------------------
# assert_rules_loaded
# Queries Prometheus /api/v1/rules, asserts every expected alertname is
# present and each rule has required annotations + severity label.
# ---------------------------------------------------------------------------
assert_rules_loaded() {
  echo ""
  echo "--- asserting Prometheus loaded all ${#EXPECTED_ALERTS[@]} alert rules ---"

  local rules_json
  if ! rules_json=$(curl -sf --max-time 10 "${PROMETHEUS_URL}/api/v1/rules" 2>/dev/null); then
    fail "could not fetch ${PROMETHEUS_URL}/api/v1/rules"
    return
  fi

  local status
  status=$(echo "${rules_json}" | jq -r '.status // "error"')
  if [[ "${status}" != "success" ]]; then
    fail "Prometheus /api/v1/rules returned status=${status}"
    return
  fi

  for alert in "${EXPECTED_ALERTS[@]}"; do
    local match
    match=$(echo "${rules_json}" | jq --arg n "${alert}" \
      '[.data.groups[].rules[]? | select(.type == "alerting" and .name == $n)] | .[0] // null')
    if [[ "${match}" == "null" ]]; then
      fail "alert ${alert} not loaded"
      continue
    fi

    local has_summary has_description has_runbook has_severity
    has_summary=$(echo "${match}"     | jq -r '.annotations.summary // empty'     | grep -c . || true)
    has_description=$(echo "${match}" | jq -r '.annotations.description // empty' | grep -c . || true)
    has_runbook=$(echo "${match}"     | jq -r '.annotations.runbook_url // empty' | grep -c . || true)
    has_severity=$(echo "${match}"    | jq -r '.labels.severity // empty'         | grep -c . || true)

    if [[ "${has_summary}" -eq 0 ]]; then
      fail "${alert} missing annotations.summary"
    elif [[ "${has_description}" -eq 0 ]]; then
      fail "${alert} missing annotations.description"
    elif [[ "${has_runbook}" -eq 0 ]]; then
      fail "${alert} missing annotations.runbook_url"
    elif [[ "${has_severity}" -eq 0 ]]; then
      fail "${alert} missing labels.severity"
    else
      pass "${alert} loaded with summary, description, runbook_url, severity"
    fi
  done
}

# ---------------------------------------------------------------------------
# assert_scrape_targets_up
# Asserts Prometheus discovered every expected scrape job. Target health
# (up/down) is not asserted because aether-go / aether-rust may be absent or
# unhealthy in CI — only discovery is required for the monitoring contract.
# ---------------------------------------------------------------------------
assert_scrape_targets_up() {
  echo ""
  echo "--- asserting Prometheus discovered scrape targets ---"
  local targets_json
  if ! targets_json=$(curl -sf --max-time 10 "${PROMETHEUS_URL}/api/v1/targets" 2>/dev/null); then
    fail "could not fetch ${PROMETHEUS_URL}/api/v1/targets"
    return
  fi

  for job in "${EXPECTED_TARGETS[@]}"; do
    local count
    count=$(echo "${targets_json}" | jq --arg j "${job}" \
      '[.data.activeTargets[]? | select(.labels.job == $j)] | length')
    if [[ "${count}" -ge 1 ]]; then
      pass "scrape job ${job} discovered (${count} target(s))"
    else
      fail "scrape job ${job} not discovered by Prometheus"
    fi
  done
}

# ---------------------------------------------------------------------------
# assert_alertmanager_config
# /api/v2/status returns configYAML (parsed Alertmanager config). Empty means
# config failed to load.
# ---------------------------------------------------------------------------
assert_alertmanager_config() {
  echo ""
  echo "--- asserting Alertmanager accepted its config ---"
  local status_json
  if ! status_json=$(curl -sf --max-time 10 "${ALERTMANAGER_URL}/api/v2/status" 2>/dev/null); then
    fail "could not fetch ${ALERTMANAGER_URL}/api/v2/status"
    return
  fi

  local config_yaml
  config_yaml=$(echo "${status_json}" | jq -r '.config.original // empty')
  if [[ -z "${config_yaml}" ]]; then
    fail "Alertmanager config.original empty — config failed to load"
    return
  fi

  if echo "${config_yaml}" | grep -q "slack-default"; then
    pass "Alertmanager config loaded (slack-default receiver present)"
  else
    fail "Alertmanager config loaded but slack-default receiver not found"
  fi
}

# ---------------------------------------------------------------------------
# assert_dashboards_provisioned
# Grafana anonymous viewer is enabled in compose, so /api/search is callable
# without auth.
# ---------------------------------------------------------------------------
assert_dashboards_provisioned() {
  echo ""
  echo "--- asserting Grafana provisioned all dashboards ---"
  local search_json
  if ! search_json=$(curl -sf --max-time 10 "${GRAFANA_URL}/api/search?type=dash-db" 2>/dev/null); then
    fail "could not fetch ${GRAFANA_URL}/api/search"
    return
  fi

  for uid in "${EXPECTED_DASHBOARDS[@]}"; do
    local count
    count=$(echo "${search_json}" | jq --arg u "${uid}" '[.[] | select(.uid == $u)] | length')
    if [[ "${count}" -ge 1 ]]; then
      pass "dashboard ${uid} provisioned"
    else
      fail "dashboard ${uid} not provisioned"
    fi
  done
}

# ---------------------------------------------------------------------------
teardown() {
  [[ "${TEARDOWN_DONE}" -eq 1 ]] && return
  TEARDOWN_DONE=1
  echo ""
  echo "teardown: stopping stack..."
  if ! docker compose -f "${COMPOSE_FILE}" down -v --timeout 30; then
    echo "ERROR: docker compose down failed" >&2
    exit 3
  fi
  echo "teardown: done"
}

trap teardown EXIT INT TERM

# ---------------------------------------------------------------------------
main() {
  echo "=== Aether Monitoring Smoke Test ==="
  echo "repo root: ${REPO_ROOT}"
  echo ""

  preflight

  echo ""
  echo "--- starting monitoring stack (prometheus, alertmanager, grafana) ---"
  # Only bring up monitoring services — aether-go / aether-rust are not
  # required for the assertions below and may fail to build in CI.
  docker compose -f "${COMPOSE_FILE}" up -d prometheus alertmanager grafana

  echo ""
  echo "--- readiness checks ---"
  readiness_wait "${PROMETHEUS_URL}/-/ready" 120
  readiness_wait "${ALERTMANAGER_URL}/-/ready" 60
  readiness_wait "${GRAFANA_URL}/api/health" 60

  assert_rules_loaded
  assert_scrape_targets_up
  assert_alertmanager_config
  assert_dashboards_provisioned

  echo ""
  if [[ "${FAIL_COUNT}" -eq 0 ]]; then
    echo "=== ALL CHECKS PASSED ==="
    exit 0
  else
    echo "=== FAILED: ${FAIL_COUNT} assertion(s) ==="
    exit 2
  fi
}

main "$@"
