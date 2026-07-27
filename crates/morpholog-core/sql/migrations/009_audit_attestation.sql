-- Attestation lineage on the audit record.
--
-- Every commit records how the actor identity was established, e.g.
-- {"mode":"gateway","authenticated_by":"<login role>"} - the role
-- PostgreSQL authenticated for the proposing connection. The field is
-- part of the Merkle leaf for the rows that carry it.
--
-- NEVER backfill this column. A row's leaf encoding is chosen by the
-- field's presence: rows written before attestation existed hash under
-- the original encoding, and rewriting them would invalidate every
-- checkpoint root computed over them.
--
-- The NOT VALID constraint is the activation boundary: PostgreSQL
-- skips validating the pre-existing rows (which stay NULL, and must)
-- but refuses every NEW unattested insert - so a stale binary from
-- before attestation cannot quietly write rows the Merkle layer would
-- bless under the historical encoding. Attestation is a one-way
-- switch, not a per-row option. See
-- crates/morpholog-core/sql/schema.sql for the fresh-database
-- definition, which is NOT NULL outright.

ALTER TABLE morpholog.audit
    ADD COLUMN IF NOT EXISTS attestation jsonb
        CHECK (attestation IS NULL OR jsonb_typeof(attestation) = 'object');

-- Added only when absent. Dropping and re-adding unconditionally would
-- downgrade an already-VALIDATED constraint to NOT VALID on a database
-- that is at the head - harmless for new rows, but it means re-applying
-- the migrations is not the no-op `morpholog migrate` needs it to be.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'audit_attestation_required'
          AND conrelid = 'morpholog.audit'::regclass
    ) THEN
        ALTER TABLE morpholog.audit
            ADD CONSTRAINT audit_attestation_required
            CHECK (attestation IS NOT NULL)
            NOT VALID;
    END IF;
END
$$;
