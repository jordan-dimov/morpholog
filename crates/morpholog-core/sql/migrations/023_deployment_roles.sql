-- The group roles this deployment's least-privilege floor grants to.
-- Roles belong to the whole cluster, so a database names its own here,
-- and `migrate` re-applies the floor to these roles and no others instead
-- of looking for a role by name. At most one row; none when the floor was
-- never provisioned.
--
-- Backfill: a database provisioned before this record existed used the
-- fixed names `morpholog_writer` and `morpholog_reader`. They are recorded
-- only when this database shows the floor they were given - schema usage,
-- the claims, audit and outbox writes, and the reader's audit read - so a
-- stray grant to a role of that name is not taken for a provisioned floor.
-- A database that does not match records nothing and has no floor for
-- `migrate` to re-apply; `init --least-privilege` can provision it.
--
-- Guarded like 022: the head shape is left alone, and any other table of
-- that name is refused rather than declared current.

DO $$
DECLARE
    shape text;
BEGIN
    PERFORM set_config('search_path', 'pg_catalog', true);
    IF to_regclass('morpholog.deployment_roles') IS NOT NULL THEN
        SELECT string_agg(attname || ' ' || format_type(atttypid, atttypmod)
                          || CASE WHEN attnotnull THEN ' not null' ELSE '' END, ', ' ORDER BY attnum)
        INTO shape
        FROM pg_attribute
        WHERE attrelid = 'morpholog.deployment_roles'::regclass AND attnum > 0 AND NOT attisdropped;
        IF shape = 'singleton boolean not null, writer_role text not null, reader_role text not null' THEN
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
        IF has_schema_privilege('morpholog_writer', 'morpholog', 'USAGE')
           AND has_table_privilege('morpholog_writer', 'morpholog.claims', 'INSERT')
           AND has_table_privilege('morpholog_writer', 'morpholog.claims', 'DELETE')
           AND has_table_privilege('morpholog_writer', 'morpholog.audit', 'INSERT')
           AND has_table_privilege('morpholog_writer', 'morpholog.outbox', 'UPDATE')
           AND has_schema_privilege('morpholog_reader', 'morpholog', 'USAGE')
           AND has_table_privilege('morpholog_reader', 'morpholog.audit', 'SELECT') THEN
            INSERT INTO morpholog.deployment_roles (writer_role, reader_role)
            VALUES ('morpholog_writer', 'morpholog_reader');
        END IF;
    END IF;
END
$$;
