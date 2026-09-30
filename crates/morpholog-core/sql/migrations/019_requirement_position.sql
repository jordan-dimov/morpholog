-- Migration 019: each index requirement records the argument position its
-- specification seeks on. A requirement an operator's own index satisfies
-- has no managed index to read the position from, and the position is what
-- says whether a statistics object is still needed. Nullable: a binary from
-- before this migration keeps recording requirements without it, and a
-- requirement whose position is not recorded is resolved from the managed
-- index of the same specification where one exists, and is otherwise
-- unknown, which keeps every statistics object from being pruned until a
-- run under that programme's name records it. Idempotent: the column is
-- added only where absent, and an existing one must be an integer.

DO $$
DECLARE
    seen text;
BEGIN
    SELECT data_type INTO seen
      FROM information_schema.columns
     WHERE table_schema = 'morpholog' AND table_name = 'index_requirement'
       AND column_name = 'position';
    IF seen IS NOT NULL AND seen <> 'integer' THEN
        RAISE EXCEPTION 'morpholog.index_requirement.position exists as %; migration 019 refuses to adopt it', seen;
    END IF;
END $$;

ALTER TABLE morpholog.index_requirement
    ADD COLUMN IF NOT EXISTS position integer CHECK (position >= 0);
