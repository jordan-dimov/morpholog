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

DO $$
DECLARE
    marker text;
BEGIN
    IF to_regprocedure('morpholog.value_key_v1(jsonb)') IS NULL THEN
        RETURN;
    END IF;
    SELECT obj_description(oid, 'pg_proc') INTO marker
      FROM pg_proc
     WHERE oid = 'morpholog.value_key_v1(jsonb)'::regprocedure;
    IF marker IS DISTINCT FROM 'morpholog value key v1' THEN
        RAISE EXCEPTION 'morpholog.value_key_v1 exists and is not the one migration 017 defines; refusing to replace it';
    END IF;
END $$;

CREATE OR REPLACE FUNCTION morpholog.value_key_v1(v jsonb) RETURNS jsonb
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
$$;

COMMENT ON FUNCTION morpholog.value_key_v1(jsonb) IS 'morpholog value key v1';

DROP FUNCTION IF EXISTS morpholog.declared_kind(text, text, jsonb, text, integer);
