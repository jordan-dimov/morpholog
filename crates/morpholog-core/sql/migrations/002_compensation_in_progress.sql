-- Migration 002: compensation_in_progress status.
--
-- Apply this to an existing morpholog_dev database that was created
-- after migration 001 landed but before this one. Fresh installations
-- applying schema.sql already include this change and should NOT also
-- apply this migration (the schema is at the head; this file just
-- records the diff for upgraders).
--
-- Idempotent: re-running is safe (DROP IF EXISTS keeps the migration
-- safe to re-run).
--
-- What this migration does:
--   Extends outbox.status CHECK to allow 'compensation_in_progress',
--   the leased state during which a single worker holds exclusive
--   rights to invoke a compensating transformation for a failed row.
--
-- Why: the substrate that landed in migration 001 (mark_outbox_failed
-- releases the lease; record_compensation only protects the pointer)
-- did not on its own prevent duplicate compensation under worker
-- races. The PR that lands this migration introduces a lease over the
-- compensation window: failed -> compensation_in_progress (claimed
-- via SKIP LOCKED) -> failed (with compensation_transition_id set)
-- or compensation_in_progress -> compensation_failed (genuinely-
-- broken). At most one worker holds the compensation lease for a
-- given failed row, eliminating the duplicate-compensation race in
-- the normal-operation case. See docs/outbox-sketch.md.

SET search_path TO morpholog, public;

BEGIN;

ALTER TABLE outbox DROP CONSTRAINT IF EXISTS outbox_status_check;
ALTER TABLE outbox ADD CONSTRAINT outbox_status_check
    CHECK (status IN (
        'pending',
        'in_progress',
        'delivered',
        'failed',
        'compensation_in_progress',
        'compensation_failed'
    ));

COMMIT;
