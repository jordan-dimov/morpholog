-- Every admission names the programme that admitted it: the canonical
-- hash of the whole programme (`morpholog hash`), stamped at commit. With
-- it a row is a join key back to the exact rulebook, not only a list of
-- the rule names it was checked against. The field is part of the Merkle
-- leaf for the rows that carry it.
--
-- NEVER backfill this column. A row's leaf encoding is chosen by the
-- field's presence: rows written before the stamp existed hash under
-- their earlier encodings, and rewriting them would invalidate every
-- checkpoint root computed over them. Nor could a backfill be truthful:
-- nothing records which programme admitted a historical row.
--
-- The shape constraint also holds the ladder: a hash only on a row that
-- already carries an attestation and parameter names. The NOT VALID
-- constraint is the activation boundary, as for parameters: PostgreSQL
-- skips the pre-existing rows (which stay NULL, and must) but refuses
-- every NEW unstamped insert. See crates/morpholog-core/sql/schema.sql
-- for the fresh-database definition, where the same named constraint is
-- validated outright.

ALTER TABLE morpholog.audit
    ADD COLUMN IF NOT EXISTS model_hash text;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'audit_model_hash_shape'
          AND conrelid = 'morpholog.audit'::regclass
    ) THEN
        ALTER TABLE morpholog.audit
            ADD CONSTRAINT audit_model_hash_shape CHECK (
                model_hash IS NULL
                OR (model_hash ~ '^sha256:[0-9a-f]{64}$'
                    AND attestation IS NOT NULL
                    AND parameters IS NOT NULL)
            );
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'audit_model_hash_required'
          AND conrelid = 'morpholog.audit'::regclass
    ) THEN
        ALTER TABLE morpholog.audit
            ADD CONSTRAINT audit_model_hash_required
            CHECK (model_hash IS NOT NULL)
            NOT VALID;
    END IF;
END
$$;
