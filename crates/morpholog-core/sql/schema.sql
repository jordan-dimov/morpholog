-- Morpholog v0 PostgreSQL schema.
--
-- Three tables: claims (admitted state), audit (causal log of committed
-- transformations), outbox (post-commit intents).
--
-- A claim is an admitted assertion, not objective reality. It is
-- set-valued: identity is (predicate_name, arguments). The runtime
-- serialises arguments as a JSONB array of positional values; decimals
-- are serialised as JSON strings to avoid float-precision drift.


CREATE SCHEMA IF NOT EXISTS morpholog;

SET search_path TO morpholog, public;


-- Admitted state. Each row is one admitted claim.
-- The primary key enforces set semantics: assert C where C is
-- already present is a no-op; retract C where C is missing fails.
CREATE TABLE claims (
    predicate_name  text        NOT NULL,
    arguments       jsonb       NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    asserted_in     uuid        NOT NULL,
    asserted_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (predicate_name, arguments)
);

-- Find all claims admitted by a given transition.
CREATE INDEX claims_asserted_in ON claims (asserted_in);


-- One row per committed transformation. Failed transformations
-- produce no audit row (they may be written to a separate
-- operational rejection log later, out of v0 scope).
CREATE TABLE audit (
    transition_id        uuid         PRIMARY KEY,        -- UUIDv7, app-generated
    transformation_name  text         NOT NULL,
    arguments            jsonb        NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    -- The actor under whose authority this transition was proposed.
    -- JSONB-encoded EvalValue (typically Subject); the codec writes
    -- {"type":"subject","value":"..."} for the v0 actor shape.
    actor                jsonb        NOT NULL,
    invariant_epoch      int          NOT NULL,           -- v0: always 1
    invariants_checked   jsonb        NOT NULL,           -- [{name, version}]
    asserted_claims      jsonb        NOT NULL,           -- JSONB array of {predicate, args} objects
    retracted_claims     jsonb        NOT NULL,           -- JSONB array of {predicate, args} objects
    emitted_intents      jsonb        NOT NULL,           -- summary; rows in outbox
    committed_at         timestamptz  NOT NULL DEFAULT now()
);

CREATE INDEX audit_committed_at ON audit (committed_at);


-- Post-commit intents. Workers poll, deliver at-least-once,
-- update status. Duplicate delivery is the workers' problem
-- to tolerate via idempotency_key.
--
-- Delivery-state columns (failed_at, failure_reason, next_attempt_at,
-- compensation_transition_id, locked_by, lock_expires_at) are nullable;
-- they fill in as a row moves through the delivery state machine.
-- See docs/outbox-sketch.md for the sequencing.
CREATE TABLE outbox (
    intent_id                    uuid         PRIMARY KEY,
    transition_id                uuid         NOT NULL REFERENCES audit(transition_id),
    intent_type                  text         NOT NULL,
    arguments                    jsonb        NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    idempotency_key              text         NOT NULL UNIQUE,
    status                       text         NOT NULL DEFAULT 'pending'
                                              CHECK (status IN (
                                                  'pending',
                                                  'in_progress',
                                                  'delivered',
                                                  'failed',
                                                  'compensation_in_progress',
                                                  'compensation_failed'
                                              )),
    attempt_count                int          NOT NULL DEFAULT 0,
    enqueued_at                  timestamptz  NOT NULL DEFAULT now(),
    last_attempt_at              timestamptz,
    delivered_at                 timestamptz,
    -- Delivery-state extensions (see docs/outbox-sketch.md).
    failed_at                    timestamptz,
    failure_reason               text,
    next_attempt_at              timestamptz,
    compensation_transition_id   uuid         REFERENCES audit(transition_id),
    locked_by                    text,
    lock_expires_at              timestamptz
);

-- Workers poll: oldest pending first. The `next_attempt_at`
-- column is also consulted at claim time so a row whose retry is
-- not yet due is skipped.
CREATE INDEX outbox_pending ON outbox (enqueued_at) WHERE status = 'pending';

-- Workers claim only rows whose `next_attempt_at` is due. This
-- index supports that filter cheaply for the pending+due case.
CREATE INDEX outbox_due_pending ON outbox (next_attempt_at)
    WHERE status = 'pending';

-- Supports earliest_pending_retry(intent_type), which the polling
-- worker calls after every idle drain to clamp its post-drain
-- sleep to the soonest scheduled retry. With multiple intent
-- types interleaved in the outbox, a single-column index on
-- next_attempt_at requires scanning past rows of other intent
-- types; the composite makes the lookup an index seek per intent
-- type instead.
CREATE INDEX outbox_pending_intent_next_attempt
    ON outbox (intent_type, next_attempt_at)
    WHERE status = 'pending';
