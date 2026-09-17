-- Migration 015: the registry behind `morpholog provision indexes`.
--
-- Static structure only. The indexes themselves are created by the
-- command, outside any transaction, from the specifications the SQL
-- compiler emits for a programme; this records which of them Morpholog
-- manages and which programmes require each. The catalogue remains the
-- truth about what physically exists. Neither table participates in
-- correctness: the compiled checks are right without any index.

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
