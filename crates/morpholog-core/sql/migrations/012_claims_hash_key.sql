-- Claims keyed by a digest of their arguments, not the arguments.
--
-- The whole argument array was the primary key, which put a silent
-- ceiling on argument size: a btree index row may not exceed 2704
-- bytes, so a long enough argument (an embedder's free-text statement)
-- was refused by the index rather than by any rule, as a raw database
-- error the proposal never got to decide. The key becomes
-- (predicate_name, arguments_hash), a stored digest of the canonical
-- jsonb text - fixed-width, and equal exactly when the arrays are
-- equal for every lawfully encoded argument (decimals are strings, so
-- jsonb's numeric normalisation never applies). See schema.sql for the
-- fresh-database definition and the helper's volatility note.
--
-- The derived read cache had the same key shape and the same ceiling.
-- Its rows are the kernel's set-valued output, already distinct, so it
-- loses the key for a plain lookup index rather than taking on a
-- digest of its own.
--
-- Guarded three ways on each table: the old key migrates, the head
-- shape is left alone, and any other shape is refused rather than
-- declared current. The head shape is proven exactly - the helper's
-- body, the column's expression, and every stored digest against the
-- helper - because a digest materialised under any other definition
-- would leave its row unreachable by the retract that recomputes it.
-- Adding the stored column rewrites the claims table once.

-- The helper: created when absent, left alone when it is the one this
-- migration expects, refused otherwise. Never replaced: replacing a
-- function does not recompute the stored digests generated under it.
DO $$
DECLARE
    body text;
BEGIN
    -- Catalogue renderings qualify names not on the search path, so pin
    -- it: the comparisons below are then exact, whatever the session's.
    PERFORM set_config('search_path', 'pg_catalog', true);
    IF to_regprocedure('morpholog.claim_digest(jsonb)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.claim_digest(args jsonb) RETURNS bytea
                LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
                RETURN sha256(convert_to(args::text, 'UTF8'))
        $create$;
        RETURN;
    END IF;
    body := pg_get_function_sqlbody('morpholog.claim_digest(jsonb)'::regprocedure);
    IF body IS DISTINCT FROM 'RETURN sha256(convert_to((args)::text, ''UTF8''::name))' THEN
        RAISE EXCEPTION 'morpholog.claim_digest exists with another definition (%); '
            'refusing to replace it underneath digests stored by it', coalesce(body, 'not a SQL body');
    END IF;
END
$$;

DO $$
DECLARE
    key text;
    any_column boolean;
    generated text;
    stale bigint;
BEGIN
    PERFORM set_config('search_path', 'pg_catalog', true);
    SELECT pg_get_constraintdef(oid) INTO key FROM pg_constraint
    WHERE conrelid = 'morpholog.claims'::regclass AND contype = 'p';
    any_column := EXISTS (
        SELECT 1 FROM pg_attribute
        WHERE attrelid = 'morpholog.claims'::regclass
          AND attname = 'arguments_hash' AND NOT attisdropped
    );
    -- The head column's expression, as rendered: stored, bytea, and
    -- exactly the helper over the arguments.
    SELECT pg_get_expr(d.adbin, d.adrelid) INTO generated
    FROM pg_attribute a
    JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE a.attrelid = 'morpholog.claims'::regclass
      AND a.attname = 'arguments_hash' AND NOT a.attisdropped
      AND a.attgenerated = 's' AND a.atttypid = 'bytea'::regtype;
    IF key = 'PRIMARY KEY (predicate_name, arguments_hash)'
       AND generated = 'morpholog.claim_digest(arguments)' THEN
        -- The shape is right; the stored digests must be too, or a row
        -- keyed under an earlier helper is unreachable by retraction.
        SELECT count(*) INTO stale FROM morpholog.claims
        WHERE arguments_hash <> morpholog.claim_digest(arguments);
        IF stale > 0 THEN
            RAISE EXCEPTION '% stored digests in morpholog.claims disagree with '
                'morpholog.claim_digest; refusing to declare the table current', stale;
        END IF;
        RETURN;
    END IF;
    IF key IS DISTINCT FROM 'PRIMARY KEY (predicate_name, arguments)' OR any_column THEN
        RAISE EXCEPTION 'morpholog.claims is neither the pre-digest nor the digest shape '
            '(key %, arguments_hash %); refusing to guess',
            coalesce(key, 'absent'), coalesce(generated, CASE WHEN any_column THEN 'present, not the digest' ELSE 'absent' END);
    END IF;
    ALTER TABLE morpholog.claims
        ADD COLUMN arguments_hash bytea NOT NULL
            GENERATED ALWAYS AS (morpholog.claim_digest(arguments)) STORED;
    ALTER TABLE morpholog.claims
        DROP CONSTRAINT claims_pkey,
        ADD PRIMARY KEY (predicate_name, arguments_hash);
END
$$;

DO $$
DECLARE
    key text := (
        SELECT pg_get_constraintdef(oid) FROM pg_constraint
        WHERE conrelid = 'morpholog_read.derived_claims'::regclass AND contype = 'p'
    );
BEGIN
    IF key IS NULL THEN
        RETURN;
    END IF;
    IF key <> 'PRIMARY KEY (refresh_id, predicate_name, arguments)' THEN
        RAISE EXCEPTION 'morpholog_read.derived_claims has an unexpected key (%); refusing to guess', key;
    END IF;
    ALTER TABLE morpholog_read.derived_claims DROP CONSTRAINT derived_claims_pkey;
END
$$;

-- By definition, not by name: an index of this name with other columns
-- would leave the derived views without their lookup path.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_indexes
        WHERE schemaname = 'morpholog_read'
          AND indexname = 'derived_claims_generation_predicate'
          AND indexdef LIKE '%(refresh_id, predicate_name)'
    ) THEN
        DROP INDEX IF EXISTS morpholog_read.derived_claims_generation_predicate;
        CREATE INDEX derived_claims_generation_predicate
            ON morpholog_read.derived_claims (refresh_id, predicate_name);
    END IF;
END
$$;
