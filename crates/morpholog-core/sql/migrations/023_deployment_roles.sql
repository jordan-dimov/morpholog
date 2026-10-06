-- The group roles this deployment's least-privilege floor grants to.
-- Roles belong to the whole cluster, so a database names its own here,
-- and `migrate` re-applies the floor to these roles and no others instead
-- of looking for a role by name. At most one row; none when the floor was
-- never provisioned.
--
-- Backfill: a database provisioned before this record existed used the
-- fixed names `morpholog_writer` and `morpholog_reader`. They are recorded
-- only when this database grants them, by name, the floor they were given:
-- schema usage, the claims, audit and outbox writes, and the reader's audit
-- read. Access through PUBLIC or through another role does not count, so
-- neither a stray grant nor a role's membership elsewhere is taken for a
-- provisioned floor. A database that does not match records nothing and
-- has no floor for `migrate` to re-apply; `init --least-privilege` can
-- provision it.
--
-- Guarded like 022: the head shape (columns, constraints and default) is
-- left alone, and any other table of that name is refused rather than
-- declared current.

DO $$
DECLARE
    shape text;
    granted text[];
BEGIN
    PERFORM set_config('search_path', 'pg_catalog', true);
    IF to_regclass('morpholog.deployment_roles') IS NOT NULL THEN
        SELECT concat_ws(' | ',
            (SELECT string_agg(attname || ' ' || format_type(atttypid, atttypmod)
                               || CASE WHEN attnotnull THEN ' not null' ELSE '' END,
                               ', ' ORDER BY attnum)
             FROM pg_attribute
             WHERE attrelid = 'morpholog.deployment_roles'::regclass
               AND attnum > 0 AND NOT attisdropped),
            (SELECT string_agg(pg_get_constraintdef(oid), '; ' ORDER BY contype, conname)
             FROM pg_constraint WHERE conrelid = 'morpholog.deployment_roles'::regclass),
            (SELECT string_agg(attname || ' default ' || pg_get_expr(adbin, adrelid), ', ')
             FROM pg_attrdef JOIN pg_attribute ON attrelid = adrelid AND attnum = adnum
             WHERE adrelid = 'morpholog.deployment_roles'::regclass))
        INTO shape;
        IF shape = 'singleton boolean not null, writer_role text not null, reader_role text not null'
                   ' | CHECK (singleton); NOT NULL reader_role; NOT NULL singleton;'
                   ' NOT NULL writer_role; PRIMARY KEY (singleton)'
                   ' | singleton default true' THEN
            RETURN;
        END IF;
        RAISE EXCEPTION 'morpholog.deployment_roles exists with another shape (%); refusing to guess', shape;
    END IF;

    CREATE TABLE morpholog.deployment_roles (
        singleton    boolean  PRIMARY KEY DEFAULT true CHECK (singleton),
        writer_role  text     NOT NULL,
        reader_role  text     NOT NULL
    );

    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'morpholog_writer')
       AND EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'morpholog_reader') THEN
        -- Only grants made to the role by name: aclexplode lists PUBLIC as
        -- grantee 0, and inheritance never appears in an ACL.
        SELECT array_agg(c.relname || ':' || r.rolname || ':' || a.privilege_type)
        INTO granted
        FROM pg_class c, aclexplode(c.relacl) a, pg_roles r
        WHERE c.relnamespace = 'morpholog'::regnamespace AND r.oid = a.grantee;
        SELECT array_cat(granted, array_agg('schema:' || r.rolname || ':' || a.privilege_type))
        INTO granted
        FROM pg_namespace n, aclexplode(n.nspacl) a, pg_roles r
        WHERE n.nspname = 'morpholog' AND r.oid = a.grantee;
        IF granted @> ARRAY[
            'schema:morpholog_writer:USAGE',
            'claims:morpholog_writer:INSERT',
            'claims:morpholog_writer:DELETE',
            'audit:morpholog_writer:INSERT',
            'outbox:morpholog_writer:UPDATE',
            'schema:morpholog_reader:USAGE',
            'audit:morpholog_reader:SELECT'
        ] THEN
            INSERT INTO morpholog.deployment_roles (writer_role, reader_role)
            VALUES ('morpholog_writer', 'morpholog_reader');
        END IF;
    END IF;
END
$$;
