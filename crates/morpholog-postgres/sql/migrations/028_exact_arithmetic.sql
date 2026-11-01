-- Exact decimal arithmetic for the compiled checks: a function that
-- keeps an exact result only where a decimal holds it, and the kernel's
-- `round` over exact steps, judged on the multiple it selects. The
-- fitting functions of migration 026 stay for division. Idempotent: each
-- function is created only where absent, and an existing one must be
-- exactly this one, marker and body, or the migration refuses to touch it.

DO $migration$
DECLARE
    marker text;
    body_digest text;
BEGIN
    PERFORM set_config('search_path', 'pg_catalog', true);
    IF to_regprocedure('morpholog.decimal_exact_v1(numeric)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.decimal_exact_v1(x numeric) RETURNS numeric
                LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
            AS $$
BEGIN
    IF min_scale(x) <= 28 AND abs(x) * ('1e' || min_scale(x))::numeric < 79228162514264337593543950336 THEN
        RETURN x;
    END IF;
    RETURN NULL;
END
$$
        $create$;
        COMMENT ON FUNCTION morpholog.decimal_exact_v1(numeric) IS 'morpholog decimal exact v1';
    ELSE
        SELECT obj_description(oid, 'pg_proc'), encode(sha256(convert_to(prosrc, 'UTF8')), 'hex')
          INTO marker, body_digest
          FROM pg_proc
         WHERE oid = 'morpholog.decimal_exact_v1(numeric)'::regprocedure;
        IF marker IS DISTINCT FROM 'morpholog decimal exact v1'
           OR body_digest IS DISTINCT FROM '9630c9c0ec179d52b04bc224135024e70d8d9b2f477f4e1b7135685f805dfc06' THEN
            RAISE EXCEPTION 'morpholog.decimal_exact_v1 exists and is not the one migration 028 defines (marker %, body %); refusing to replace it', coalesce(marker, 'none'), body_digest;
        END IF;
    END IF;
    IF to_regprocedure('morpholog.decimal_round_v2(numeric, numeric)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.decimal_round_v2(v numeric, q numeric) RETURNS numeric
                LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
            AS $$
DECLARE
    rem numeric;
    toward numeric;
    away numeric;
BEGIN
    IF q <= 0 THEN
        RETURN NULL;
    END IF;
    rem := mod(v, q);
    IF rem = 0 THEN
        RETURN v;
    END IF;
    toward := v - rem;
    IF v < 0 THEN
        away := toward - q;
    ELSE
        away := toward + q;
    END IF;
    IF abs(rem) + abs(rem) >= q THEN
        RETURN morpholog.decimal_exact_v1(away);
    END IF;
    RETURN morpholog.decimal_exact_v1(toward);
END
$$
        $create$;
        COMMENT ON FUNCTION morpholog.decimal_round_v2(numeric, numeric) IS 'morpholog decimal round v2';
    ELSE
        SELECT obj_description(oid, 'pg_proc'), encode(sha256(convert_to(prosrc, 'UTF8')), 'hex')
          INTO marker, body_digest
          FROM pg_proc
         WHERE oid = 'morpholog.decimal_round_v2(numeric, numeric)'::regprocedure;
        IF marker IS DISTINCT FROM 'morpholog decimal round v2'
           OR body_digest IS DISTINCT FROM '9e19749fa487fbf237b7954cc9bfea3e31029c065c1ab9dfac76b146f894413d' THEN
            RAISE EXCEPTION 'morpholog.decimal_round_v2 exists and is not the one migration 028 defines (marker %, body %); refusing to replace it', coalesce(marker, 'none'), body_digest;
        END IF;
    END IF;
END
$migration$;
