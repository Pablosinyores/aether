-- Mempool reconciliation — close the loop on persisted predictions.
--
-- Joins one-to-one with `mempool_predictions` (issue #131 first half /
-- PR #133): for every prediction, the reconciler poll loop writes exactly
-- one row here once the outcome is known. The two tables together answer
-- "did the tx land where we said it would, in the order we said it would,
-- hitting the pool we said it would?" — entirely in SQL.
--
-- Outcome lifecycle:
--   confirmed     - tx landed in a block at or after predicted_target_block.
--                   `actual_target_block`, `actual_tx_index`, `block_delta`,
--                   `ordering_correct`, `pool_path_correct` are all populated.
--   dropped       - prediction is now older than head - 12 blocks and no
--                   matching tx has surfaced. Mirrors the Flashbots-side
--                   "12-block dropped" heuristic. Only `resolution_ts`
--                   carries meaning.
--   replaced     - a later pending tx from the same sender + nonce landed
--                   first (same-nonce replacement). `replaced_by_tx_hash`
--                   carries the replacement's hash; the other "actual"
--                   columns are NULL because the prediction itself never
--                   confirmed.
--   still_pending - reserved for the case where the reconciler shuts down
--                   with predictions still in-flight; the next start-up
--                   resumes from this state. Not emitted under steady-state
--                   operation.
--
-- Clock-authority policy matches 0001_trade_ledger.sql / 0003_mempool_predictions:
--   * `resolution_ts` is CLIENT-SET (writer populates it the moment the
--     resolution is computed in code; the `DEFAULT now()` is a psql-level
--     safety net only).
--
-- The FK to `mempool_predictions(prediction_id)` uses ON DELETE CASCADE so
-- truncating predictions for a re-soak also clears reconciliation. The
-- reverse direction is enforced by the writer (insert prediction first,
-- then reconciliation), not by a CHECK constraint, so the writer can
-- batch-insert reconciliations without locking against concurrent
-- prediction inserts.

CREATE TABLE IF NOT EXISTS mempool_reconciliation (
    prediction_id          UUID PRIMARY KEY
        REFERENCES mempool_predictions(prediction_id) ON DELETE CASCADE,
    -- Client-set; instant of resolution in the reconciler loop.
    resolution_ts          TIMESTAMPTZ NOT NULL DEFAULT now(),
    outcome                TEXT NOT NULL
        CHECK (outcome IN ('confirmed','dropped','replaced','still_pending')),
    -- NULL for `dropped` / `replaced` / `still_pending`. For `confirmed`:
    -- the block the prediction's pending tx actually landed in.
    actual_target_block    BIGINT,
    -- Position within `actual_target_block`. NULL when outcome ≠ confirmed.
    actual_tx_index        INTEGER,
    -- `actual_target_block - predicted_target_block`. Negative = landed
    -- earlier than predicted; positive = landed later. NULL when outcome
    -- ≠ confirmed.
    block_delta            INTEGER,
    -- The mempool predictor records `predicted_target_block` only — it does
    -- not predict tx ordering within the block. Until a predicted-index is
    -- recorded by the engine (future work), `ordering_correct` is left
    -- NULL on confirmed rows so the column stays a no-op rather than
    -- a misleading TRUE.
    ordering_correct       BOOLEAN,
    -- TRUE when the receipt's logs contain an entry whose `address` matches
    -- `mempool_predictions.pool_address`. FALSE when the receipt landed but
    -- no log touched the predicted pool (router routed elsewhere, or the
    -- predicted pool was wrong). NULL when outcome ≠ confirmed or
    -- `pool_address` was NULL on the prediction.
    pool_path_correct      BOOLEAN,
    replaced_by_tx_hash    BYTEA,
    -- Free-form reason for non-confirmed outcomes (e.g. the
    -- "12-block window elapsed" / "same-nonce replacement" labels the
    -- reconciler emits). NULL on `confirmed`.
    failure_reason         TEXT
);

CREATE INDEX IF NOT EXISTS mempool_reconciliation_actual_target_block_idx
    ON mempool_reconciliation (actual_target_block);
CREATE INDEX IF NOT EXISTS mempool_reconciliation_outcome_idx
    ON mempool_reconciliation (outcome);
