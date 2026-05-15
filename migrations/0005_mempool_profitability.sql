-- Mempool profitability — realized P&L per reconciled prediction.
--
-- Joins one-to-one with `mempool_predictions` (and indirectly through
-- `mempool_reconciliation`): for every prediction whose outcome was
-- `confirmed`, the scorer writes one row here with what our analytical
-- arb cycle would have *actually* realized against the post-state of
-- the pool at the block where the victim swap landed.
--
-- The point is to learn whether the predictor is finding *profitable*
-- real-world opportunities — without ever submitting a bundle. The
-- headline answer is SUM(net_profit_wei) WHERE decision='profitable'
-- over the soak window.
--
-- Outcome lifecycle (`decision`):
--   profitable    - Bellman-Ford on the actual-block graph found a
--                   cycle whose gross profit exceeds the gas estimate.
--                   `realized_profit_*` and `net_profit_wei` are positive.
--   unprofitable  - Cycle was found but gross profit < gas estimate.
--                   `net_profit_wei` is negative; `realized_profit_*`
--                   may still be non-zero (gross can be positive while
--                   net is negative).
--   reverted      - Reserved for the revm-fork-verify path (planned
--                   follow-up). The detector found a cycle but a
--                   forked-EVM replay would have reverted. Not emitted
--                   by the v1 scorer; the column carries the value for
--                   forward compatibility.
--   no_path       - Bellman-Ford on the actual-block graph found no
--                   negative cycle through the affected pool. The
--                   analytical predictor surfaced a path at decode
--                   time but the real-block post-state had no path.
--
-- Clock-authority policy matches 0001_trade_ledger.sql / 0003 / 0004:
--   * `scored_at` is CLIENT-SET. Writers MUST populate it the moment
--     the scoring is computed in code; the `DEFAULT now()` is a psql
--     safety net only.
--
-- Cascade FK to `mempool_predictions(prediction_id)` so a re-soak that
-- truncates predictions also clears the profitability rows.

CREATE TABLE IF NOT EXISTS mempool_profitability (
    prediction_id          UUID PRIMARY KEY
        REFERENCES mempool_predictions(prediction_id) ON DELETE CASCADE,
    -- Client-set; instant the scorer finished computing this row.
    scored_at              TIMESTAMPTZ      NOT NULL DEFAULT now(),
    -- JSONB-encoded cycle as a list of {pool_address, token_in,
    -- token_out, protocol} hops. Variable length so the schema
    -- accommodates 2-hop and longer cycles without a separate table.
    cycle_path             JSONB            NOT NULL,
    -- Gross profit from replaying the cycle against the actual-block
    -- reserves. NUMERIC(78,0) keeps U256 economics lossless.
    realized_profit_wei    NUMERIC(78,0)    NOT NULL,
    -- Convenience copy in ETH units. 38 digits + 18 decimals fits
    -- 1e20 ETH which is more than the total supply, so overflow is
    -- impossible. NUMERIC(38,18) is precise; DOUBLE PRECISION would
    -- lose lower digits.
    realized_profit_eth    NUMERIC(38,18)   NOT NULL,
    -- Gas estimate in wei (gas_units × gas_price_wei). Both factors
    -- come from the existing per-protocol gas model + the chain's
    -- current base fee at scoring time. Stored separately from
    -- `realized_profit_wei` so the scorer can be re-run with a
    -- different gas model without losing the gross signal.
    gas_estimate_wei       NUMERIC(78,0)    NOT NULL,
    -- realized_profit_wei - gas_estimate_wei. May be negative.
    -- Sign on this column is the SQL signal "would we have made
    -- money" — the headline answer the dashboard exposes.
    net_profit_wei         NUMERIC(78,0)    NOT NULL,
    decision               TEXT             NOT NULL
        CHECK (decision IN ('profitable','unprofitable','reverted','no_path')),
    scoring_engine_git_sha TEXT
);

CREATE INDEX IF NOT EXISTS mempool_profitability_decision_idx
    ON mempool_profitability (decision);
CREATE INDEX IF NOT EXISTS mempool_profitability_scored_at_idx
    ON mempool_profitability (scored_at DESC);
