-- Migration 004: actor column on morpholog.audit.
--
-- Apply this to an existing morpholog_dev database that was created
-- after migration 003 landed but before this one. Fresh installations
-- applying schema.sql already include this column.
--
-- Idempotent on first apply (IF NOT EXISTS is not available for
-- ADD COLUMN under PostgreSQL <16 only; PG17 supports it). Wrapped
-- in a DO block so re-running on a database that already has the
-- column is a no-op rather than an error.
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

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = 'morpholog'
          AND table_name = 'audit'
          AND column_name = 'actor'
    ) THEN
        ALTER TABLE morpholog.audit
            ADD COLUMN actor jsonb;
        UPDATE morpholog.audit
            SET actor = '{"type":"subject","value":"unknown"}'::jsonb
            WHERE actor IS NULL;
        ALTER TABLE morpholog.audit
            ALTER COLUMN actor SET NOT NULL;
    END IF;
END $$;
