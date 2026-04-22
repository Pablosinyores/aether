# Metrics Reference

Aether exposes metrics from two processes. The Rust engine serves real Prometheus histograms and counters on `:9092/metrics`. The Go executor serves metrics via the Prometheus client library on `:9090/metrics`. The Go monitor service also exposes simplified gauge-style metrics on `:9090/metrics`.

## Rust Engine Metrics (`:9092`)

### Histograms

| Metric | Description | Buckets (ms) |
|---|---|---|
| `aether_detection_latency_ms` | Bellman-Ford detection latency | 0.1, 0.5, 1, 3, 5, 10, 50 |
| `aether_simulation_latency_ms` | EVM simulation latency | 0.5, 1, 2, 5, 10, 25, 50, 75, 100, 250, 500 |

These are real Prometheus histograms that emit `_bucket`, `_sum`, and `_count` series. You can compute percentiles and averages from them.

### Counters

| Metric | Description |
|---|---|
| `aether_cycles_detected_total` | Total negative cycles detected by Bellman-Ford |
| `aether_simulations_run_total` | Total EVM simulations executed |
| `aether_arbs_published_total` | Total validated arbs published to Go via gRPC |
| `aether_blocks_processed_total` | Total blocks processed |

::: warning Port Collision
Both the Go executor (`cmd/executor/`) and Go monitor (`cmd/monitor/`) default to `:9090` for their metrics server. If running both services on the same host, set `METRICS_PORT=9091` on one of them to avoid a `ListenAndServe` bind failure.
:::

## Go Executor Metrics (`:9090`)

### Counters

| Metric | Description |
|---|---|
| `aether_executor_bundles_submitted_total` | Bundles submitted for builder fan-out |
| `aether_executor_bundles_included_total` | Bundles with at least one builder acceptance |
| `aether_executor_profit_wei_total` | Cumulative estimated net profit (wei) |
| `aether_executor_gas_spent_wei_total` | Cumulative estimated gas spent (wei) |
| `aether_executor_risk_rejections_total` | Arbs rejected by preflight risk checks |

### Histogram

| Metric | Description | Buckets (ms) |
|---|---|---|
| `aether_end_to_end_latency_ms` | Arb detection to bundle submission | 10, 50, 75, 100, 250, 500, 1000, 2000, 5000 |

This is a real Prometheus histogram (emits `_bucket`, `_sum`, `_count`).

### Gauges

| Metric | Description | Unit |
|---|---|---|
| `aether_gas_price_gwei` | Current gas oracle base fee | gwei |
| `aether_daily_pnl_eth` | Daily profit minus gas costs (resets at UTC midnight) | ETH |
| `aether_eth_balance` | Searcher wallet balance | ETH |

## Go Monitor Metrics (`:9090`)

The monitor service also emits simplified metrics (stored as last-value gauges, not histograms):

| Metric | Prometheus Type | Description |
|---|---|---|
| `aether_opportunities_detected_total` | counter | Total arbitrage opportunities detected |
| `aether_bundles_submitted_total` | counter | Total bundles submitted |
| `aether_bundles_included_total` | counter | Total bundles included on-chain |
| `aether_reverts_total{type="bug"}` | counter | Reverts caused by bugs |
| `aether_reverts_total{type="competitive"}` | counter | Reverts from MEV competition |
| `aether_gas_price_gwei` | gauge | Current gas price |
| `aether_detection_latency_ms` | gauge | Last detection latency (not a histogram) |
| `aether_simulation_latency_ms` | gauge | Last simulation latency (not a histogram) |
| `aether_end_to_end_latency_ms` | gauge | Last end-to-end latency (not a histogram) |
| `aether_eth_balance` | gauge | Searcher wallet balance |

::: warning Monitor vs. Engine Metrics
The Rust engine emits `aether_detection_latency_ms` and `aether_simulation_latency_ms` as **real Prometheus histograms** (with `_bucket`, `_sum`, `_count`). The Go monitor emits metrics with the same names but as **gauges** storing only the last observed value. Use the Rust engine endpoint (`:9092`) for accurate percentile queries. The monitor gauges are for the dashboard display only.
:::

## Alert Thresholds

These are documented in [Risk Parameters](/reference/risk-parameters) and configured in `config/risk.yaml`. Key thresholds:

| Condition | Severity | Action |
|---|---|---|
| Gas price >300 gwei | SEV2 | System auto-halts |
| Daily loss >0.5 ETH | SEV2 | System auto-halts |
| ETH balance <0.1 ETH | SEV2 | System auto-halts |
| 10+ consecutive bug reverts in 10 min | SEV3 | System auto-pauses |
| Node latency >500ms | SEV3 | System degrades |
| Bundle miss rate >80% in 1h | SEV3 | Alert dispatched |
| Competitive revert rate >90% | SEV3 | Alert dispatched |

## PromQL Queries

### Detection Latency (Rust engine histograms)

```txt
# Average detection latency (from histogram sum/count)
rate(aether_detection_latency_ms_sum[5m]) / rate(aether_detection_latency_ms_count[5m])

# Detection latency p99
histogram_quantile(0.99, rate(aether_detection_latency_ms_bucket[5m]))
```

::: tip
These queries target the Rust engine's real histograms on `:9092`. They will return empty if pointed at the monitor's gauge-style metrics on `:9090`.
:::

### End-to-End Latency (Go executor histogram)

```txt
# Average end-to-end latency
rate(aether_end_to_end_latency_ms_sum[5m]) / rate(aether_end_to_end_latency_ms_count[5m])

# End-to-end latency p99
histogram_quantile(0.99, rate(aether_end_to_end_latency_ms_bucket[5m]))
```

### Bundle Inclusion Rate

```txt
rate(aether_executor_bundles_included_total[5m]) / rate(aether_executor_bundles_submitted_total[5m]) * 100
```

### Profitability

```txt
# Daily PnL
aether_daily_pnl_eth

# Gas price
aether_gas_price_gwei

# Cumulative profit
aether_executor_profit_wei_total
```

### Pipeline Throughput

```txt
# Blocks per minute
rate(aether_blocks_processed_total[5m]) * 60

# Cycles detected per minute
rate(aether_cycles_detected_total[5m]) * 60

# Arbs published per minute
rate(aether_arbs_published_total[5m]) * 60
```
