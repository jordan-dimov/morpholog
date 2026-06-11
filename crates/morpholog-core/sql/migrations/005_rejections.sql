-- Migration 005: the operational rejection log.
--
-- Apply this to an existing morpholog_dev database created before the
-- rejection log landed. Fresh installations applying schema.sql
-- already include the table.
--
-- Idempotent: re-running is safe.
--
-- Until applied, every REJECTION (the normal business refusal path)
-- and every `inspect coverage` / `inspect rejections` surfaces as an
-- operational error, because the recording insert and the replay
-- read hit a missing table. Unlike earlier migrations, the breakage
-- lands on the refusal path embedders exercise constantly - apply
-- this before upgrading the binary.
--
-- What this migration does:
--   Creates morpholog.rejections, one row per refused proposal,
--   recording who proposed what and which rule refused it (kind +
--   rule name, structured at the source, plus the exact reason
--   string the envelope carried).
--
-- Why: a rejection rolls back its transaction, so until now it left
-- no trace at all. The record is written AFTER the rollback in a
-- separate autocommit insert - at-most-once, operational evidence,
-- never part of the legitimacy-grade audit record. It is the
-- substrate that upgrades `inspect coverage`'s `fired` verdict to
-- `constrained`: how often a rule actually refused a proposal.

SET search_path TO morpholog, public;

CREATE TABLE IF NOT EXISTS rejections (
    rejection_id         uuid         PRIMARY KEY,
    transformation_name  text         NOT NULL,
    arguments            jsonb        NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    actor                jsonb        NOT NULL,
    kind                 text         NOT NULL CHECK (kind IN ('invariant', 'require', 'bind')),
    rule                 text         NOT NULL,
    invariant_version    bigint,
    reason               text         NOT NULL,
    rejected_at          timestamptz  NOT NULL DEFAULT now(),
    CONSTRAINT rejections_kind_version_agree CHECK (
        (kind = 'invariant') = (invariant_version IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS rejections_rejected_at
    ON rejections (rejected_at, rejection_id);
