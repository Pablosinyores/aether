# Mempool observability ledger runbook

How to stand up and query the public-mempool **prediction → reconciliation →
profitability** ledger: the loop that records every pending-tx swap the engine
decodes + analytically simulates, checks whether each one landed where we
predicted, and scores what our analytical arb cycle would have *actually*
realised — all in Postgres, **without ever submitting a bundle**.

This is observability only. It is independent of the trade ledger
(`DATABASE_URL`) and of the mempool-backrun *execution* path (see
[`mempool-backrun-rollout.md`](./mempool-backrun-rollout.md) for that).

---

## What gets persisted

Three tables, written by three independent processes, joined 1:1 on
`prediction_id`:

| Table | Migration | Writer | Answers |
|---|---|---|---|
| `mempool_predictions` | `0003` | Rust engine (`aether-rust`) | What did we predict? (post-state, profit factor, target block, pool) |
| `mempool_reconciliation` | `0004` | Go reconciler (`aether-reconciler`) | Did it land where/how we said? (`outcome`, `block_delta`, `pool_path_correct`) |
| `mempool_profitability` | `0005` | Rust scorer (`aether-profit-scorer`) | Would we have made money? (`realized_profit_wei`, `net_profit_wei`, `decision`) |

The mempool tables carry no foreign key into the trade-ledger tables, so one
Postgres database can host both ledgers. `MEMPOOL_LEDGER_DSN` may equal
`DATABASE_URL`.

---

## Persistence gates

The engine writes prediction rows **only** when all three of these are set.
Each gate is independent; missing any one means no rows.

| Env var | Controls | If unset / fails |
|---|---|---|
| `MEMPOOL_TRACKING=1` | Spawns the pending-tx subscription + decode pipeline | No predictions computed at all |
| `MEMPOOL_WS_URL` (falls back to `ETH_RPC_URL`) | WS endpoint for pending txs | Tracking enabled but skipped (logged) |
| `MEMPOOL_LEDGER_DSN` | Swaps `NoopMempoolSink` → `PgMempoolWriter` | Predictions computed but **silently dropped** |

> **No-op on DB failure.** If `MEMPOOL_LEDGER_DSN` is set but the connection
> fails at boot, the writer logs a warning and falls back to the no-op sink —
> it does **not** crash or buffer. If rows are not appearing, grep the engine
> log for `MEMPOOL_LEDGER_DSN unset` / `connect failed`.

---

## Stand it up

All commands assume you are on `develop` (or a branch cut from it). Replace
`<key>` with an Ethereum WS provider key. The DSN below uses the bundled
Postgres on `:5432`; keep the port consistent across every process.

> **Port gotcha.** Some in-code example DSNs use `:5433`
> (`cmd/reconciler/main.go`, scorer docs), but the bundled compose service
> exposes `:5432`. Pick one port and use it everywhere.

### 1. Start Postgres (gated behind the `ledger` compose profile)

```bash
docker compose -f deploy/docker/docker-compose.yml --profile ledger up -d postgres
# postgres:17 — db / user / password all default to "aether", mapped to localhost:5432
```

### 2. Apply migrations

`scripts/db_migrate.sh` runs every file in `migrations/` against `$DATABASE_URL`
via `sqlx-cli`. Point it at the same database the mempool ledger will use; all
five tables (trade ledger + mempool) are created together and coexist safely.

```bash
cargo install sqlx-cli --no-default-features --features postgres,native-tls   # once
export DATABASE_URL=postgres://aether:aether@localhost:5432/aether
./scripts/db_migrate.sh
```

### 3. Run the engine — writes `mempool_predictions`

```bash
MEMPOOL_TRACKING=1 \
MEMPOOL_WS_URL=wss://eth-mainnet.g.alchemy.com/v2/<key> \
MEMPOOL_LEDGER_DSN=postgres://aether:aether@localhost:5432/aether \
cargo run --release --bin aether-rust
```

### 4. Run the reconciler — writes `mempool_reconciliation`

Polls confirmed blocks and resolves each prediction (`confirmed` / `dropped` /
`replaced`). Batches receipt RPCs — 200 txs with one prediction hit cost one
receipt call, not 200.

```bash
go build -o aether-reconciler ./cmd/reconciler
MEMPOOL_LEDGER_DSN=postgres://aether:aether@localhost:5432/aether \
ETH_RPC_URL=wss://eth-mainnet.g.alchemy.com/v2/<key> \
RECONCILER_METRICS_ADDR=:9094 \
./aether-reconciler
```

### 5. Run the profit scorer — writes `mempool_profitability`

Polls every 30 s for `confirmed` predictions with no profitability row yet,
re-runs Bellman-Ford + the ternary-search optimiser against the affected
pool's reserves at `actual_target_block`, and records realized P&L. Needs
`config/pools.toml` for the reference graph (refreshed every 5 min).

```bash
MEMPOOL_LEDGER_DSN=postgres://aether:aether@localhost:5432/aether \
ETH_RPC_URL=wss://eth-mainnet.g.alchemy.com/v2/<key> \
PROFIT_SCORER_METRICS_ADDR=:9095 \
AETHER_GIT_SHA=$(git rev-parse --short HEAD) \
cargo run --release --bin aether-profit-scorer
```

---

## Check on it anytime

### Grafana

The reconciler + scorer panels auto-provision from
`deploy/docker/grafana/dashboards/mempool.json` when the monitoring stack is
up. Reconciler metrics on `:9094`, scorer on `:9095`.

### SQL

```sql
-- 1. Did we make money? Headline answer over the soak window.
SELECT decision, count(*), sum(net_profit_wei) / 1e18 AS net_eth
FROM mempool_profitability
GROUP BY decision;

-- 2. Sim correctness: where do predictions go wrong, and by how many blocks?
SELECT outcome,
       count(*),
       avg(block_delta)                  AS avg_block_delta,
       avg((pool_path_correct)::int)     AS pool_hit_rate
FROM mempool_reconciliation
GROUP BY outcome;

-- 3. Predicted-vs-realized error, most recent first.
SELECT p.pending_tx_hash,
       p.protocol,
       p.profit_factor_predicted,
       f.realized_profit_eth,
       f.decision,
       r.outcome,
       r.block_delta
FROM mempool_predictions p
JOIN mempool_reconciliation  r USING (prediction_id)
LEFT JOIN mempool_profitability f USING (prediction_id)
ORDER BY p.decoded_at DESC
LIMIT 100;
```

---

## Known limitations

Read these before trusting the numbers:

- **`ordering_correct` is always NULL.** The engine predicts `target_block`
  only, not tx index within the block (`0004_mempool_reconciliation.sql`).
  Intra-block ordering correctness is unmeasured until predicted-index lands.
- **The scorer's graph is an approximation.** Only the *affected* pool's
  reserves are fetched at `actual_target_block`; the rest of the graph
  reflects the latest fetched reserves, not the historical block. Borderline
  cycles can land in `decision='unprofitable'` falsely. A full per-block fetch
  (≈76 RPC calls/scoring) is deferred future work.
- **`decision='reverted'` is reserved, not emitted.** The v1 scorer is purely
  analytical; the revm-fork-verify path that would populate it is a planned
  follow-up.

---

## Reference

- Prediction writer: `crates/grpc-server/src/mempool_writer.rs`, pipeline in
  `crates/grpc-server/src/mempool_pipeline.rs`, decode in
  `crates/ingestion/src/mempool.rs`
- Reconciler: `cmd/reconciler/main.go`, `internal/db/mempool_reconciliation_pg.go`
- Scorer: `crates/grpc-server/src/bin/aether_profit_scorer.rs`,
  `crates/grpc-server/src/profitability_writer.rs`
- Schemas: `migrations/0003_mempool_predictions.sql`,
  `migrations/0004_mempool_reconciliation.sql`,
  `migrations/0005_mempool_profitability.sql`
- Migration runner: `scripts/db_migrate.sh`
- Dashboard: `deploy/docker/grafana/dashboards/mempool.json`
