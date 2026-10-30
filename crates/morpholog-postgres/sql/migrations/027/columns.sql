-- Defaults are stored in the catalog, so no legacy row is rewritten and
-- the ACCESS EXCLUSIVE lock this takes lasts milliseconds.
ALTER TABLE morpholog.audit
    ADD COLUMN IF NOT EXISTS audit_order bigint NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS act_ordinal int NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS ordered_at timestamptz,
    ADD COLUMN IF NOT EXISTS operations jsonb;
