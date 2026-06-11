-- Migration 006: the audit keyset index carries the tie-break column.
--
-- Apply to an existing database created before the audit read
-- contract landed. Fresh installations applying schema.sql already
-- create the composite index.
--
-- Idempotent: re-running is safe.
--
-- Why: every audit read - the blessed `inspect audit` tail, verify,
-- coverage replay, and the as-of reconstructions - orders and pages
-- by `(committed_at, transition_id)`. The original single-column
-- index served the ORDER BY only up to its first column; the
-- composite serves the keyset cursor (`(committed_at, transition_id)
-- > ($1, $2)`) directly. Correctness is unaffected either way - this
-- is a performance migration.

SET search_path TO morpholog, public;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_indexes
        WHERE schemaname = 'morpholog'
          AND indexname = 'audit_committed_at'
          AND indexdef LIKE '%transition_id%'
    ) THEN
        DROP INDEX IF EXISTS morpholog.audit_committed_at;
        CREATE INDEX audit_committed_at
            ON morpholog.audit (committed_at, transition_id);
    END IF;
END $$;
