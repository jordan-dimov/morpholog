-- Ed25519 signatures over an audit tree head.
--
-- A checkpoint commits to a prefix of the audit log; signing the tree
-- head makes an externally held anchor attributable - tampering then
-- needs the private key, not just write access. Each signature is
-- {key_id, purpose, public_key, signature}; `[]` is an unsigned
-- checkpoint, which stays valid. See crates/morpholog-core/sql/schema.sql
-- for the canonical definition; this migration adds the column to a
-- database provisioned before signing landed.

ALTER TABLE morpholog.audit_checkpoints
    ADD COLUMN IF NOT EXISTS signatures jsonb NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(signatures) = 'array');
