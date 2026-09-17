-- Migration 015: the registry behind `morpholog provision indexes`.
--
-- Static structure only. The indexes themselves are created by the
-- command, outside any transaction, from the specifications the SQL
-- compiler emits for a programme. Two facts, kept apart: what Morpholog
-- manages (and so may delete), and which programmes require which
-- specification - whether Morpholog built the index or an operator's
-- equivalent one satisfies it. The catalogue remains the truth about
-- what physically exists. Neither table participates in correctness:
-- the compiled checks are right without any index.
--
-- Idempotent, like every migration, because an installation may carry
-- the tables without a record of this version - but never blessing a
-- table of the same name and another shape. The shape the command
-- relies on is pinned whole: columns with nullability and defaults,
-- and every constraint as PostgreSQL renders it.

DO $$
DECLARE
    columns_seen text;
    constraints_seen text;
BEGIN
    IF to_regclass('morpholog.managed_index') IS NOT NULL THEN
        SELECT string_agg(column_name || ':' || data_type || ':' || is_nullable || ':' || coalesce(column_default, '-'), ',' ORDER BY ordinal_position)
          INTO columns_seen
          FROM information_schema.columns
         WHERE table_schema = 'morpholog' AND table_name = 'managed_index';
        SELECT string_agg(contype::text || ':' || pg_get_constraintdef(oid), ',' ORDER BY contype::text, pg_get_constraintdef(oid))
          INTO constraints_seen
          FROM pg_constraint
         WHERE conrelid = 'morpholog.managed_index'::regclass;
        IF columns_seen IS DISTINCT FROM
           'spec_digest:text:NO:-,index_name:text:NO:-,predicate_name:text:NO:-,position:integer:NO:-,representation:text:NO:-,expression_sql:text:NO:-,partial_predicate:text:NO:-,registered_at:timestamp with time zone:NO:now()'
           OR constraints_seen IS DISTINCT FROM
           'c:CHECK (("position" >= 0)),n:NOT NULL expression_sql,n:NOT NULL index_name,n:NOT NULL partial_predicate,n:NOT NULL "position",n:NOT NULL predicate_name,n:NOT NULL registered_at,n:NOT NULL representation,n:NOT NULL spec_digest,p:PRIMARY KEY (spec_digest),u:UNIQUE (index_name)'
        THEN
            RAISE EXCEPTION 'morpholog.managed_index exists with another shape (columns %; constraints %); migration 015 refuses to adopt it', columns_seen, constraints_seen;
        END IF;
    END IF;
    IF to_regclass('morpholog.index_requirement') IS NOT NULL THEN
        SELECT string_agg(column_name || ':' || data_type || ':' || is_nullable || ':' || coalesce(column_default, '-'), ',' ORDER BY ordinal_position)
          INTO columns_seen
          FROM information_schema.columns
         WHERE table_schema = 'morpholog' AND table_name = 'index_requirement';
        SELECT string_agg(contype::text || ':' || pg_get_constraintdef(oid), ',' ORDER BY contype::text, pg_get_constraintdef(oid))
          INTO constraints_seen
          FROM pg_constraint
         WHERE conrelid = 'morpholog.index_requirement'::regclass;
        IF columns_seen IS DISTINCT FROM
           'program_identity:text:NO:-,spec_digest:text:NO:-,program_hash:text:NO:-,reconciled_at:timestamp with time zone:NO:now()'
           OR constraints_seen IS DISTINCT FROM
           'n:NOT NULL program_hash,n:NOT NULL program_identity,n:NOT NULL reconciled_at,n:NOT NULL spec_digest,p:PRIMARY KEY (program_identity, spec_digest)'
        THEN
            RAISE EXCEPTION 'morpholog.index_requirement exists with another shape (columns %; constraints %); migration 015 refuses to adopt it', columns_seen, constraints_seen;
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
    spec_digest        text        NOT NULL,
    program_hash       text        NOT NULL,
    reconciled_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (program_identity, spec_digest)
);
