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
--
-- Guarded like 012: the pre-migration shape migrates, the exact head
-- shape is left alone, and any other shape (a column of another type, a
-- same-named constraint saying something else) is refused rather than
-- declared current, since the runtime and the activation boundary both
-- rest on these exact definitions.

DO $$
DECLARE
    column_type text;
    column_not_null boolean;
    column_derived boolean;
    shape text;
    required text;
BEGIN
    -- Catalogue renderings qualify names not on the search path, so pin
    -- it: the comparisons below are then exact, whatever the session's.
    PERFORM set_config('search_path', 'pg_catalog', true);
    SELECT format_type(atttypid, atttypmod), attnotnull, atthasdef OR attgenerated <> ''
    INTO column_type, column_not_null, column_derived
    FROM pg_attribute
    WHERE attrelid = 'morpholog.audit'::regclass
      AND attname = 'model_hash' AND NOT attisdropped;
    SELECT pg_get_constraintdef(oid) INTO shape FROM pg_constraint
    WHERE conrelid = 'morpholog.audit'::regclass AND conname = 'audit_model_hash_shape';
    SELECT pg_get_constraintdef(oid) INTO required FROM pg_constraint
    WHERE conrelid = 'morpholog.audit'::regclass AND conname = 'audit_model_hash_required';

    -- Either constraint may carry NOT VALID: it then checks only rows
    -- written after it was added, which is exactly the rule for new rows.
    IF column_type = 'text' AND NOT column_not_null AND NOT column_derived
       AND regexp_replace(shape, ' NOT VALID$', '') = 'CHECK (((model_hash IS NULL) OR ((model_hash ~ ''^sha256:[0-9a-f]{64}$''::text) AND (attestation IS NOT NULL) AND (parameters IS NOT NULL))))'
       AND regexp_replace(required, ' NOT VALID$', '') = 'CHECK ((model_hash IS NOT NULL))' THEN
        RETURN;
    END IF;
    IF column_type IS NOT NULL OR shape IS NOT NULL OR required IS NOT NULL THEN
        RAISE EXCEPTION 'morpholog.audit is neither the pre-model-hash nor the model-hash shape '
            '(column %, audit_model_hash_shape %, audit_model_hash_required %); refusing to guess',
            coalesce(column_type
                     || CASE WHEN column_not_null THEN ' not null' ELSE '' END
                     || CASE WHEN column_derived THEN ' with a default or expression' ELSE '' END,
                     'absent'),
            coalesce(shape, 'absent'), coalesce(required, 'absent');
    END IF;

    ALTER TABLE morpholog.audit ADD COLUMN model_hash text;
    ALTER TABLE morpholog.audit
        ADD CONSTRAINT audit_model_hash_shape CHECK (
            model_hash IS NULL
            OR (model_hash ~ '^sha256:[0-9a-f]{64}$'
                AND attestation IS NOT NULL
                AND parameters IS NOT NULL)
        );
    ALTER TABLE morpholog.audit
        ADD CONSTRAINT audit_model_hash_required
        CHECK (model_hash IS NOT NULL)
        NOT VALID;
END
$$;
