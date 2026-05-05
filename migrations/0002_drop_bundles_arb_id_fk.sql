-- Trade ledger — drop bundles.arb_id → arbs(arb_id) foreign key.
--
-- Why: the Rust engine writes `arbs` rows and the Go executor writes
-- `bundles` rows independently, both fire-and-forget through their own
-- bounded mpsc → writer task. There is no cross-process ordering guarantee
-- between "ARB PUBLISHED → insert_arb" on the Rust side and "bundle signed
-- + sent → insert_bundle" on the Go side; under load the Go bundle insert
-- can land at Postgres before the Rust arb insert, and the FK fires
-- immediately on row INSERT (Postgres FK checks are not deferred by
-- default). Result: a measurable fraction of bundle rows would be dropped
-- with `aether_ledger_writes_total{op="insert_bundle",result="err"}` on
-- every busy block, masking real ledger health.
--
-- Trade-off: an `arbs` row may briefly fail to land (Rust connection
-- blip), leaving an orphan bundle. Acceptable because:
--   - both sides drop on failure with a counter, so orphans surface as
--     `aether_ledger_writes_total{op="insert_arb",result="err"}` anyway,
--   - downstream queries already do LEFT JOIN arbs ↔ bundles when both
--     sides are persisted, NULL on the arb side is informative.
--
-- Future: re-add the FK once a coordinator (e.g. Rust writes first and
-- signals Go via gRPC ack) provides ordering, or add a backfilling
-- reconciliation worker that re-runs the missing arb inserts.
--
-- The constraint is dropped IF EXISTS so the migration is idempotent on a
-- partially-applied database.

ALTER TABLE bundles
    DROP CONSTRAINT IF EXISTS bundles_arb_id_fkey;
