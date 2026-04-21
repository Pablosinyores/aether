# Aether Docker Monitoring Stack

## Overview

| Service | Port | Purpose |
|---------|------|---------|
| aether-go | 9090 | Go executor metrics (Prometheus) |
| aether-rust | 9092 | Rust engine metrics (Prometheus) |
| prometheus | 9091 | Metrics store, alert evaluation |
| alertmanager | 9093 | Alert routing (Slack) |
| grafana | 3000 | Dashboards (admin/admin) |

Start the monitoring-only stack: `docker compose up -d prometheus alertmanager grafana`

## Adding a Metric

1. Emit the metric in `cmd/executor/metrics.go` (Go) or `crates/grpc-server/src/metrics.rs` (Rust).
2. Restart the relevant service. Prometheus auto-scrapes on the 15s interval.
3. Verify it appears at `http://localhost:9091/graph`.

## Adding a Dashboard

1. Create a JSON file in `deploy/docker/grafana/dashboards/`. Assign a stable `"uid"` string.
2. All panels must reference `${DS_PROMETHEUS}` as the datasource uid.
3. The provisioner reloads dashboards every 30s — no Grafana restart needed.
4. Use `jq empty <file>.json` to validate JSON before committing.

## Adding an Alert

1. Add a rule block to `deploy/docker/prometheus/alerts.yml` under group `aether.rules`.
2. Include `summary`, `description`, and `runbook_url` annotations. Use `{{ $labels.job }}` and `{{ $value }}` for context.
3. Validate: `docker run --rm -v "$PWD/deploy/docker/prometheus:/p" prom/prometheus:latest promtool check rules /p/alerts.yml`
4. Reload Prometheus: `curl -XPOST http://localhost:9091/-/reload`

## Adding a Receiver

1. Edit `deploy/docker/alertmanager.yml`.
2. Alerting is Slack-only in production. PagerDuty/Discord/Telegram receivers are intentionally out of scope — propose via a separate design ticket if the team decides to broaden channels.
3. The Slack webhook is injected at container start via sed substitution of `__SLACK_WEBHOOK_URL__` from `$SLACK_WEBHOOK_URL` env.
4. An optional richer Slack message template lives at `deploy/docker/alertmanager/templates/slack.tmpl` — wiring it requires adding a `templates:` stanza to `alertmanager.yml` and mounting the directory in docker-compose. Deferred as a follow-up.

## Histogram Bucket Caveats

Quantile estimates are bounded by the top histogram bucket. If p99 reads as the top-bucket value, it means most observations exceed that boundary — not that the exact value equals it. Configured bucket tops:

- Detection latency: 50ms (`aether_detection_latency_ms`)
- Simulation latency: 500ms (`aether_simulation_latency_ms`)
- End-to-end latency: 5000ms (`aether_end_to_end_latency_ms`)

Add finer or higher buckets in the metric definition to get better resolution.

## Running the Smoke Test

```bash
bash scripts/monitoring_smoke.sh
```

Brings up the monitoring stack (prometheus, alertmanager, grafana), waits for readiness, and asserts:

- every expected alert rule was loaded by Prometheus, with `summary`, `description`, `runbook_url`, and `severity` populated;
- both `aether-go` and `aether-rust` scrape jobs are discovered;
- Alertmanager accepted its config (slack-default receiver resolved);
- every expected Grafana dashboard UID is provisioned.

The script tears the stack down on exit. Requires `docker`, `curl`, `jq`.

Synthetic rule injection (`docker cp` + `/-/reload`) is deliberately avoided — main's `prometheus.yml` pins `rule_files` to an explicit path and the compose stack does not pass `--web.enable-lifecycle`, so that approach cannot function without modifying files owned by other workstreams. Asserting rule loadedness via `/api/v1/rules` gives deterministic coverage within this PR's scope.
