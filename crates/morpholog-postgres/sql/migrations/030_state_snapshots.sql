-- The admitted state at settled places in the audit log, so an as-of
-- read replays only the rows after the nearest one. A cache built from
-- the log on request, never written by a commit and never evidence: a
-- read that finds none, or one it cannot use, replays the whole log.
CREATE TABLE morpholog.state_snapshots (
    snapshot_id    bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- The place in the log the state is as of, in the replay's order.
    audit_order    bigint      NOT NULL,
    act_ordinal    int         NOT NULL,
    committed_at   timestamptz NOT NULL,
    transition_id  uuid        NOT NULL,
    -- How the claims below encode the state; a read ignores any other.
    format         int         NOT NULL,
    claim_count    bigint      NOT NULL CHECK (claim_count >= 0),
    built_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (audit_order, act_ordinal, committed_at, transition_id, format)
);

-- One row per live claim, in the replay's order, with the transition
-- that admitted it while absent.
CREATE TABLE morpholog.state_snapshot_claims (
    snapshot_id     bigint NOT NULL REFERENCES morpholog.state_snapshots ON DELETE CASCADE,
    ordinal         bigint NOT NULL,
    predicate_name  text   NOT NULL,
    arguments       jsonb  NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    asserted_in     uuid   NOT NULL,
    PRIMARY KEY (snapshot_id, ordinal)
);
