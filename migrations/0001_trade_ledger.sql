-- Trade ledger — initial schema.
--
-- Owns the persistent record of every arb the engine evaluates and every
-- bundle the executor builds. Designed to survive process restarts so we can
-- answer "what did the bot do this week" in SQL instead of grepping logs.
--
-- U256 economics → NUMERIC(78,0)  (max 2^256 has 78 digits).
-- Variable-shape per-arb data    → JSONB (path, protocols, pool addresses).
-- All timestamps                 → TIMESTAMPTZ.
--
-- Clock-authority policy:
--   * Event-time columns (arbs.ts, bundles.submitted_at,
--     inclusion_results.resolved_at) are CLIENT-SET. Writers MUST populate
--     the value at the moment the event occurs in code — DEFAULT now()
--     exists only as a safety net for ad-hoc inserts via psql / migrations
--     and must not be relied on by application paths. Reasoning: the gap
--     between "event happened" and "row hit Postgres" can be tens of
--     milliseconds under load; trusting DB-time skews latency analysis and
--     back-tests.
--   * Persistence-boundary columns (pool_registry.first_seen / last_seen,
--     pnl_daily.updated_at) are DB-SET. They record when the row landed,
--     not when the event happened, so DB time is the right authority.
--
-- See docs/architecture.md and CLAUDE.md "Trade ledger" for context.

CREATE TABLE IF NOT EXISTS arbs (
    arb_id          UUID PRIMARY KEY,
    ts              TIMESTAMPTZ      NOT NULL DEFAULT now(),
    target_block    BIGINT           NOT NULL,
    path_hash       BYTEA            NOT NULL,
    hops            SMALLINT         NOT NULL,
    path            JSONB            NOT NULL,
    protocols       JSONB            NOT NULL,
    pool_addresses  JSONB            NOT NULL,
    flashloan_token BYTEA            NOT NULL,
    flashloan_amount NUMERIC(78,0)   NOT NULL,
    gross_profit_wei NUMERIC(78,0)   NOT NULL,
    net_profit_wei  NUMERIC(78,0)    NOT NULL,
    gas_estimate    BIGINT           NOT NULL,
    tip_bps         INTEGER          NOT NULL,
    detection_us    BIGINT,
    sim_us          BIGINT,
    git_sha         TEXT
);

CREATE INDEX IF NOT EXISTS arbs_target_block_idx ON arbs (target_block);
CREATE INDEX IF NOT EXISTS arbs_ts_desc_idx      ON arbs (ts DESC);
CREATE INDEX IF NOT EXISTS arbs_path_hash_idx    ON arbs (path_hash);

CREATE TABLE IF NOT EXISTS bundles (
    bundle_id       UUID PRIMARY KEY,
    arb_id          UUID             NOT NULL REFERENCES arbs(arb_id) ON DELETE CASCADE,
    submitted_at    TIMESTAMPTZ      NOT NULL DEFAULT now(),
    target_block    BIGINT           NOT NULL,
    signed_tx_hex   TEXT             NOT NULL,
    gas_used        BIGINT,
    is_shadow       BOOLEAN          NOT NULL DEFAULT FALSE,
    builders        JSONB            NOT NULL
);

CREATE INDEX IF NOT EXISTS bundles_arb_id_idx       ON bundles (arb_id);
CREATE INDEX IF NOT EXISTS bundles_submitted_at_idx ON bundles (submitted_at DESC);

CREATE TABLE IF NOT EXISTS inclusion_results (
    bundle_id       UUID             NOT NULL REFERENCES bundles(bundle_id) ON DELETE CASCADE,
    builder         TEXT             NOT NULL,
    included        BOOLEAN          NOT NULL,
    included_block  BIGINT,
    landed_tx_hash  BYTEA,
    error           TEXT,
    resolved_at     TIMESTAMPTZ      NOT NULL DEFAULT now(),
    PRIMARY KEY (bundle_id, builder)
);

CREATE INDEX IF NOT EXISTS inclusion_results_bundle_id_idx     ON inclusion_results (bundle_id);
CREATE INDEX IF NOT EXISTS inclusion_results_included_block_idx ON inclusion_results (included_block) WHERE included;

CREATE TABLE IF NOT EXISTS pool_registry (
    address         BYTEA PRIMARY KEY,
    protocol        TEXT             NOT NULL,
    token0          BYTEA            NOT NULL,
    token1          BYTEA            NOT NULL,
    fee_bps         INTEGER,
    tier            TEXT,
    first_seen      TIMESTAMPTZ      NOT NULL DEFAULT now(),
    last_seen       TIMESTAMPTZ      NOT NULL DEFAULT now(),
    source          TEXT             NOT NULL
);

CREATE INDEX IF NOT EXISTS pool_registry_protocol_idx ON pool_registry (protocol);
CREATE INDEX IF NOT EXISTS pool_registry_pair_idx     ON pool_registry (token0, token1);

CREATE TABLE IF NOT EXISTS pnl_daily (
    day             DATE PRIMARY KEY,
    realized_profit_wei NUMERIC(78,0) NOT NULL DEFAULT 0,
    gas_spent_wei   NUMERIC(78,0)    NOT NULL DEFAULT 0,
    bundle_count    BIGINT           NOT NULL DEFAULT 0,
    inclusion_count BIGINT           NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ      NOT NULL DEFAULT now()
);
