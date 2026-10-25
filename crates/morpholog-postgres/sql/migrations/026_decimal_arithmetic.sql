-- The kernel's decimal arithmetic for the compiled checks: one function
-- that fits an exact result as the kernel's operations do, one exact
-- division, and the kernel's rounding step by step. Idempotent: each
-- function is created only where absent, and an existing one must be
-- exactly this one, marker and body, or the migration refuses to touch it.

DO $migration$
DECLARE
    marker text;
    body_digest text;
BEGIN
    PERFORM set_config('search_path', 'pg_catalog', true);
    IF to_regprocedure('morpholog.decimal_fit_v1(numeric)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.decimal_fit_v1(x numeric) RETURNS numeric
                LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
            AS $$
DECLARE
    bound constant numeric := 79228162514264337593543950336;
    shifted numeric;
    q numeric;
    r numeric;
BEGIN
    IF min_scale(x) <= 28 AND abs(x) * ('1e' || min_scale(x))::numeric < bound THEN
        RETURN x;
    END IF;
    FOR s IN REVERSE 28..0 LOOP
        shifted := x * ('1e' || s)::numeric;
        q := trunc(shifted);
        r := abs(shifted - q);
        IF r > 0.5 OR (r = 0.5 AND mod(q, 2) <> 0) THEN
            q := q + sign(x);
        END IF;
        IF abs(q) < bound THEN
            RETURN q * ('1e-' || s)::numeric;
        END IF;
    END LOOP;
    RETURN NULL;
END
$$
        $create$;
        COMMENT ON FUNCTION morpholog.decimal_fit_v1(numeric) IS 'morpholog decimal fit v1';
    ELSE
        SELECT obj_description(oid, 'pg_proc'), encode(sha256(convert_to(prosrc, 'UTF8')), 'hex')
          INTO marker, body_digest
          FROM pg_proc
         WHERE oid = 'morpholog.decimal_fit_v1(numeric)'::regprocedure;
        IF marker IS DISTINCT FROM 'morpholog decimal fit v1'
           OR body_digest IS DISTINCT FROM 'e5a2f7c487834d06202844cd14a0c5240c953191dd4e3d3ff32f6c0c6b391fcb' THEN
            RAISE EXCEPTION 'morpholog.decimal_fit_v1 exists and is not the one migration 026 defines (marker %, body %); refusing to replace it', coalesce(marker, 'none'), body_digest;
        END IF;
    END IF;
    IF to_regprocedure('morpholog.decimal_div_v1(numeric, numeric)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.decimal_div_v1(a numeric, b numeric) RETURNS numeric
                LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
            AS $$
DECLARE
    bound constant numeric := 79228162514264337593543950336;
    n numeric;
    q numeric;
    r numeric;
BEGIN
    IF b = 0 THEN
        RETURN NULL;
    END IF;
    FOR s IN REVERSE 28..0 LOOP
        n := a * ('1e' || s)::numeric;
        q := div(n, b);
        r := abs(n - q * b);
        IF 2 * r > abs(b) OR (2 * r = abs(b) AND mod(q, 2) <> 0) THEN
            q := q + sign(a) * sign(b);
        END IF;
        IF abs(q) < bound THEN
            RETURN q * ('1e-' || s)::numeric;
        END IF;
    END LOOP;
    RETURN NULL;
END
$$
        $create$;
        COMMENT ON FUNCTION morpholog.decimal_div_v1(numeric, numeric) IS 'morpholog decimal division v1';
    ELSE
        SELECT obj_description(oid, 'pg_proc'), encode(sha256(convert_to(prosrc, 'UTF8')), 'hex')
          INTO marker, body_digest
          FROM pg_proc
         WHERE oid = 'morpholog.decimal_div_v1(numeric, numeric)'::regprocedure;
        IF marker IS DISTINCT FROM 'morpholog decimal division v1'
           OR body_digest IS DISTINCT FROM 'fae7f1085eb957c13df8baffbfafbda55b27099e8a765b353cbe25c5172e5421' THEN
            RAISE EXCEPTION 'morpholog.decimal_div_v1 exists and is not the one migration 026 defines (marker %, body %); refusing to replace it', coalesce(marker, 'none'), body_digest;
        END IF;
    END IF;
    IF to_regprocedure('morpholog.decimal_round_v1(numeric, numeric)') IS NULL THEN
        EXECUTE $create$
            CREATE FUNCTION morpholog.decimal_round_v1(v numeric, q numeric) RETURNS numeric
                LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
            AS $$
DECLARE
    rem numeric;
    toward numeric;
    away numeric;
    doubled numeric;
BEGIN
    IF q <= 0 THEN
        RETURN NULL;
    END IF;
    rem := mod(v, q);
    IF rem = 0 THEN
        RETURN v;
    END IF;
    toward := morpholog.decimal_fit_v1(v - rem);
    IF toward IS NULL THEN
        RETURN NULL;
    END IF;
    IF v < 0 THEN
        away := morpholog.decimal_fit_v1(toward - q);
    ELSE
        away := morpholog.decimal_fit_v1(toward + q);
    END IF;
    doubled := morpholog.decimal_fit_v1(abs(rem) + abs(rem));
    IF doubled IS NULL OR doubled >= q THEN
        RETURN away;
    END IF;
    RETURN toward;
END
$$
        $create$;
        COMMENT ON FUNCTION morpholog.decimal_round_v1(numeric, numeric) IS 'morpholog decimal round v1';
    ELSE
        SELECT obj_description(oid, 'pg_proc'), encode(sha256(convert_to(prosrc, 'UTF8')), 'hex')
          INTO marker, body_digest
          FROM pg_proc
         WHERE oid = 'morpholog.decimal_round_v1(numeric, numeric)'::regprocedure;
        IF marker IS DISTINCT FROM 'morpholog decimal round v1'
           OR body_digest IS DISTINCT FROM '16f7b9e7225e64a8ac490edc7690dd12811faad410e08594152fed858d386c64' THEN
            RAISE EXCEPTION 'morpholog.decimal_round_v1 exists and is not the one migration 026 defines (marker %, body %); refusing to replace it', coalesce(marker, 'none'), body_digest;
        END IF;
    END IF;
END $migration$;
