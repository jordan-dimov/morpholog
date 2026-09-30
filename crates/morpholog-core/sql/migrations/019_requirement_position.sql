-- Migration 019: each index requirement records the argument position its
-- specification seeks on. A requirement an operator's own index satisfies
-- has no managed index to read the position from, and the position is what
-- says whether a statistics object is still needed. Nullable: a binary from
-- before this migration keeps recording requirements without it, and a
-- requirement whose position is not recorded is resolved from the managed
-- index of the same specification where one exists, and is otherwise
-- unknown, which keeps every statistics object from being pruned until a
-- run under that programme's name records it. Idempotent: the column is
-- added only where absent, and an existing one must be exactly this one,
-- a nullable integer with no default and the non-negative check, or the
-- migration refuses to adopt it.

DO $$
DECLARE
    column_seen text;
    check_seen text;
BEGIN
    SELECT data_type || ':' || is_nullable || ':' || coalesce(column_default, '-')
      INTO column_seen
      FROM information_schema.columns
     WHERE table_schema = 'morpholog' AND table_name = 'index_requirement'
       AND column_name = 'position';
    IF column_seen IS NOT NULL THEN
        SELECT string_agg(pg_get_constraintdef(oid), ',' ORDER BY pg_get_constraintdef(oid))
          INTO check_seen
          FROM pg_constraint
         WHERE conrelid = 'morpholog.index_requirement'::regclass AND contype = 'c';
        IF column_seen IS DISTINCT FROM 'integer:YES:-'
           OR check_seen IS DISTINCT FROM 'CHECK (("position" >= 0))'
        THEN
            RAISE EXCEPTION 'morpholog.index_requirement.position exists with another shape (column %; checks %); migration 019 refuses to adopt it', column_seen, check_seen;
        END IF;
    END IF;
END $$;

ALTER TABLE morpholog.index_requirement
    ADD COLUMN IF NOT EXISTS position integer CHECK (position >= 0);
