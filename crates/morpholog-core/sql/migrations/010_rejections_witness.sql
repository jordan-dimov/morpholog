-- The refusal witness on the rejection log.
--
-- A refused proposal records the values the refused rule was reading, in
-- the same tagged codec the envelope uses. Without this column a binary
-- that writes it fails on every refusal - the post-rollback insert names a
-- column that is not there, so the caller receives a database error where a
-- lawful rejection belongs, and `inspect rejections` fails too. Apply this
-- before running a binary from this release against an existing database.
--
-- Nullable, and deliberately never backfilled: a witness is what one
-- refusal reported at the moment it happened, so there is nothing to
-- reconstruct for rows written before the column existed. NULL there means
-- "not captured", which is the truth.
--
-- The CHECK forbids an empty array as well as a wrong type. Absence means
-- nothing was captured; `[]` would claim the rule was reading nothing,
-- which is never true of a refusal, so the two must not both be sayable.
--
-- Unlike 009, no NOT VALID activation constraint: a witness is genuinely
-- optional per row (a gate refusal has none, and neither does an invariant
-- refusal the kernel cannot pin to one iteration), so there is no
-- one-way switch to enforce. See crates/morpholog-core/sql/schema.sql for
-- the fresh-database definition.

ALTER TABLE morpholog.rejections
    ADD COLUMN IF NOT EXISTS witness jsonb;

ALTER TABLE morpholog.rejections
    DROP CONSTRAINT IF EXISTS rejections_witness_check;
ALTER TABLE morpholog.rejections
    ADD CONSTRAINT rejections_witness_check
    CHECK (
        witness IS NULL
        OR (jsonb_typeof(witness) = 'array' AND jsonb_array_length(witness) > 0)
    );

ALTER TABLE morpholog.rejections
    DROP CONSTRAINT IF EXISTS rejections_witness_is_invariant_only;
ALTER TABLE morpholog.rejections
    ADD CONSTRAINT rejections_witness_is_invariant_only
    CHECK (witness IS NULL OR kind = 'invariant');
