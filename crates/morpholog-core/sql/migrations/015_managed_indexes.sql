-- Migration 015: the registry behind `morpholog provision indexes`.
--
-- Static structure only. The indexes themselves are created by the
-- command, outside any transaction, from the specifications the SQL
-- compiler emits for a programme; this records which of them Morpholog
-- manages and which programmes require each. The catalogue remains the
-- truth about what physically exists. Neither table participates in
-- correctness: the compiled checks are right without any index.
--
-- Idempotent, like every migration, because an installation may carry
-- the tables without a record of this version - but never blessing a
-- table of the same name and another shape: that is refused here, not
-- discovered by the first provisioning run.

DO $$
DECLARE
    shape text;
BEGIN
    IF to_regclass('morpholog.managed_index') IS NOT NULL THEN
        SELECT string_agg(column_name || ':' || data_type, ',' ORDER BY ordinal_position)
          INTO shape
          FROM information_schema.columns
         WHERE table_schema = 'morpholog' AND table_name = 'managed_index';
        IF shape IS DISTINCT FROM
           'spec_digest:text,index_name:text,predicate_name:text,position:integer,representation:text,expression_sql:text,partial_predicate:text,registered_at:timestamp with time zone'
        THEN
            RAISE EXCEPTION 'morpholog.managed_index exists with another shape (%); migration 015 refuses to adopt it', shape;
        END IF;
    END IF;
    IF to_regclass('morpholog.index_requirement') IS NOT NULL THEN
        SELECT string_agg(column_name || ':' || data_type, ',' ORDER BY ordinal_position)
          INTO shape
          FROM information_schema.columns
         WHERE table_schema = 'morpholog' AND table_name = 'index_requirement';
        IF shape IS DISTINCT FROM
           'program_identity:text,spec_digest:text,program_hash:text,reconciled_at:timestamp with time zone'
        THEN
            RAISE EXCEPTION 'morpholog.index_requirement exists with another shape (%); migration 015 refuses to adopt it', shape;
        END IF;
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS morpholog.managed_index (
    spec_digest        text        PRIMARY KEY,
    index_name         text        NOT NULL UNIQUE,
    predicate_name     text        NOT NULL,
    position           integer     NOT NULL CHECK (position >= 0),
    representation     text        NOT NULL,
    expression_sql     text        NOT NULL,
    partial_predicate  text        NOT NULL,
    registered_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS morpholog.index_requirement (
    program_identity   text        NOT NULL,
    spec_digest        text        NOT NULL REFERENCES morpholog.managed_index (spec_digest),
    program_hash       text        NOT NULL,
    reconciled_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (program_identity, spec_digest)
);
