-- Attestation lineage on the audit record.
--
-- Every commit records how the actor identity was established, e.g.
-- {"mode":"gateway","authenticated_by":"<login role>"} - the role
-- PostgreSQL authenticated for the proposing connection. The field is
-- part of the Merkle leaf for the rows that carry it.
--
-- NEVER backfill this column. A row's leaf encoding is chosen by the
-- field's presence: rows written before attestation existed hash under
-- the original encoding, and rewriting them would invalidate every
-- checkpoint root computed over them. See
-- crates/morpholog-core/sql/schema.sql for the canonical definition.

ALTER TABLE morpholog.audit
    ADD COLUMN IF NOT EXISTS attestation jsonb
        CHECK (attestation IS NULL OR jsonb_typeof(attestation) = 'object');
