-- Every admission names the semantics that decided it: the kernel's
-- SEMANTICS_VERSION, stamped at commit beside the programme hash. The
-- pair says which rules, under which contract, admitted the row. The
-- field is part of the Merkle leaf for the rows that carry it.
--
-- NEVER backfill this column. A row's leaf encoding is chosen by the
-- field's presence: rows written before the stamp existed hash under
-- their earlier encodings, and rewriting them would invalidate every
-- checkpoint root computed over them. Nor would a backfill be truthful:
-- NULL means the semantics were not recorded, not that they were
-- version 1.
--
-- The version is a u32, so the column is bigint and the shape holds it
-- to the u32 range. The shape also holds the ladder: a semantics version
-- only on a row that names its programme (which already requires an
-- attestation and parameter names). The NOT VALID constraint is the
-- activation boundary: PostgreSQL skips the pre-existing rows (which stay
-- NULL, and must) but refuses every NEW unstamped insert. See
-- crates/morpholog-core/sql/schema.sql for the fresh-database definition,
-- where the same named constraint is validated outright.
--
-- Guarded like 020: the pre-migration shape migrates, the exact head
-- shape is left alone, and any other shape is refused rather than
-- declared current.

DO $$
DECLARE
    column_type text;
    column_not_null boolean;
    column_derived boolean;
    shape text;
    required text;
BEGIN
    PERFORM set_config('search_path', 'pg_catalog', true);
    SELECT format_type(atttypid, atttypmod), attnotnull, atthasdef OR attgenerated <> ''
    INTO column_type, column_not_null, column_derived
    FROM pg_attribute
    WHERE attrelid = 'morpholog.audit'::regclass
      AND attname = 'semantics_version' AND NOT attisdropped;
    SELECT pg_get_constraintdef(oid) INTO shape FROM pg_constraint
    WHERE conrelid = 'morpholog.audit'::regclass AND conname = 'audit_semantics_version_shape';
    SELECT pg_get_constraintdef(oid) INTO required FROM pg_constraint
    WHERE conrelid = 'morpholog.audit'::regclass AND conname = 'audit_semantics_version_required';

    -- Either constraint may carry NOT VALID: it then checks only rows
    -- written after it was added, which is exactly the rule for new rows.
    IF column_type = 'bigint' AND NOT column_not_null AND NOT column_derived
       AND regexp_replace(shape, ' NOT VALID$', '') = 'CHECK (((semantics_version IS NULL) OR (((semantics_version >= 1) AND (semantics_version <= ''4294967295''::bigint)) AND (model_hash IS NOT NULL))))'
       AND regexp_replace(required, ' NOT VALID$', '') = 'CHECK ((semantics_version IS NOT NULL))' THEN
        RETURN;
    END IF;
    IF column_type IS NOT NULL OR shape IS NOT NULL OR required IS NOT NULL THEN
        RAISE EXCEPTION 'morpholog.audit is neither the pre-semantics-version nor the semantics-version shape '
            '(column %, audit_semantics_version_shape %, audit_semantics_version_required %); refusing to guess',
            coalesce(column_type
                     || CASE WHEN column_not_null THEN ' not null' ELSE '' END
                     || CASE WHEN column_derived THEN ' with a default or expression' ELSE '' END,
                     'absent'),
            coalesce(shape, 'absent'), coalesce(required, 'absent');
    END IF;

    ALTER TABLE morpholog.audit ADD COLUMN semantics_version bigint;
    ALTER TABLE morpholog.audit
        ADD CONSTRAINT audit_semantics_version_shape CHECK (
            semantics_version IS NULL
            OR (semantics_version BETWEEN 1 AND 4294967295
                AND model_hash IS NOT NULL)
        );
    ALTER TABLE morpholog.audit
        ADD CONSTRAINT audit_semantics_version_required
        CHECK (semantics_version IS NOT NULL)
        NOT VALID;
END
$$;
