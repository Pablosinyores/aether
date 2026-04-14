# Metrics Reference

Complete reference for all Prometheus metrics exposed by Aether on `:9090/metrics`.

## Counters

| Metric | Description | Labels |
|---|---|---|
| `aether_opportunities_detected_total` | Total arbitrage opportunities detected | — |
| `aether_bundles_submitted_total` | Total bundles submitted to builders | `builder` |
| `aether_bundles_included_total` | Total bundles included on-chain | `builder` |
| `aether_bundles_reverted_total` | Total bundles that reverted on-chain | `reason` |
| `aether_events_processed_total` | Total events decoded and processed | `event_type` |
| `aether_simulations_total` | Total EVM simulations executed | `result` (success/revert/halt) |

## Histograms

| Metric | Description | Buckets |
|---|---|---|
| `aether_detection_latency_ms` | Time from event to opportunity detection | 0.5, 1, 2, 3, 5, 10, 20 |
| `aether_simulation_latency_ms` | Time for EVM simulation | 1, 2, 5, 10, 20, 50, 100 |
| `aether_end_to_end_latency_ms` | Full pipeline: event to bundle submission | 5, 10, 15, 20, 50, 100, 200 |
| `aether_submission_latency_ms` | Time to submit bundle to builders | 10, 50, 100, 500, 1000, 2000 |

## Gauges

| Metric | Description | Unit |
|---|---|---|
| `aether_gas_price_gwei` | Current gas price | gwei |
| `aether_daily_pnl_eth` | Daily profit/loss | ETH |
| `aether_eth_balance` | Searcher wallet balance | ETH |
| `aether_pools_monitored` | Number of actively monitored pools | count |
| `aether_node_healthy_count` | Number of healthy node connections | count |
| `aether_system_state` | Current system state (1=Running, 2=Degraded, 3=Paused, 4=Halted) | enum |

## Alert Thresholds

| Metric | Condition | Severity | Action |
|---|---|---|---|
| `aether_opportunities_detected_total` | rate <10/min | Warn | Check node connectivity |
| `aether_bundles_included_total` | inclusion rate <20% | Alert | Check builder endpoints |
| `aether_detection_latency_ms` | p99 >10ms | Warn | Check CPU, graph size |
| `aether_simulation_latency_ms` | p99 >50ms | Warn | Check RPC latency |
| `aether_end_to_end_latency_ms` | p99 >100ms | Alert | Check all components |
| `aether_gas_price_gwei` | >300 | SEV2 | System auto-halts |
| `aether_daily_pnl_eth` | <-0.5 | SEV2 | System auto-halts |
| `aether_eth_balance` | <0.1 | SEV2 | System auto-halts |

## Common PromQL Queries

### Detection Rate

```txt
# Opportunities per minute (5min window)
rate(aether_opportunities_detected_total[5m]) * 60
```

### Bundle Inclusion Rate

```txt
# Inclusion percentage
rate(aether_bundles_included_total[5m]) / rate(aether_bundles_submitted_total[5m]) * 100
```

### Latency Percentiles

```txt
# Detection p50
histogram_quantile(0.5, rate(aether_detection_latency_ms_bucket[5m]))

# Detection p99
histogram_quantile(0.99, rate(aether_detection_latency_ms_bucket[5m]))

# End-to-end p99
histogram_quantile(0.99, rate(aether_end_to_end_latency_ms_bucket[5m]))
```

### Profitability

```txt
# Daily PnL
aether_daily_pnl_eth

# Gas cost trend
aether_gas_price_gwei
```

### Per-Builder Performance

```txt
# Submissions per builder
rate(aether_bundles_submitted_total[5m]) by (builder)

# Inclusion rate per builder
rate(aether_bundles_included_total[5m]) / rate(aether_bundles_submitted_total[5m]) by (builder)
```
