-- Migration 004: actor column on morpholog.audit.
--
-- Apply this to an existing morpholog_dev database that was created
-- after migration 003 landed but before this one. Fresh installations
-- applying schema.sql already include this column.
--
-- Idempotent: re-running is safe. Robust against a partially-migrated
-- database where `actor` was added nullable but never backfilled or
-- constrained.
--
-- What this migration does:
--   Adds a JSONB column `actor` to morpholog.audit, recording the
--   actor under whose authority each committed transition was
--   proposed. Existing rows (written before actor identity was
--   plumbed through) are backfilled to the sentinel subject value
--   "unknown" so the column can be made NOT NULL going forward.
--
-- Why: actor identity is transition context - it answers "who
-- proposed this transition, under what authority?" - and the audit
-- log is the only durable record of it. Future invariants (authority
-- checks) will consult this column. Plumbing it through is a
-- prerequisite for the actor-authority worked example.

SET search_path TO morpholog, public;

ALTER TABLE morpholog.audit
    ADD COLUMN IF NOT EXISTS actor jsonb;

UPDATE morpholog.audit
    SET actor = '{"type":"subject","value":"unknown"}'::jsonb
    WHERE actor IS NULL;

ALTER TABLE morpholog.audit
    ALTER COLUMN actor SET NOT NULL;
