# Monitoring

Aether exposes metrics from two processes. The Rust engine serves histograms and counters on `:9092/metrics`. The Go executor serves metrics on `:9090/metrics` via the Prometheus client library. The Go monitor also serves simplified gauge-style metrics and an HTML dashboard.

## Prometheus Setup

::: warning Port Collision
Both the Go executor (`cmd/executor/`) and Go monitor (`cmd/monitor/`) default to `:9090` for their metrics server. If running both services on the same host, set `METRICS_PORT=9091` on one of them to avoid a `ListenAndServe` bind failure.
:::

Prometheus needs to scrape **both** endpoints:

```yaml
scrape_configs:
  - job_name: 'aether-rust'
    scrape_interval: 15s
    static_configs:
      - targets: ['localhost:9092']

  - job_name: 'aether-go'
    scrape_interval: 15s
    static_configs:
      - targets: ['localhost:9090']
```

### Running Prometheus

```bash
# Via Docker Compose
docker compose -f deploy/docker/docker-compose.yml up -d prometheus

# Prometheus UI
open http://localhost:9090
```

## Key Metrics

### From Rust Engine (`:9092`)

| Metric | Type | Description |
|---|---|---|
| `aether_detection_latency_ms` | Histogram | Bellman-Ford detection latency |
| `aether_simulation_latency_ms` | Histogram | EVM simulation latency |
| `aether_cycles_detected_total` | Counter | Negative cycles found |
| `aether_simulations_run_total` | Counter | EVM simulations executed |
| `aether_arbs_published_total` | Counter | Validated arbs sent to Go |
| `aether_blocks_processed_total` | Counter | Blocks processed |

### From Go Executor (`:9090`)

| Metric | Type | Description |
|---|---|---|
| `aether_executor_bundles_submitted_total` | Counter | Bundles submitted to builders |
| `aether_executor_bundles_included_total` | Counter | Bundles accepted by builders |
| `aether_executor_profit_wei_total` | Counter | Cumulative net profit (wei) |
| `aether_executor_gas_spent_wei_total` | Counter | Cumulative gas spent (wei) |
| `aether_executor_risk_rejections_total` | Counter | Arbs rejected by risk checks |
| `aether_end_to_end_latency_ms` | Histogram | Detection to submission latency |
| `aether_gas_price_gwei` | Gauge | Current gas price |
| `aether_daily_pnl_eth` | Gauge | Daily profit/loss in ETH |
| `aether_eth_balance` | Gauge | Searcher wallet balance |

### From Go Monitor (`:9090`)

| Metric | Type | Description |
|---|---|---|
| `aether_reverts_total{type="bug"}` | Counter | Bug-caused reverts |
| `aether_reverts_total{type="competitive"}` | Counter | MEV competition reverts |

See [Metrics Reference](/reference/metrics) for the complete list with bucket values and query examples.

## PromQL Queries

### Detection Latency (from Rust histograms on `:9092`)

```txt
# Average
rate(aether_detection_latency_ms_sum[5m]) / rate(aether_detection_latency_ms_count[5m])

# p99
histogram_quantile(0.99, rate(aether_detection_latency_ms_bucket[5m]))
```

### End-to-End Latency (from Go histogram on `:9090`)

```txt
# p99
histogram_quantile(0.99, rate(aether_end_to_end_latency_ms_bucket[5m]))
```

### Bundle Inclusion Rate

```txt
rate(aether_executor_bundles_included_total[5m]) / rate(aether_executor_bundles_submitted_total[5m]) * 100
```

### Daily PnL and Gas

```txt
aether_daily_pnl_eth
aether_gas_price_gwei
```

## Grafana Dashboards

Grafana connects to Prometheus as a data source. Recommended dashboard panels:

1. **System Overview** — Current state, uptime, blocks processed
2. **Latency** — Detection and simulation percentiles over time (source: Rust `:9092` histograms)
3. **Profitability** — Daily PnL, cumulative profit, gas costs
4. **Bundles** — Submission rate, inclusion rate, risk rejections
5. **Gas** — Gas price trend, gas cost per trade
6. **Infrastructure** — ETH balance, revert breakdown (bug vs competitive)

## Alerting

### Alert Rules

Alerts are triggered by the Go monitor/risk manager based on metric thresholds. See [Risk Parameters](/reference/risk-parameters) for the complete list.

| Condition | Severity | Action |
|---|---|---|
| Gas price >300 gwei | SEV2 | System auto-halts |
| 10+ bug reverts in 10 min | SEV3 | System auto-pauses |
| Daily loss >0.5 ETH | SEV2 | System auto-halts |
| ETH balance <0.1 ETH | SEV2 | System auto-halts |
| Node latency >500ms | SEV3 | System degrades |
| Bundle miss rate >80% in 1h | SEV3 | Alert dispatched |

### Alert Channels

All alerts route through a single **Slack webhook** with severity-based channel routing:

| Channel | Severity | Configuration |
|---|---|---|
| `#aether-alerts-sev1` | SEV1 | `config/risk.yaml` → `alerting.slack` |
| `#aether-alerts-sev2` | SEV2 | `config/risk.yaml` → `alerting.slack` |
| `#aether-alerts` | SEV3, SEV4 | `config/risk.yaml` → `alerting.slack` |

## Log Aggregation

Logs are written to journald and can be aggregated with Loki for centralized search.

### Useful Log Queries

```bash
# Recent errors from both services
journalctl -u aether-rust -u aether-go --since "1 hour ago" -p err

# Revert reasons
journalctl -u aether-rust --since "1 hour ago" | grep -i "revert"

# Bundle submission results
journalctl -u aether-go --since "1 hour ago" | grep "bundle"

# Node connection issues
journalctl -u aether-rust --since "1 hour ago" | grep -i "reconnect\|disconnect\|timeout"
```

## Dashboard

The built-in HTML dashboard runs on `:8080`, auto-refreshes every 5 seconds, and shows:

- Pipeline stats (blocks, cycles, simulations, arbs, bundles)
- Financials (daily PnL, gas price, ETH balance)
- Latency (detection, simulation, executor, total pipeline)

```bash
curl -s http://localhost:8080/ | head -5
```
