-- Migration 003: composite partial index for earliest_pending_retry.
--
-- Apply this to an existing morpholog_dev database that was created
-- after migration 002 landed but before this one. Fresh installations
-- applying schema.sql already include this change.
--
-- Idempotent: re-running is safe (IF NOT EXISTS).
--
-- What this migration does:
--   Adds a composite partial index on (intent_type, next_attempt_at)
--   filtered to status='pending', supporting the polling worker's
--   earliest_pending_retry helper.
--
-- Why: earliest_pending_retry runs after every idle drain pass to
-- decide how long to sleep before the next poll. The pre-existing
-- partial index outbox_due_pending on (next_attempt_at) WHERE
-- status='pending' covers the claim filter (status + due) but not
-- the per-intent-type lookup the smart-sleep query does. With many
-- intent types interleaved, PostgreSQL would have to scan past
-- rows of other intent types to find this worker's minimum
-- next_attempt_at. The composite makes the lookup an index seek
-- per intent type instead.

SET search_path TO morpholog, public;

CREATE INDEX IF NOT EXISTS outbox_pending_intent_next_attempt
    ON outbox (intent_type, next_attempt_at)
    WHERE status = 'pending';
