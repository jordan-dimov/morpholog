-- Migration 018: the coordinate the compiled checks order civil dates
-- by. `morpholog.date_ordinal` turns a stored date into one integer,
-- `year * 10000 + month * 100 + day`, monotone in the civil fields and
-- true to the signed six-digit years the codec writes outside 0000-9999,
-- which neither text order nor a `date` cast would keep. Another tag
-- yields NULL, and the check that asked reports the kernel's kind error
-- from that; a date whose text is not the codec's is an error, since no
-- admitted claim can carry one. Idempotent: the function is created only
-- where absent, and an existing one must be exactly this one, marker and
-- body, or the migration refuses to touch it.

DO $migration$
DECLARE
    marker text;
    body_digest text;
BEGIN
    PERFORM set_config('search_path', 'pg_catalog', true);
    IF to_regprocedure('morpholog.date_ordinal(jsonb)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.date_ordinal(v jsonb) RETURNS integer
                LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
            AS $$
DECLARE
    m text[];
    y integer;
    mo integer;
    d integer;
BEGIN
    IF v ->> 'type' IS DISTINCT FROM 'date' THEN
        RETURN NULL;
    END IF;
    m := regexp_match(v ->> 'value', '^(-?[0-9]{4,6})-([0-9]{2})-([0-9]{2})$');
    IF m IS NULL THEN
        RAISE EXCEPTION 'not a stored date: %', v ->> 'value';
    END IF;
    y := m[1]::integer;
    mo := m[2]::integer;
    d := m[3]::integer;
    -- The codec writes only dates jiff holds: years -9999 to 9999, four
    -- digits inside 0000-9999 and signed six outside, real calendar days.
    IF y < -9999 OR y > 9999
       OR m[1] !~ '^([0-9]{4}|-[0-9]{6})$' OR (m[1] ~ '^-' AND y = 0)
       OR mo < 1 OR mo > 12 OR d < 1
       OR d > (CASE mo
                 WHEN 2 THEN (CASE WHEN y % 4 = 0 AND (y % 100 <> 0 OR y % 400 = 0) THEN 29 ELSE 28 END)
                 WHEN 4 THEN 30 WHEN 6 THEN 30 WHEN 9 THEN 30 WHEN 11 THEN 30
                 ELSE 31 END)
    THEN
        RAISE EXCEPTION 'not a stored date: %', v ->> 'value';
    END IF;
    RETURN y * 10000 + mo * 100 + d;
END
$$
        $create$;
        COMMENT ON FUNCTION morpholog.date_ordinal(jsonb) IS 'morpholog date coordinate v1';
        RETURN;
    END IF;
    SELECT obj_description(oid, 'pg_proc'), encode(sha256(convert_to(prosrc, 'UTF8')), 'hex')
      INTO marker, body_digest
      FROM pg_proc
     WHERE oid = 'morpholog.date_ordinal(jsonb)'::regprocedure;
    IF marker IS DISTINCT FROM 'morpholog date coordinate v1'
       OR body_digest IS DISTINCT FROM '3200ba8a2b7e9d8a9395485caca199a6810e73a82532917a306a7de927332dc7' THEN
        RAISE EXCEPTION 'morpholog.date_ordinal exists and is not the one migration 018 defines (marker %, body %); refusing to replace it', coalesce(marker, 'none'), body_digest;
    END IF;
END $migration$;
