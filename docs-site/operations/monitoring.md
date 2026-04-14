# Monitoring

Aether exposes comprehensive metrics via Prometheus and provides built-in alerting through PagerDuty, Telegram, and Discord.

## Prometheus Setup

Metrics are exposed by the Go monitor service on `:9090/metrics`. Prometheus scrapes this endpoint at a 15-second interval.

### Prometheus Configuration

The included Prometheus config is at `deploy/docker/prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'aether'
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

### Counters

| Metric | Description | Alert Threshold |
|---|---|---|
| `aether_opportunities_detected_total` | Arbitrage opportunities found | <10/min → warn |
| `aether_bundles_submitted_total` | Bundles sent to builders | — |
| `aether_bundles_included_total` | Bundles included on-chain | Inclusion rate <20% → alert |

### Histograms

| Metric | Description | Alert Threshold |
|---|---|---|
| `aether_detection_latency_ms` | Detection pipeline latency | p99 >10ms → warn |
| `aether_simulation_latency_ms` | EVM simulation latency | p99 >50ms → warn |
| `aether_end_to_end_latency_ms` | Full pipeline latency | p99 >100ms → alert |

### Gauges

| Metric | Description | Alert Threshold |
|---|---|---|
| `aether_gas_price_gwei` | Current gas price | >300 → halt |
| `aether_daily_pnl_eth` | Daily profit/loss in ETH | <-0.5 ETH → halt |
| `aether_eth_balance` | Searcher wallet balance | <0.1 ETH → halt |
| `aether_pools_monitored` | Active pool count | — |
| `aether_node_healthy_count` | Healthy node connections | <2 → degrade |

## PromQL Queries

### Opportunity Detection Rate

```txt
rate(aether_opportunities_detected_total[5m])
```

### Bundle Inclusion Rate

```txt
rate(aether_bundles_included_total[5m]) / rate(aether_bundles_submitted_total[5m])
```

### Detection Latency (p50, p99)

```txt
# p50
histogram_quantile(0.5, rate(aether_detection_latency_ms_bucket[5m]))

# p99
histogram_quantile(0.99, rate(aether_detection_latency_ms_bucket[5m]))
```

### End-to-End Latency p99

```txt
histogram_quantile(0.99, rate(aether_end_to_end_latency_ms_bucket[5m]))
```

### Daily PnL

```txt
aether_daily_pnl_eth
```

### Gas Price Trend

```txt
aether_gas_price_gwei
```

## Grafana Dashboards

Grafana connects to Prometheus as a data source. Recommended dashboard panels:

1. **System Overview** — Current state (Running/Degraded/Paused/Halted), uptime, active pools
2. **Latency** — Detection, simulation, and end-to-end latency percentiles over time
3. **Profitability** — Daily PnL, cumulative PnL, profit per trade
4. **Bundles** — Submission rate, inclusion rate, miss rate
5. **Gas** — Gas price trend, gas cost per trade
6. **Infrastructure** — ETH balance, node health, CPU/memory usage

## Alerting

### Alert Rules

Alerts are triggered by the Go monitor service based on metric thresholds:

| Condition | Severity | Action |
|---|---|---|
| Gas price >300 gwei | SEV2 | System auto-halts |
| 10+ consecutive reverts in 10 min | SEV3 | System auto-pauses |
| Daily loss >0.5 ETH | SEV2 | System auto-halts |
| ETH balance <0.1 ETH | SEV2 | System auto-halts |
| Node latency >500ms | SEV3 | System degrades |
| Bundle miss rate >80% in 1h | SEV3 | Alert dispatched |
| Detection latency p99 >10ms | SEV3 | Alert dispatched |
| Opportunities <10/min | SEV3 | Alert dispatched |

### Alert Channels

| Channel | Used For | Configuration |
|---|---|---|
| PagerDuty | SEV1, SEV2 | `config/risk.yaml → alerting.pagerduty` |
| Telegram | SEV2, SEV3 | `config/risk.yaml → alerting.telegram` |
| Discord | All severities | `config/risk.yaml → alerting.discord` |

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

The built-in HTTP dashboard runs on `:8080`:

```bash
# Check dashboard is up
curl -s http://localhost:8080/ | head -5
```

Provides at-a-glance view of:
- System state and health
- Recent opportunities and executions
- Key performance metrics
- Circuit breaker status
