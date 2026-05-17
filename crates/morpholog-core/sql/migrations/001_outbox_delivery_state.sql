-- Migration 001: outbox delivery-state vocabulary.
--
-- Apply this to an existing morpholog_dev database that was created
-- before this PR landed. Fresh installations applying schema.sql
-- already include these changes and should NOT also apply this
-- migration (the schema is at the head; this file just records the
-- diff for upgraders).
--
-- Idempotent: re-running is safe (IF NOT EXISTS / DROP IF EXISTS).
--
-- What this migration does:
--   1. Adds six nullable columns to morpholog.outbox covering the
--      delivery-state machine: failed_at, failure_reason,
--      next_attempt_at, compensation_transition_id, locked_by,
--      lock_expires_at.
--   2. Extends the outbox.status CHECK constraint to allow
--      'in_progress' (the leased state) and 'compensation_failed'
--      (the genuinely-broken state where the compensating
--      transformation was rejected by an invariant).
--   3. Adds a partial index on (next_attempt_at) for due-pending
--      rows, supporting the claim-only-due-rows worker pattern.
--
-- Sequencing: this is PR 1 of the outbox arc per
-- docs/outbox-sketch.md. Subsequent PRs build the helpers, the
-- single-row processor, the polling worker, and the supervisor.

SET search_path TO morpholog, public;

BEGIN;

-- Drop the old status CHECK so we can re-add it with the expanded
-- enumeration. There is no IF EXISTS for ADD CONSTRAINT, but DROP
-- accepts IF EXISTS, which keeps this migration safe to re-run.
ALTER TABLE outbox DROP CONSTRAINT IF EXISTS outbox_status_check;
ALTER TABLE outbox ADD CONSTRAINT outbox_status_check
    CHECK (status IN (
        'pending',
        'in_progress',
        'delivered',
        'failed',
        'compensation_failed'
    ));

-- Delivery-state columns. All nullable; populated as rows progress
-- through the state machine.
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS failed_at                  timestamptz;
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS failure_reason             text;
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS next_attempt_at            timestamptz;
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS compensation_transition_id uuid
    REFERENCES audit(transition_id);
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS locked_by                  text;
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS lock_expires_at            timestamptz;

-- Supports the worker's claim query: oldest due-pending row first.
CREATE INDEX IF NOT EXISTS outbox_due_pending ON outbox (next_attempt_at)
    WHERE status = 'pending';

COMMIT;
