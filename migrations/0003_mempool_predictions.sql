-- Mempool predictions — public-flow observability ledger.
--
-- Records every pending-tx swap the engine decoded + analytically simulated,
-- so a follow-up reconciler (issue #131 Go half) can compare against
-- confirmed blocks and answer "did the tx land where we predicted, in the
-- order we predicted, hitting the pool we predicted?" — all in SQL.
--
-- Independent of `arbs` / `bundles`: this side never submits, so there is no
-- foreign key into the trade-ledger tables. The two ledgers can be enabled
-- separately via distinct DSNs (DATABASE_URL = trade ledger,
-- MEMPOOL_LEDGER_DSN = mempool ledger), so an operator can run mempool
-- observability without provisioning the executor schema and vice versa.
--
-- U256 economics       → NUMERIC(78,0)  (max 2^256 has 78 digits).
-- Variable-shape state → JSONB (V3 sqrt + tick; Curve A + balances;
--                                Balancer balances + weights).
-- All timestamps       → TIMESTAMPTZ.
--
-- Clock-authority policy (matches 0001_trade_ledger.sql):
--   * `decoded_at` is CLIENT-SET. Writers MUST populate it at the moment
--     the pending-tx event lands in the decoder; the `DEFAULT now()`
--     fallback exists only for ad-hoc inserts and must not be relied on by
--     application paths. The gap between "tx hit the mempool subscription"
--     and "row landed in Postgres" can be tens of ms under load; trusting
--     DB time would skew the detection-lead-vs-confirmation analysis the
--     follow-up reconciler builds on.
--
-- See issue #131 for the broader observability plan, and CLAUDE.md for the
-- 7-layer architecture context.

CREATE TABLE IF NOT EXISTS mempool_predictions (
    prediction_id              UUID PRIMARY KEY,
    -- Client-set; instant of decode in the Rust pipeline.
    decoded_at                 TIMESTAMPTZ      NOT NULL DEFAULT now(),
    -- 32-byte tx hash. UNIQUE so a re-broadcast of the same pending tx
    -- (Alchemy WS can replay on reconnect) does not insert a duplicate
    -- prediction row; the writer uses ON CONFLICT DO NOTHING.
    pending_tx_hash            BYTEA            NOT NULL UNIQUE,
    router_address             BYTEA            NOT NULL,
    -- Wire label matches the decoder's `Protocol` debug rendering:
    -- uni_v2 / sushi / uni_v3 / curve / balancer. Bound to TEXT (not an
    -- enum type) so adding a new decoded protocol does not require a
    -- migration; values are validated at the Rust boundary.
    protocol                   TEXT             NOT NULL,
    token_in                   BYTEA            NOT NULL,
    token_out                  BYTEA            NOT NULL,
    amount_in                  NUMERIC(78,0)    NOT NULL,
    -- NULL when the (token_in, token_out, protocol) triple missed the live
    -- pool registry — the pre-sim filter drops these before the writer is
    -- called, but the column stays nullable so a future "filtered" code
    -- path can emit a stub row for completeness.
    pool_address               BYTEA,
    -- current_head + 1 at decode time; the reconciler compares against
    -- the actual landed block to produce `block_delta`.
    predicted_target_block     BIGINT           NOT NULL,
    -- Shape varies by protocol:
    --   uni_v2 / sushi   → {"reserve_in": "..", "reserve_out": ".."}
    --   uni_v3            → {"sqrt_price_x96_post": "..", "tick_post": ..}
    --   balancer          → {"balance_in": "..", "balance_out": ".."}
    --   curve             → {"balances_post": ["..", ".."], "amp": ".."}
    predicted_post_state       JSONB            NOT NULL,
    -- Populated when the post-state Bellman-Ford scan surfaced a profitable
    -- cycle; NULL when the scan ran but found nothing.  A NOT NULL profit
    -- factor is the SQL signal "we would have considered acting on this".
    profit_factor_predicted    DOUBLE PRECISION,
    -- Engine-side measurement of how far ahead of confirmation we saw the
    -- pending tx (decoded_at - earliest builder-side timestamp). NULL when
    -- the builder timestamp is unknown (Alchemy WS doesn't expose one
    -- today; reserved for the MEV-Share SSE path).
    detection_lead_ms          BIGINT,
    engine_git_sha             TEXT
);

CREATE INDEX IF NOT EXISTS mempool_predictions_target_block_idx ON mempool_predictions (predicted_target_block);
CREATE INDEX IF NOT EXISTS mempool_predictions_decoded_at_idx   ON mempool_predictions (decoded_at DESC);

-- mempool_reconciliation lands in PR-2 (issue #131 Go half). Defining it
-- here in a separate migration would couple the two PRs; the reconciler
-- ships its own 0004_mempool_reconciliation.sql.
