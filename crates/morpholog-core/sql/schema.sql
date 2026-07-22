-- Morpholog v0 PostgreSQL schema.
--
-- Tables: claims (admitted state), audit (causal log of committed
-- transformations), outbox (post-commit intents), rejections
-- (operational log of refused proposals).
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


-- One row per committed transformation. Rejected proposals produce
-- no audit row - they are recorded in `rejections` below, which is
-- operational evidence, never part of this legitimacy-grade record.
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
    committed_at         timestamptz  NOT NULL DEFAULT now(),
    -- How the actor identity was established, e.g.
    -- {"mode":"gateway","authenticated_by":"<login role>"} - the
    -- attestation lineage the runtime records for every commit. NULL
    -- on rows written before attestation existed; those rows keep the
    -- original Merkle leaf encoding, so the column is never backfilled.
    attestation          jsonb        CHECK (attestation IS NULL
                                             OR jsonb_typeof(attestation) = 'object')
);

-- Keyset replay order: every audit read (the blessed tail, verify,
-- coverage, as-of) orders and pages by this pair.
CREATE INDEX audit_committed_at ON audit (committed_at, transition_id);


-- Tamper-evident checkpoints over the audit log: a signed-tree-head
-- style commitment to a prefix of the log. Each checkpoint records the
-- RFC 6962 Merkle root of the first `tree_size` audit rows (in
-- (committed_at, transition_id) order). The prefix is bounded by the
-- audit resume watermark, so it is append-only-stable: no later writer
-- can insert inside an already-checkpointed prefix. The checkpoints
-- themselves form a hash chain (`checkpoint_hash` commits to the prior
-- one), so forging one historical root requires re-forging every later
-- checkpoint - and any externally-published root makes even that
-- detectable. The audit log stays untouched; leaves are recomputed.
CREATE TABLE audit_checkpoints (
    checkpoint_id         uuid         PRIMARY KEY,        -- UUIDv7
    -- Number of audit rows this checkpoint commits to (the RFC 6962
    -- tree size), counted in the canonical (committed_at, transition_id)
    -- order, watermark-bounded. UNIQUE: one root per prefix length, so
    -- two checkpoint runs cannot record diverging roots at the same size.
    tree_size             bigint       NOT NULL UNIQUE CHECK (tree_size >= 0),
    -- The Merkle Tree Hash of those rows, rendered `sha256:<hex>`.
    root_hash             text         NOT NULL CHECK (root_hash ~ '^sha256:[0-9a-f]{64}$'),
    -- The prior checkpoint's `checkpoint_hash` (NULL for genesis). UNIQUE
    -- + the FK make the checkpoints a strict linked list: at most one
    -- child per parent, every parent real.
    prev_checkpoint_hash  text         UNIQUE REFERENCES audit_checkpoints (checkpoint_hash),
    -- SHA-256 over (tree_size, root_hash, prev_checkpoint_hash),
    -- rendered `sha256:<hex>` - this checkpoint's identity in the chain.
    checkpoint_hash       text         NOT NULL UNIQUE CHECK (checkpoint_hash ~ '^sha256:[0-9a-f]{64}$'),
    -- The resume-watermark horizon the prefix was bounded by, and the
    -- last covered row's coordinates (NULL at tree_size 0). Diagnostic
    -- only - the root is the cryptographic commitment - but they make an
    -- operator's incident report legible.
    covered_until         timestamptz  NOT NULL,
    last_transition_id    uuid,
    last_committed_at     timestamptz,
    created_at            timestamptz  NOT NULL DEFAULT now(),
    -- Ed25519 signatures over this tree head; `[]` when unsigned. Each
    -- element is {key_id, purpose, public_key, signature}. Signing makes
    -- an externally held anchor attributable - tampering then needs the
    -- private key, not just write access (see `signing.rs`). The CHECK
    -- keeps a hand-edit from turning a tamper-evidence verdict into a
    -- decode error on the read path.
    signatures            jsonb        NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(signatures) = 'array')
);

-- At most one genesis checkpoint (the single chain root). A plain UNIQUE
-- on prev_checkpoint_hash permits many NULLs, so the single-genesis rule
-- needs its own partial index.
CREATE UNIQUE INDEX audit_checkpoints_one_genesis
    ON audit_checkpoints ((prev_checkpoint_hash IS NULL))
    WHERE prev_checkpoint_hash IS NULL;


-- Operational log of refused proposals. A rejection's transaction
-- rolls back, so its record is written AFTERWARDS in a separate
-- autocommit insert: at-most-once (a crash between rollback and
-- insert loses the record), and never inside the refusing
-- transaction. The audit table is the legitimacy-grade record;
-- this one answers the operational question "how often, and on
-- what grounds, did the rules refuse?" - the substrate for
-- coverage's `constrained` verdict.
CREATE TABLE rejections (
    rejection_id         uuid         PRIMARY KEY,        -- UUIDv7, app-generated
    transformation_name  text         NOT NULL,
    arguments            jsonb        NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    -- Same codec as audit.actor: tagged EvalValue, v0 always a subject.
    actor                jsonb        NOT NULL,
    -- Which kind of rule refused: an invariant over the candidate
    -- state, a `require` gate, or a `bind` with no candidates.
    kind                 text         NOT NULL CHECK (kind IN ('invariant', 'require', 'bind')),
    -- The invariant's name for kind = 'invariant'; the rendered gate
    -- expression for the gate kinds. Structured at the source, never
    -- parsed back out of `reason`.
    rule                 text         NOT NULL,
    invariant_version    bigint,                          -- NULL for gate kinds
    reason               text         NOT NULL,           -- the exact envelope string
    rejected_at          timestamptz  NOT NULL DEFAULT now(),
    -- The writer never emits a versioned gate or an unversioned
    -- invariant; the constraint keeps hand-edits from corrupting
    -- the operational evidence either way.
    CONSTRAINT rejections_kind_version_agree CHECK (
        (kind = 'invariant') = (invariant_version IS NOT NULL)
    )
);

-- Keyset replay order for coverage, mirroring audit_committed_at.
CREATE INDEX rejections_rejected_at ON rejections (rejected_at, rejection_id);


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


-- Kernel-computed read model (NOT governed state).
--
-- `morpholog_read` holds derived claims materialised by
-- `morpholog refresh derived`: the exact output of the kernel's
-- `enumerate_derived`, written so BI tools can read computed state in
-- plain SQL. These
-- rows are a discardable, refreshable PROJECTION, never evidence: the
-- proposal kernel, invariant evaluation, and value lookups never read
-- this schema. SQL is a projection of kernel-produced values, not a
-- second evaluator - the same principle as the base-predicate views,
-- whose interpreter-produced source is `morpholog.claims`.
--
-- The projection is GENERATION-based: a refresh builds a new generation
-- (its own `refresh_id`) without touching the live one, then flips a
-- single-row active pointer. Readers stay on the prior generation until
-- the flip commits, so a refresh never blocks or half-updates the read
-- surface, and the heavy bulk insert is decoupled from the instant
-- pointer swap.
CREATE SCHEMA IF NOT EXISTS morpholog_read;

-- `morpholog_read` is a discardable, separately-droppable cache, not
-- covered by `init`'s day-zero guard (which only checks `morpholog`), so
-- its tables are created idempotently: re-running this script over a
-- lingering cache (e.g. after dropping only `morpholog`) must not fail.
--
-- One row per refresh generation (steady state: one, the active one).
-- Freshness is OPERATIONAL metadata: which model produced it, when, and
-- the latest audit transition VISIBLE in the refresh's read snapshot.
-- (Deliberately not modelled as governed claims - this is a read cache.)
--
-- `source_snapshot_*` is a coarse freshness marker, NOT a lossless
-- audit-resume high-water: `audit.committed_at` is the writer's
-- transaction-start time while row visibility follows commit order, so a
-- transaction still in flight when the snapshot is taken - whose
-- committed_at may sort EARLIER than this marker - is simply excluded and
-- picked up by the next refresh. A consumer wanting lossless resume reads
-- the audit log via `inspect audit`, not this column.
CREATE TABLE IF NOT EXISTS morpholog_read.derived_refreshes (
    refresh_id      uuid        PRIMARY KEY,
    model_hash      text        NOT NULL,
    refreshed_at    timestamptz NOT NULL,
    source_snapshot_transition_id uuid,
    source_snapshot_committed_at  timestamptz,
    derived_claim_count bigint   NOT NULL
);

-- The single active generation. A refresh upserts this one row to flip
-- which generation readers and derived views see.
CREATE TABLE IF NOT EXISTS morpholog_read.derived_active (
    singleton  boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    refresh_id uuid    NOT NULL REFERENCES morpholog_read.derived_refreshes (refresh_id)
);

-- The materialised rows, partitioned by generation. `arguments` is the
-- kernel's positional value array, tagged-JSONB exactly as
-- `morpholog.claims.arguments` - so the exact computed value is preserved
-- and the (future) derived-view layer reuses the base-predicate
-- extractors unchanged. The PK leads with `refresh_id` so a derived view
-- filters by the active generation then predicate.
CREATE TABLE IF NOT EXISTS morpholog_read.derived_claims (
    refresh_id     uuid  NOT NULL
                         REFERENCES morpholog_read.derived_refreshes (refresh_id)
                         ON DELETE CASCADE,
    predicate_name text  NOT NULL,
    arguments      jsonb NOT NULL CHECK (jsonb_typeof(arguments) = 'array'),
    PRIMARY KEY (refresh_id, predicate_name, arguments)
);
