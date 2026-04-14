# Incident Response

Playbooks for handling incidents affecting the Aether system.

## Severity Levels

| Level | Description | Response Time | Examples |
|---|---|---|---|
| **SEV1** | System halted, active loss of funds | Immediate | Private key compromise, contract exploit, unexpected fund drain |
| **SEV2** | System halted, no loss | 15 min | Gas >300 gwei halt, balance <0.1 ETH, all nodes down |
| **SEV3** | Degraded performance | 1 hour | Single node down, high revert rate, latency spike |
| **SEV4** | Minor issue | Next business day | Dashboard down, log aggregation failure, non-critical alert |

## SEV1: Private Key Compromise

**Detection:** Unauthorized transactions from searcher wallet, unexpected balance changes.

::: danger Immediate Actions (within 5 minutes)
**1. HALT all services immediately**
```bash
sudo systemctl stop aether-go aether-rust
```

**2. Revoke contract permissions**
```bash
cast send <EXECUTOR_ADDRESS> "transferOwnership(address)" \
    0x0000000000000000000000000000000000000001 \
    --private-key <COLD_WALLET_KEY> --rpc-url <RPC_URL>
```

**3. Sweep remaining funds from searcher wallet**
```bash
cast send <COLD_WALLET> --value $(cast balance <SEARCHER_WALLET>) \
    --private-key <SEARCHER_KEY> --rpc-url <RPC_URL>
```

**4. Rescue tokens from executor contract**
```bash
cast send <EXECUTOR_ADDRESS> "rescue(address,uint256)" <TOKEN> <AMOUNT> \
    --private-key <COLD_WALLET_KEY> --rpc-url <RPC_URL>
```

**5. Block outbound network access**
```bash
sudo iptables -A OUTPUT -j DROP
sudo iptables -I OUTPUT -d 127.0.0.1 -j ACCEPT
```
:::

**Investigation:**
- Review all transactions from searcher wallet: `cast logs --from-block <BLOCK> --address <SEARCHER>`
- Check for unauthorized SSH access: `last -i`, `journalctl -u sshd`
- Review process list for unexpected processes: `ps aux`
- Check for modified binaries: `sha256sum /opt/aether/bin/*`

**Recovery:**
- Generate new searcher key pair
- Deploy new AetherExecutor contract
- Update all configuration with new addresses
- Rotate all API keys (builder endpoints, node providers)
- Restore from known-good backup

## SEV1: Contract Exploit

**Detection:** Unexpected token flows in/out of AetherExecutor, failed invariant checks.

::: danger Immediate Actions
1. Stop all services
2. Rescue tokens from contract via `rescue()` (cold wallet only)
3. Analyze the exploit transaction
4. Deploy patched contract
5. Update all references to new contract address
:::

## SEV2: All Ethereum Nodes Down

**Detection:** `aether_node_healthy_count` = 0, system enters HALTED state.

**Actions:**

1. Check node provider status pages (Alchemy, QuickNode, Infura)
2. Check local Reth/Geth node:
   ```bash
   curl -s -X POST -H "Content-Type: application/json" \
       -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
       http://localhost:8545
   ```
3. Try alternative endpoints:
   ```bash
   vim /opt/aether/config/nodes.yaml
   sudo systemctl restart aether-rust
   ```
4. If all providers are down, wait — likely a network-wide issue

## SEV2: Gas Price Sustained Above 300 gwei

**Detection:** `aether_gas_price_gwei` > 300 for >5 minutes, system auto-halts.

**Actions:**

1. This is **expected behavior** — the circuit breaker is protecting against unprofitable execution
2. Monitor gas prices: `curl -s https://api.etherscan.io/api?module=gastracker&action=gasoracle`
3. Resume when gas drops:
   ```bash
   grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
       -d '{"state": "RUNNING"}'
   ```
4. If gas is consistently high, consider adjusting threshold in `config/risk.yaml`

## SEV2: Daily Loss Exceeds 0.5 ETH

**Detection:** `aether_daily_pnl_eth` < -0.5, system auto-halts.

::: warning Do NOT immediately resume — investigate first
:::

**Actions:**

1. Check recent trades:
   ```bash
   psql -h localhost -U aether -d aether \
       -c "SELECT * FROM trades WHERE created_at > NOW() - INTERVAL '24 hours'
           ORDER BY created_at DESC;"
   ```
2. Analyze losing trades:
   - Were simulations showing profit that didn't materialize?
   - Is there a new MEV competitor front-running our bundles?
   - Did pool state change between detection and execution?
3. Check for systematic issues:
   ```bash
   journalctl -u aether-rust -u aether-go --since "24 hours ago" -p err
   ```
4. After root cause is identified and fixed:
   ```bash
   grpcurl -plaintext localhost:50051 aether.ControlService/SetState \
       -d '{"state": "RUNNING"}'
   ```

## SEV3: High Revert Rate

**Detection:** >3 consecutive reverts in 10 minutes, system enters PAUSED state.

**Actions:**

1. Check revert reasons:
   ```bash
   journalctl -u aether-rust --since "30 minutes ago" | grep -i "revert\|fail"
   ```
2. Common causes:
   - **Stale state** — Usually self-resolving after a few blocks
   - **MEV competition** — Check if competitors are extracting same opportunities
   - **Pool state drift** — Verify pool reserve data matches on-chain
3. System auto-resumes after 10-minute cooldown

## SEV3: Detection Latency Spike

**Detection:** `aether_detection_latency_ms` p99 > 10ms.

**Actions:**

1. Check CPU utilization:
   ```bash
   top -b -n1 | head -20
   mpstat -P 0-3 1 5  # Rust core CPUs
   ```
2. Check price graph size:
   ```bash
   curl -s http://localhost:9090/metrics | grep aether_pools_monitored
   ```
3. If graph is too large, prune cold pools:
   ```bash
   vim /opt/aether/config/pools.toml
   grpcurl -plaintext localhost:50051 aether.ControlService/ReloadConfig
   ```
4. Check for memory pressure:
   ```bash
   free -h
   cat /proc/$(pgrep aether-grpc)/status | grep -i "vmrss\|vmsize"
   ```

## SEV3: Single Node Provider Down

**Detection:** Node enters `Degraded` or `Failed` state in health checks.

**Actions:**

1. System continues operating with remaining healthy nodes (min 2 required)
2. Check provider status page
3. If extended outage, add replacement provider:
   ```bash
   vim /opt/aether/config/nodes.yaml
   sudo systemctl restart aether-rust
   ```

## Communication

### Alert Channels

| Channel | Used For | Configuration |
|---|---|---|
| PagerDuty | SEV1, SEV2 | `config/risk.yaml` → `alerting.pagerduty` |
| Telegram | SEV2, SEV3 | `config/risk.yaml` → `alerting.telegram` |
| Discord | All severities | `config/risk.yaml` → `alerting.discord` |

### Escalation Path

1. **Automated alert** fires via configured channels
2. **On-call engineer** acknowledges within response time SLA
3. **Follow playbook** for the specific incident type
4. **Post-incident:** Write brief post-mortem, update playbooks if needed

## Post-Incident Checklist

- [ ] Root cause identified
- [ ] Fix implemented and deployed
- [ ] Monitoring/alerting updated if gaps found
- [ ] Runbook/playbook updated with lessons learned
- [ ] Affected users/stakeholders notified
- [ ] Post-mortem document written (for SEV1/SEV2)
