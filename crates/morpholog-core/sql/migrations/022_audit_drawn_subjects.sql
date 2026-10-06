-- Every admission records the subjects its act drew from `new Subject()`,
-- in draw order: the one input to the decision that would otherwise be
-- gone once the act ran. An empty list means the act drew none. The
-- field is part of the Merkle leaf for the rows that carry it.
--
-- NEVER backfill this column. A row's leaf encoding is chosen by the
-- field's presence: rows written before the record existed hash under
-- their earlier encodings, and rewriting them would invalidate every
-- checkpoint root computed over them. Nor would a backfill be truthful:
-- NULL means the draws were not recorded, and they cannot be recovered.
--
-- The shape holds the list to an array of strings, and the ladder: a draw
-- record only on a row that names its semantics. The NOT VALID
-- constraint is the activation boundary: PostgreSQL skips the
-- pre-existing rows (which stay NULL, and must) but refuses every NEW
-- insert without the record. See crates/morpholog-core/sql/schema.sql for
-- the fresh-database definition, where the same named constraint is
-- validated outright.
--
-- Guarded like 021: the pre-migration shape migrates, the exact head
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
      AND attname = 'drawn_subjects' AND NOT attisdropped;
    SELECT pg_get_constraintdef(oid) INTO shape FROM pg_constraint
    WHERE conrelid = 'morpholog.audit'::regclass AND conname = 'audit_drawn_subjects_shape';
    SELECT pg_get_constraintdef(oid) INTO required FROM pg_constraint
    WHERE conrelid = 'morpholog.audit'::regclass AND conname = 'audit_drawn_subjects_required';

    -- Either constraint may carry NOT VALID: it then checks only rows
    -- written after it was added, which is exactly the rule for new rows.
    IF column_type = 'jsonb' AND NOT column_not_null AND NOT column_derived
       AND regexp_replace(shape, ' NOT VALID$', '') = 'CHECK (((drawn_subjects IS NULL) OR ((jsonb_typeof(drawn_subjects) = ''array''::text) AND (NOT jsonb_path_exists(drawn_subjects, ''strict $[*]?(@.type() != "string")''::jsonpath)) AND (semantics_version IS NOT NULL))))'
       AND regexp_replace(required, ' NOT VALID$', '') = 'CHECK ((drawn_subjects IS NOT NULL))' THEN
        RETURN;
    END IF;
    IF column_type IS NOT NULL OR shape IS NOT NULL OR required IS NOT NULL THEN
        RAISE EXCEPTION 'morpholog.audit is neither the pre-drawn-subjects nor the drawn-subjects shape '
            '(column %, audit_drawn_subjects_shape %, audit_drawn_subjects_required %); refusing to guess',
            coalesce(column_type
                     || CASE WHEN column_not_null THEN ' not null' ELSE '' END
                     || CASE WHEN column_derived THEN ' with a default or expression' ELSE '' END,
                     'absent'),
            coalesce(shape, 'absent'), coalesce(required, 'absent');
    END IF;

    ALTER TABLE morpholog.audit ADD COLUMN drawn_subjects jsonb;
    ALTER TABLE morpholog.audit
        ADD CONSTRAINT audit_drawn_subjects_shape CHECK (
            drawn_subjects IS NULL
            OR (jsonb_typeof(drawn_subjects) = 'array'
                AND NOT jsonb_path_exists(drawn_subjects, 'strict $[*] ? (@.type() != "string")')
                AND semantics_version IS NOT NULL)
        );
    ALTER TABLE morpholog.audit
        ADD CONSTRAINT audit_drawn_subjects_required
        CHECK (drawn_subjects IS NOT NULL)
        NOT VALID;
END
$$;
