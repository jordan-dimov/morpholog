-- Kernel-computed read model for derived claims (NOT governed state).
--
-- `morpholog_read` holds the exact output of `enumerate_derived`,
-- materialised by `morpholog refresh derived` so BI tools can read
-- computed state in plain SQL. A discardable, refreshable, generation-
-- based projection - never read by the proposal kernel, invariant
-- evaluation, or value lookups. See crates/morpholog-core/sql/schema.sql
-- for the canonical definition; this migration adds it to a database
-- initialised before the derived read cache existed.

CREATE SCHEMA IF NOT EXISTS morpholog_read;

CREATE TABLE IF NOT EXISTS morpholog_read.derived_refreshes (
    refresh_id      uuid        PRIMARY KEY,
    model_hash      text        NOT NULL,
    refreshed_at    timestamptz NOT NULL,
    source_highwater_transition_id uuid,
    source_highwater_committed_at  timestamptz,
    derived_claim_count bigint   NOT NULL
);

CREATE TABLE IF NOT EXISTS morpholog_read.derived_active (
    singleton  boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    refresh_id uuid    NOT NULL REFERENCES morpholog_read.derived_refreshes (refresh_id)
);

CREATE TABLE IF NOT EXISTS morpholog_read.derived_claims (
    refresh_id     uuid  NOT NULL
                         REFERENCES morpholog_read.derived_refreshes (refresh_id)
                         ON DELETE CASCADE,
    predicate_name text  NOT NULL,
    arguments      jsonb NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    PRIMARY KEY (refresh_id, predicate_name, arguments)
);
