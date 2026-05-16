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
    invariant_epoch      int          NOT NULL,           -- v0: always 1
    invariants_checked   jsonb        NOT NULL,           -- [{name, version}]
    asserted_claims      jsonb        NOT NULL,           -- list of [predicate, args]
    retracted_claims     jsonb        NOT NULL,
    emitted_intents      jsonb        NOT NULL,           -- summary; rows in outbox
    committed_at         timestamptz  NOT NULL DEFAULT now()
);

CREATE INDEX audit_committed_at ON audit (committed_at);


-- Post-commit intents. Workers poll, deliver at-least-once,
-- update status. Duplicate delivery is the workers' problem
-- to tolerate via idempotency_key.
CREATE TABLE outbox (
    intent_id        uuid         PRIMARY KEY,                 -- UUIDv7
    transition_id    uuid         NOT NULL REFERENCES audit(transition_id),
    intent_type      text         NOT NULL,
    arguments        jsonb        NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    idempotency_key  text         NOT NULL UNIQUE,
    status           text         NOT NULL DEFAULT 'pending'
                                  CHECK (status IN ('pending','delivered','failed')),
    attempt_count    int          NOT NULL DEFAULT 0,
    enqueued_at      timestamptz  NOT NULL DEFAULT now(),
    last_attempt_at  timestamptz,
    delivered_at     timestamptz
);

-- Workers poll: oldest pending first.
CREATE INDEX outbox_pending ON outbox (enqueued_at) WHERE status = 'pending';
