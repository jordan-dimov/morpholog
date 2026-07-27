-- The record of which migrations a database has had applied.
--
-- Until now nothing recorded it: the upgrade instruction was "apply every
-- numbered file that postdates your database", which a human had to track,
-- and the binary could only discover a stale database by failing a query
-- partway through a workload. This table is what lets it be asked instead.
--
-- Deliberately plain substrate state rather than an admitted claim. Three
-- reasons, recorded so the roadmap's claim-shaped instinct is answered and
-- not merely dropped: the claims table is created by schema.sql, so the
-- early migrations could not be claims about a table that did not exist
-- yet; migrations are DDL on the substrate, not admissions about a business
-- subject; and audit's Merkle integrity should not rest on rows describing
-- the substrate that stores audit.
--
-- Backfill: a database reaching this migration predates the record, so
-- there is nothing to read and nothing to infer. Every migration up to and
-- including this one is marked applied, which is sound because they are all
-- idempotent - `morpholog migrate` runs them all on a database with no
-- record, then writes this table, and re-running any of them changes
-- nothing.

CREATE TABLE IF NOT EXISTS morpholog.schema_migrations (
    version     integer      PRIMARY KEY,
    name        text         NOT NULL,
    applied_at  timestamptz  NOT NULL DEFAULT now()
);

INSERT INTO morpholog.schema_migrations (version, name) VALUES
    (1,  'outbox_delivery_state'),
    (2,  'compensation_in_progress'),
    (3,  'outbox_intent_type_next_attempt_index'),
    (4,  'audit_actor'),
    (5,  'rejections'),
    (6,  'audit_keyset_index'),
    (7,  'derived_read_cache'),
    (8,  'checkpoint_signatures'),
    (9,  'audit_attestation'),
    (10, 'rejections_witness'),
    (11, 'schema_migrations')
ON CONFLICT (version) DO NOTHING;
