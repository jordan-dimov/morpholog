-- Self-describing audit rows: the transformation's parameter names,
-- in declaration order, stamped beside the positional arguments at
-- commit. A row then names its own signature after the act that
-- wrote it has been retired, with no client-side vocabulary. The
-- field is part of the Merkle leaf for the rows that carry it.
--
-- NEVER backfill this column. A row's leaf encoding is chosen by the
-- field's presence: rows written before names existed hash under
-- their earlier encodings, and rewriting them would invalidate every
-- checkpoint root computed over them.
--
-- The NOT VALID constraint is the activation boundary, as for
-- attestation: PostgreSQL skips the pre-existing rows (which stay
-- NULL, and must) but refuses every NEW unstamped insert, so a stale
-- binary cannot quietly write rows that would hash under the
-- unstamped encoding. See crates/morpholog-core/sql/schema.sql for
-- the fresh-database definition, where the same named constraint is
-- validated outright.

ALTER TABLE morpholog.audit
    ADD COLUMN IF NOT EXISTS parameters jsonb;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'audit_parameters_shape'
          AND conrelid = 'morpholog.audit'::regclass
    ) THEN
        ALTER TABLE morpholog.audit
            ADD CONSTRAINT audit_parameters_shape CHECK (
                parameters IS NULL
                OR (jsonb_typeof(parameters) = 'array'
                    AND jsonb_array_length(parameters) = jsonb_array_length(arguments))
            );
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'audit_parameters_required'
          AND conrelid = 'morpholog.audit'::regclass
    ) THEN
        ALTER TABLE morpholog.audit
            ADD CONSTRAINT audit_parameters_required
            CHECK (parameters IS NOT NULL)
            NOT VALID;
    END IF;
END
$$;
