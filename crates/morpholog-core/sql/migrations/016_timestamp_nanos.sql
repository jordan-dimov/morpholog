-- Migration 016: two functions the compiled checks rely on - the timestamp
-- coordinate they order instants by, and the guard that fails a check
-- closed when a stored value is not of its declared kind.
--
-- `morpholog.timestamp_nanos` turns a stored timestamp argument into
-- nanoseconds since the Unix epoch, so SQL orders instants exactly as the
-- kernel does. The stored text is jiff's: a four-digit year, or a signed
-- six-digit one outside 0000-9999; a fraction only when it is not zero,
-- trailing zeros trimmed. That text does not sort (`.5Z` before `Z`),
-- `timestamptz` rounds to microseconds and stops at 4713 BC, so the civil
-- fields are converted by hand, in numeric: the count exceeds bigint.
--
-- It takes the whole tagged value. Another tag yields NULL, and the check
-- that asked reports the kernel's kind error from that; a timestamp whose
-- text is not the codec's is an error, since no admitted claim can carry
-- one. Idempotent: the function is replaced only where this marker says
-- Morpholog wrote it, never over an operator's function of the same name.

DO $$
DECLARE
    marker text;
BEGIN
    IF to_regprocedure('morpholog.timestamp_nanos(jsonb)') IS NULL THEN
        RETURN;
    END IF;
    SELECT obj_description(oid, 'pg_proc') INTO marker
      FROM pg_proc
     WHERE oid = 'morpholog.timestamp_nanos(jsonb)'::regprocedure;
    IF marker IS DISTINCT FROM 'morpholog timestamp coordinate v1' THEN
        RAISE EXCEPTION 'morpholog.timestamp_nanos exists and is not the one migration 016 defines; refusing to replace it';
    END IF;
END $$;

CREATE OR REPLACE FUNCTION morpholog.timestamp_nanos(v jsonb) RETURNS numeric
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
AS $$
DECLARE
    m text[];
    y numeric;
    mo numeric;
    d numeric;
    era numeric;
    yoe numeric;
    doy numeric;
    doe numeric;
    days numeric;
BEGIN
    IF v ->> 'type' IS DISTINCT FROM 'timestamp' THEN
        RETURN NULL;
    END IF;
    m := regexp_match(v ->> 'value', '^(-?[0-9]{4,6})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})(?:\.([0-9]{1,9}))?Z$');
    IF m IS NULL THEN
        RAISE EXCEPTION 'not a stored timestamp: %', v ->> 'value';
    END IF;
    y := m[1]::numeric;
    mo := m[2]::numeric;
    d := m[3]::numeric;
    -- The codec writes only instants jiff holds: years -9999 to 9999,
    -- four digits inside 0000-9999 and signed six outside, real calendar
    -- days, and a clock below 24:00:00.
    IF y < -9999 OR y > 9999
       OR m[1] !~ '^([0-9]{4}|-[0-9]{6})$' OR (m[1] ~ '^-' AND y = 0)
       OR mo < 1 OR mo > 12 OR d < 1
       OR d > (CASE mo
                 WHEN 2 THEN (CASE WHEN y % 4 = 0 AND (y % 100 <> 0 OR y % 400 = 0) THEN 29 ELSE 28 END)
                 WHEN 4 THEN 30 WHEN 6 THEN 30 WHEN 9 THEN 30 WHEN 11 THEN 30
                 ELSE 31 END)
       OR m[4]::numeric > 23 OR m[5]::numeric > 59 OR m[6]::numeric > 59
    THEN
        RAISE EXCEPTION 'not a stored timestamp: %', v ->> 'value';
    END IF;
    IF mo <= 2 THEN
        y := y - 1;
    END IF;
    era := floor(y / 400);
    yoe := y - era * 400;
    doy := floor((153 * (mo + CASE WHEN mo > 2 THEN -3 ELSE 9 END) + 2) / 5) + d - 1;
    doe := yoe * 365 + floor(yoe / 4) - floor(yoe / 100) + doy;
    days := era * 146097 + doe - 719468;
    RETURN (days * 86400 + m[4]::numeric * 3600 + m[5]::numeric * 60 + m[6]::numeric) * 1000000000
        + rpad(coalesce(m[7], ''), 9, '0')::numeric;
END
$$;

COMMENT ON FUNCTION morpholog.timestamp_nanos(jsonb) IS 'morpholog timestamp coordinate v1';

-- See schema.sql: a compiled read of a position by its declared kind must
-- find that kind, or the check fails closed with SQLSTATE MP001.
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
        RAISE EXCEPTION 'morpholog.declared_kind exists and is not the one migration 016 defines; refusing to replace it';
    END IF;
END $$;

CREATE OR REPLACE FUNCTION morpholog.declared_kind(predicate text, declared_predicate text, v jsonb, kind text, pos integer) RETURNS boolean
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE COST 1
AS $$
DECLARE
    stored text;
BEGIN
    IF predicate <> declared_predicate THEN
        RETURN true;
    END IF;
    stored := coalesce(v ->> 'type', jsonb_typeof(v));
    IF stored = kind THEN
        RETURN true;
    END IF;
    RAISE EXCEPTION USING
        ERRCODE = 'MP001',
        MESSAGE = format('%s[%s] holds a %s where the programme declares %s', declared_predicate, pos, stored, kind),
        DETAIL = json_build_object('predicate', declared_predicate, 'position', pos, 'declared', kind, 'stored', stored)::text;
END
$$;

COMMENT ON FUNCTION morpholog.declared_kind(text, text, jsonb, text, integer) IS 'morpholog declared kind guard v1';
