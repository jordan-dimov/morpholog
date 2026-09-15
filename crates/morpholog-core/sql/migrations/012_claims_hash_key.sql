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
-- Guarded three ways on each table: the old key migrates, the new key
-- is left alone, and any other shape is refused rather than declared
-- current. Adding the stored column rewrites the claims table once.

CREATE OR REPLACE FUNCTION morpholog.claim_digest(args jsonb) RETURNS bytea
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN sha256(convert_to(args::text, 'UTF8'));

DO $$
DECLARE
    key text := (
        SELECT pg_get_constraintdef(oid) FROM pg_constraint
        WHERE conrelid = 'morpholog.claims'::regclass AND contype = 'p'
    );
    digest_column boolean := EXISTS (
        SELECT 1 FROM pg_attribute
        WHERE attrelid = 'morpholog.claims'::regclass
          AND attname = 'arguments_hash' AND attgenerated = 's' AND NOT attisdropped
    );
BEGIN
    IF key = 'PRIMARY KEY (predicate_name, arguments_hash)' AND digest_column THEN
        RETURN;
    END IF;
    IF key IS DISTINCT FROM 'PRIMARY KEY (predicate_name, arguments)' OR digest_column THEN
        RAISE EXCEPTION 'morpholog.claims is neither the pre-digest nor the digest shape '
            '(key %, digest column %); refusing to guess',
            coalesce(key, 'absent'), digest_column;
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

CREATE INDEX IF NOT EXISTS derived_claims_generation_predicate
    ON morpholog_read.derived_claims (refresh_id, predicate_name);
