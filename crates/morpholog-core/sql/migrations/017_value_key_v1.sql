-- Migration 017: the canonical equality key, and the end of the guard.
--
-- `morpholog.value_key_v1` is the one equality the compiled checks
-- compare stored values by (see schema.sql for what it keys). With it, a
-- compiled read no longer depends on the kind the programme declares for
-- a position, so `morpholog.declared_kind`, which failed a check closed
-- when a stored value was of another kind, has nothing left to guard and
-- is dropped. Idempotent: the key function is replaced only where this
-- marker says Morpholog wrote it, never over an operator's function of
-- the same name.

DO $migration$
DECLARE
    marker text;
    body_digest text;
BEGIN
    -- Catalogue renderings qualify names not on the search path, so pin
    -- it: the comparisons below are then exact, whatever the session's.
    PERFORM set_config('search_path', 'pg_catalog', true);
    IF to_regprocedure('morpholog.value_key_v1(jsonb)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.value_key_v1(v jsonb) RETURNS jsonb
                LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
            AS $$
DECLARE
    tag text;
    elements jsonb;
BEGIN
    IF jsonb_typeof(v) <> 'object' THEN
        RETURN jsonb_build_array('raw', v);
    END IF;
    tag := v ->> 'type';
    IF tag IS NULL THEN
        RETURN jsonb_build_array('raw', v);
    END IF;
    CASE tag
        WHEN 'decimal' THEN
            RETURN jsonb_build_array(tag, trim_scale((v ->> 'value')::numeric)::text);
        WHEN 'quantity' THEN
            RETURN jsonb_build_array(tag, v -> 'value' ->> 'unit', trim_scale((v -> 'value' ->> 'amount')::numeric)::text);
        WHEN 'collection' THEN
            SELECT coalesce(jsonb_agg(morpholog.value_key_v1(element) ORDER BY ordinal), '[]'::jsonb)
              INTO elements
              FROM jsonb_array_elements(v -> 'value') WITH ORDINALITY AS items(element, ordinal);
            RETURN jsonb_build_array(tag, elements);
        ELSE
            RETURN jsonb_build_array(tag, v -> 'value');
    END CASE;
END
$$
        $create$;
        COMMENT ON FUNCTION morpholog.value_key_v1(jsonb) IS 'morpholog value key v1';
        RETURN;
    END IF;
    -- Present: it must be exactly this function, marker and body, and is
    -- then left alone. An index is built over it and is never rebuilt
    -- for a changed body, so a body that differs is refused whatever the
    -- marker says: a key with other semantics is a new function.
    SELECT obj_description(oid, 'pg_proc'), encode(sha256(convert_to(prosrc, 'UTF8')), 'hex')
      INTO marker, body_digest
      FROM pg_proc
     WHERE oid = 'morpholog.value_key_v1(jsonb)'::regprocedure;
    IF marker IS DISTINCT FROM 'morpholog value key v1'
       OR body_digest IS DISTINCT FROM '2a7ea81bdf669b11b299795eb88278d3c1dd207416b1a971376e2ffd9668ffd7' THEN
        RAISE EXCEPTION 'morpholog.value_key_v1 exists and is not the one migration 017 defines (marker %, body %); refusing to replace it', coalesce(marker, 'none'), body_digest;
    END IF;
END $migration$;

-- The guard is dropped only where migration 016 wrote it; an operator's
-- function of the same name is refused, never removed.
DO $$
DECLARE
    marker text;
BEGIN
    IF to_regprocedure('morpholog.declared_kind(text, text, jsonb, text, integer)') IS NULL THEN
        RETURN;
    END IF;
    SELECT obj_description(oid, 'pg_proc') INTO marker
      FROM pg_proc
     WHERE oid = 'morpholog.declared_kind(text, text, jsonb, text, integer)'::regprocedure;
    IF marker IS DISTINCT FROM 'morpholog declared kind guard v1' THEN
        RAISE EXCEPTION 'morpholog.declared_kind exists and is not the guard migration 016 wrote; refusing to drop it';
    END IF;
    DROP FUNCTION morpholog.declared_kind(text, text, jsonb, text, integer);
END $$;
