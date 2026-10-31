-- The order's sequences, the per-transaction table and the functions.
-- Nothing here changes how a row is written until the trigger exists.
CREATE SEQUENCE IF NOT EXISTS morpholog.audit_order_seq CACHE 1;
-- Microseconds since the epoch of the last `ordered_at` handed out.
CREATE SEQUENCE IF NOT EXISTS morpholog.audit_ordered_at_us CACHE 1 MINVALUE 0 START 0;

-- The order each open transaction drew, keyed by an id its caller cannot
-- choose. Rows of finished transactions are pruned as orders are drawn.
CREATE TABLE IF NOT EXISTS morpholog.audit_order_txn (
    xid         xid8        PRIMARY KEY,
    audit_order bigint      NOT NULL,
    next_act    int         NOT NULL,
    ordered_at  timestamptz NOT NULL
);
REVOKE ALL ON morpholog.audit_order_txn FROM PUBLIC;

-- The ordering lock G is the advisory session lock (625626, 0). An
-- order n is held as the transaction lock on the bigint (1 << 62) | n
-- until commit. Both keys are reserved.
CREATE OR REPLACE FUNCTION morpholog.audit_order_assign() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = morpholog, pg_temp AS $$
DECLARE
    -- This transaction's own row, found by its address: a search of the
    -- table would conflict, under serializable isolation, with every
    -- other writer's insert.
    held tid := nullif(current_setting('morpholog.audit_order_row', true), '')::tid;
    n bigint;
    us bigint;
BEGIN
    IF NEW.operations IS NULL THEN
        RAISE EXCEPTION 'this audit row records no operations: it was written by a binary older than the database'
            USING ERRCODE = '55000';
    END IF;
    IF held IS NOT NULL THEN
        UPDATE audit_order_txn SET next_act = next_act + 1
         WHERE ctid = held AND xid = pg_current_xact_id()
         RETURNING audit_order, next_act - 1, ordered_at, ctid
              INTO NEW.audit_order, NEW.act_ordinal, NEW.ordered_at, held;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'this transaction names an audit order that is not its own'
                USING ERRCODE = '55000';
        END IF;
    ELSE
        -- G is released on every path, a cancel included, so a pooled
        -- connection never keeps it.
        BEGIN
            PERFORM pg_advisory_lock(625626, 0);
            n := nextval('audit_order_seq');
            PERFORM pg_advisory_xact_lock((1::bigint << 62) | n);
            SELECT greatest(
                       floor(extract(epoch FROM clock_timestamp()) * 1000000)::bigint,
                       last_value + 1)
              INTO us FROM audit_ordered_at_us;
            PERFORM setval('audit_ordered_at_us', us);
            PERFORM pg_advisory_unlock(625626, 0);
        EXCEPTION WHEN OTHERS OR QUERY_CANCELED THEN
            PERFORM pg_advisory_unlock(625626, 0);
            RAISE;
        END;
        NEW.audit_order := n;
        NEW.act_ordinal := 0;
        NEW.ordered_at := timestamptz 'epoch' + us * interval '1 microsecond';
        INSERT INTO audit_order_txn VALUES (pg_current_xact_id(), n, 1, NEW.ordered_at)
            RETURNING ctid INTO held;
        -- Open and prepared transactions are at or above the snapshot's
        -- xmin, so only finished ones are removed.
        IF n % 64 = 0 THEN
            DELETE FROM audit_order_txn WHERE xid IN (
                SELECT xid FROM audit_order_txn
                 WHERE xid < pg_snapshot_xmin(pg_current_snapshot())
                 LIMIT 256);
        END IF;
    END IF;
    PERFORM set_config('morpholog.audit_order_row', held::text, true);
    -- The activation is order 1 alone: its baseline chunks at ordinals
    -- 0 to k-1, then its record at k. Each control row's ordinal is the
    -- number of chunks before it.
    IF NEW.transformation_name IN ('morpholog.activate', 'morpholog.baseline')
       AND (NEW.audit_order <> 1
            OR NEW.act_ordinal <> (SELECT count(*) FROM audit
                                    WHERE audit_order = 1
                                      AND transformation_name = 'morpholog.baseline')) THEN
        RAISE EXCEPTION 'the activation rows can only be the first order, chunks then record'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION morpholog.audit_order_assign() FROM PUBLIC;

-- Every order below the result is final: committed or rolled back. It is
-- the least order still held, so a prepared transaction counts too, or
-- else the next order to be drawn.
CREATE OR REPLACE FUNCTION morpholog.audit_watermark() RETURNS bigint
LANGUAGE plpgsql SECURITY DEFINER SET search_path = morpholog, pg_temp AS $$
DECLARE
    inflight bigint;
    last bigint;
    called boolean;
BEGIN
    BEGIN
        PERFORM pg_advisory_lock(625626, 0);
        SELECT min(((classid::bigint & 1073741823) << 32) | objid::bigint)
          INTO inflight FROM pg_catalog.pg_locks
         WHERE locktype = 'advisory'
           AND database = (SELECT oid FROM pg_catalog.pg_database
                            WHERE datname = current_database())
           AND objsubid = 1 AND classid::bigint BETWEEN 1073741824 AND 2147483647
           AND mode = 'ExclusiveLock' AND granted;
        SELECT last_value, is_called INTO last, called FROM audit_order_seq;
        PERFORM pg_advisory_unlock(625626, 0);
    EXCEPTION WHEN OTHERS OR QUERY_CANCELED THEN
        PERFORM pg_advisory_unlock(625626, 0);
        RAISE;
    END;
    RETURN coalesce(inflight, CASE WHEN called THEN last + 1 ELSE last END);
END $$;
REVOKE ALL ON FUNCTION morpholog.audit_watermark() FROM PUBLIC;

-- The watermark with the quiet case made final. When no order is held,
-- the ordering clock is moved up to the server clock, under G, and that
-- floor is returned: every order drawn later takes an instant above it
-- even if the clock steps back. The sequence never moves down, and
-- setval takes effect at once, so a cancelled call leaves only the
-- floor. It writes, so it runs outside a read-only transaction, on the
-- primary.
CREATE OR REPLACE FUNCTION morpholog.audit_horizon(OUT audit_order bigint, OUT quiet_at timestamptz)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = morpholog, pg_temp AS $$
DECLARE
    inflight bigint;
    last bigint;
    called boolean;
    floor_us bigint;
BEGIN
    BEGIN
        PERFORM pg_advisory_lock(625626, 0);
        SELECT min(((classid::bigint & 1073741823) << 32) | objid::bigint)
          INTO inflight FROM pg_catalog.pg_locks
         WHERE locktype = 'advisory'
           AND database = (SELECT oid FROM pg_catalog.pg_database
                            WHERE datname = current_database())
           AND objsubid = 1 AND classid::bigint BETWEEN 1073741824 AND 2147483647
           AND mode = 'ExclusiveLock' AND granted;
        SELECT last_value, is_called INTO last, called FROM audit_order_seq;
        IF inflight IS NULL THEN
            SELECT greatest(last_value,
                            floor(extract(epoch FROM clock_timestamp()) * 1000000)::bigint)
              INTO floor_us FROM audit_ordered_at_us;
            PERFORM setval('audit_ordered_at_us', floor_us);
            quiet_at := timestamptz 'epoch' + floor_us * interval '1 microsecond';
        END IF;
        PERFORM pg_advisory_unlock(625626, 0);
    EXCEPTION WHEN OTHERS OR QUERY_CANCELED THEN
        PERFORM pg_advisory_unlock(625626, 0);
        RAISE;
    END;
    audit_order := coalesce(inflight, CASE WHEN called THEN last + 1 ELSE last END);
END $$;
REVOKE ALL ON FUNCTION morpholog.audit_horizon() FROM PUBLIC;
