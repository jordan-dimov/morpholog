# Outbox worker: design sketch

Status: design sketch, not implementation. Pairs with the spike test at [`crates/morpholog-postgres/tests/outbox_spike.rs`](../crates/morpholog-postgres/tests/outbox_spike.rs), which demonstrates the target shape via hand-rolled audit-replay + compensation in user code. The implementation PR(s) that follow should answer the open questions at the bottom of this doc; deletion of this doc happens once `docs/forced-by-examples.md` carries the retrospective.

## Problem

Every committed transformation enqueues outbox rows. Nobody consumes them. The audit log says "we sent X" while the external world has heard nothing. This is a known v0 gap, but it has become structural: a regulated user who wires Morpholog into a real workflow today gets a runtime that commits to deliveries it never makes.

The deeper concern is what happens when a delivery *should have* succeeded but didn't. A lending drawdown commits locally; the runtime emits a `DispatchWire` intent; forty-five seconds later, the SEPA network rejects the wire due to an out-of-band AML routing lock. The Morpholog ledger now states a definitive lie: that money was legitimately drawn down. Reality contradicts the books, and the runtime has no path back to consistency.

The architectural answer - phrased crisply by a recent strategic review - is **Morpholog plus an Outside Coordinator**:

- Morpholog stays a strict local transactional gatekeeper. The IR does not learn about networks, retries, or distributed consensus. If it did, the language would stop being decidable and start being a worse-than-Camunda workflow engine.
- An outside coordinator (the outbox worker) owns the asynchronous conversation with the real world. It tries to deliver, retries on transient failure, gives up on systemic failure.
- When delivery fails terminally, the coordinator reconciles by invoking a Morpholog *compensating transformation* - a normal Morpholog transformation that goes through every invariant check and writes its own audit row.

The mental model worth being precise about: **the original transition was a locally legitimate admission**. The runtime knew what it knew at the time, an invariant gate cleared, a claim was admitted. Then the external world contradicted the intended side effect. Morpholog does not pretend the original transition never happened - claims are admitted assertions, not objective facts; that distinction is load-bearing throughout the runtime. Instead, the runtime records the later contradiction (status flips to `failed`; the reason is preserved) and the compensation (a new transition through the same invariant-governed path) as further admitted facts. An auditor reading the audit log sees the full sequence: legitimate admission, evidence of external failure, governed correction.

This sketch pins how the coordinator works.

## The smallest forcing example: terminal delivery failure with reconciliation

Reuses the audit log directly; uses local fixtures rather than polluting any worked example with delivery semantics.

1. A transformation `spike_post_entry` commits. It asserts a journal entry and emits a `WireDispatch` intent.
2. A hand-rolled consumer reads the pending outbox row.
3. A mock deliverer returns `NonRetryable("AML lock")`.
4. The consumer marks the outbox row `failed`.
5. The consumer invokes a compensating transformation `spike_reverse_entry`, which asserts a balancing reverse entry. This goes through `propose_against_pg` and writes its own audit row.
6. After all that: the audit log has two transitions (`tid_commit`, `tid_compensate`); current state shows both the original entry and the reversal; the outbox row carries the failure with enough breadcrumbs to trace why.

That is the full pattern. The spike test in this PR exercises it end-to-end. The happy path - deliverer returns `Delivered`, row marked, no compensation - is exercised as a second test for symmetry.

## Sequencing the implementation

This is genuinely too much for one PR. The spike makes a tempting "build the whole worker stack" picture look reachable; that picture is the trap. The discipline that has carried other parts of the runtime (smallest forced step; validate; then the next forced step) applies here too. Three PRs at least, probably four:

1. **(landed in PR #32) Durable outbox state vocabulary.** Schema additions (`failed_at`, `failure_reason`, `compensation_transition_id`, `next_attempt_at`, `locked_by`, `lock_expires_at`) plus `mark_outbox_delivered`, `mark_outbox_transient_attempt`, `mark_outbox_failed`, `record_compensation`, `claim_pending_outbox_row`, `release_outbox_claim`. The substrate captured "what does the database track about delivery" without yet pinning "what code does the tracking."
2. **(landed in this PR) Single-row processor + `DeliveryOutcome` + `Deliverer` trait + compensation lease.** `process_one_outbox_row` drives one full cycle. Added the `compensation_in_progress` status and three lease helpers (`begin_compensation`, `complete_compensation`, `mark_compensation_failed`) to close the duplicate-compensation race the PR 1 substrate did not solve on its own. Native `async fn` in trait for `Deliverer`. `CompensationSpec` uses a boxed args closure so callers can pass `None` without an inference workaround.
3. **(next) Polling loop + per-target worker.** One tokio task per intent type. Reuses `process_one_outbox_row` as the inner action; the loop owns poll-with-jitter scheduling and Transient backoff strategy. Still no HTTP, no supervisor, no circuit breaker.
4. **(then) Supervisor + circuit breaker + first real `Deliverer` impl.** `JoinSet`-based restart-with-intensity. `failsafe-rs` per-target breaker. `StdoutDeliverer` as the canonical first impl; `HttpDeliverer` later as its own PR.

Anything below the "Likely API shape" line is the *eventual* shape; it does not all land at once. The boxes-and-arrows below are the destination, not the next merge.

## Likely API shape (eventual)

Pins what the worker will *eventually* look like, across the four PRs above. None of this lives in `morpholog-core`.

### A new crate `morpholog-outbox`

Separate from `morpholog-postgres` because it has a different dependency profile (tokio runtime, HTTP client for the eventual webhook deliverer, `failsafe-rs` for circuit breakers) and because the worker is replaceable - a different deployment might want a Lambda-based worker, a Kafka-relayed worker, or no worker at all.

The crate depends on `morpholog-core` (for `EvalValue`, `Intent`, `Transformation`, `propose`) and on `morpholog-postgres` (for `PgPool`, `PgError`, `list_pending_outbox`, `propose_against_pg`).

### The `Deliverer` trait

```rust
pub trait Deliverer: Send + Sync {
    fn deliver(
        &self,
        row: &OutboxRow,
    ) -> impl std::future::Future<Output = DeliveryOutcome> + Send;
}

pub enum DeliveryOutcome {
    Delivered,
    Transient { next_attempt_at: DateTime<Utc> },
    NonRetryable { reason: String },
}
```

The trait uses RPITIT (return position impl trait in trait) with explicit `Send + Sync` on the implementor and `+ Send` on the future so polling loops can `tokio::spawn(deliverer.deliver(...))` against an arbitrary `D: Deliverer`. `OutboxRow` rather than `IntentInstance` is passed so implementors can read `attempt_count`, `enqueued_at`, `last_attempt_at`, etc. for retry/jitter decisions. `Transient` carries an explicit retry instant (not a duration) so the deliverer chooses the absolute schedule.

Three-way outcome borrowed from MassTransit / NServiceBus's vocabulary (their "systemic" / "poison" failure class is our `NonRetryable`). `Transient` carries an explicit retry hint so the worker doesn't have to guess. `NonRetryable` carries a reason that ends up in the failure audit trail.

Ship two impls in the implementation PR:

- `StdoutDeliverer`: prints intent as JSON to stdout. For demos and tests.
- `HttpDeliverer`: POSTs the intent to a configured URL. Returns `Transient` on 5xx + connection errors; `NonRetryable` on 4xx (except 408, 429); `Delivered` on 2xx.

A user-defined `Deliverer` impl can do anything else (write to Kafka, call a Lambda, push to SQS, ...). The trait is the boundary.

### Compensation pairing: configuration, not IR

The `Intent` IR stays exactly as it is - just a name and args. The "what should happen if delivery of intent X fails terminally?" pairing lives in the worker's configuration:

```rust
pub struct CompensationSpec {
    pub compensating_transformation: Transformation,
    pub args_from_intent: Box<dyn Fn(&IntentInstance, &str /* reason */) -> Vec<EvalValue> + Send + Sync>,
    pub invariants: Vec<Invariant>,
}

pub struct WorkerConfig {
    pub deliverers: HashMap<String /* intent_type */, Box<dyn Deliverer>>,
    pub compensations: HashMap<String /* intent_type */, CompensationSpec>,
    // ... circuit breaker / restart-policy config ...
}
```

This shape:

- Keeps the IR ontologically simple. Intent stays "name + args."
- Lets the same Morpholog program run under different compensation policies. A test deployment might log failures; a production deployment might revoke positions.
- Makes compensation paths explicit and reviewable - a deployment engineer sees the whole `(intent, deliverer, compensation)` triple in one place.
- The `args_from_intent` mapper lets the compensation transformation use whatever fields it needs from the original intent plus the failure reason.

### Per-target supervisor pattern, lightly

One supervised tokio task per *delivery target* (i.e., one task per registered intent_type). Each task:

- Claims one pending row for its intent_type using a lease pattern (see "Locking" below).
- Calls its `Deliverer` **outside any database transaction**. Routes the outcome.
- On `Delivered`: update outbox row `status='delivered'`, `delivered_at=now()`, using the lease token.
- On `Transient`: increment `attempt_count`, set `last_attempt_at` and `next_attempt_at = now() + retry_after`, release the lease.
- On `NonRetryable`: update `status='failed'`, `failed_at=now()`, `failure_reason=...`; invoke the compensating transformation via `propose_against_pg`; record the compensation's `transition_id` in the `compensation_transition_id` column for lineage.

The supervisor (root tokio task) owns the per-target tasks via `tokio::task::JoinSet`. On panic, restart the failed task with bounded restart-intensity (borrowing `ractor-supervisor`'s algorithm but re-implementing rather than depending on a full actor framework). Per-target circuit breakers via `failsafe-rs` short-circuit deliveries to a target that has been failing repeatedly, so one misbehaving target doesn't burn the others' budget.

### Locking: lease, not held transaction

A naive read of `sqlxmq` suggests `SELECT ... FOR UPDATE SKIP LOCKED` is the whole answer. It is not, for this workload: holding a PG transaction open across a network call (the deliverer) leaves a row locked for the duration of the network round-trip, creates long-held locks, makes failure behaviour worse than the alternative, and burns connection pool slots. Acceptable for a toy worker; bad production habit.

The right shape is a two-step lease:

1. **Claim:** atomically pick one pending row, mark it `status='in_progress'` with `locked_by=<worker_id>` and `lock_expires_at=now()+<timeout>`, returning the row. The atomic claim itself uses `FOR UPDATE SKIP LOCKED` inside an UPDATE-RETURNING with a subquery; the surrounding transaction is short. Commit the claim.
2. **Deliver:** invoke the deliverer with **no transaction held**.
3. **Resolve:** UPDATE the row using the lease token (`WHERE locked_by=<worker_id> AND lock_expires_at > now()`), transitioning to `delivered` / `pending+retry` / `failed`. If the lease expired, the worker discards the result silently; another worker has already taken over.

This adds two columns (`locked_by`, `lock_expires_at`) to the schema additions list - sequencing PR 1 should include them.

`SKIP LOCKED` is great for the *claim* step (so two workers do not contend for the same row); it is the wrong tool for "hold the row across delivery."

### Idempotency for compensation

Worth being precise: **`propose_against_pg` does not enforce transformation-level idempotency from arguments**. Every accepted transformation writes a new audit row. The existing `idempotency_key` column is for *outbox intents* (so a retried delivery does not produce two outbox rows), not for transformations.

So compensation needs explicit guarding against duplicate application. Two honest mechanisms:

1. **Check the outbox row's `compensation_transition_id` before invoking.** If it is already set, skip - compensation has run. Requires the `compensation_transition_id` column from sequencing step 1, plus an atomic "claim-the-compensation" step (probably via the same lease pattern used for delivery) so a crash between "compensation committed" and "compensation_transition_id written" does not produce a duplicate.
2. **Design the compensating transformation to be invariant-guarded against duplicate application.** For example, the compensation asserts a `CompensationApplied(original_intent_id, key)` claim, and an invariant rejects any second compensation against the same `original_intent_id`. The kernel then refuses the duplicate at admission time.

Mechanism 1 is the simpler v0 path: the worker is responsible for not double-invoking. Mechanism 2 is more robust but requires per-program design discipline (every compensable program has to think about its own compensation-uniqueness predicate).

The implementation PR sequencing in this doc commits to mechanism 1 for the first cut, with mechanism 2 available as a deeper guard the user can layer on top in their own programs.

**Status: PR 2 closed this gap under the "retain the lease through compensation" shape.** A new `compensation_in_progress` status was added, plus three lease helpers (`begin_compensation`, `complete_compensation`, `mark_compensation_failed`). The state machine is now: `mark_outbox_failed` releases the delivery lease and transitions to `failed`; `begin_compensation` re-claims a fresh lease via `SELECT ... FOR UPDATE SKIP LOCKED` on `status='failed' AND compensation_transition_id IS NULL`, transitioning to `compensation_in_progress`; the compensating transformation runs via `propose_against_pg`; on success `complete_compensation` transitions back to `failed` with the pointer set, on rejection `mark_compensation_failed` transitions to `compensation_failed`. At most one worker holds the compensation right for a given row at any moment.

`begin_compensation` deliberately does NOT transparently reclaim expired `compensation_in_progress` leases (unlike `claim_pending_outbox_row`, which reclaims expired delivery leases). Transparent reclaim of a compensation lease would risk a duplicate compensating transformation if the previous worker crashed *after* `propose_against_pg` committed but *before* `complete_compensation` ran. The safer default is: a stuck `compensation_in_progress` row requires operator intervention rather than automatic recovery.

The narrow window that remains: a crash between `propose_against_pg` commit and `complete_compensation` call. The lease pattern does not eliminate that window; it just shrinks it from "always racy" to "racy only during crash recovery." Programs that need full immunity should layer mechanism 2 on top - the compensating transformation asserts a `CompensationApplied(original_intent_id)` claim and an invariant rejects duplicates at admission time. The runtime supports this today; no further substrate is needed.

### Schema additions

New nullable columns on `morpholog.outbox`:

- `failed_at: timestamptz` - set when `status='failed'`.
- `failure_reason: text` - the `NonRetryable.reason` from the deliverer.
- `compensation_transition_id: uuid` - the audit row of the compensating transformation, for lineage.
- `next_attempt_at: timestamptz` - set on `Transient` outcomes; the claim query filters on `next_attempt_at <= now()` so a row with pending retry is not picked up early.
- `locked_by: text` - the worker id holding the current lease (or NULL if unclaimed).
- `lock_expires_at: timestamptz` - when the lease becomes reclaimable by another worker.

Status string also gains an `in_progress` value (currently the CHECK allows `pending|delivered|failed`; `in_progress` is the leased state).

No new tables. The original intent stays findable from the compensation (via `compensation_transition_id`), and vice versa (`SELECT FROM outbox WHERE compensation_transition_id = ...`).

### CLI surface

`morpholog inspect outbox` already exists for pending rows. The implementation PR may add `morpholog inspect failed-deliveries` and `morpholog inspect compensations`, or extend `inspect outbox` with a `--status <pending|delivered|failed>` filter. Out of scope here.

## What it is NOT (in v0)

Worth pinning so the implementation PR does not drift:

- **Not a distributed coordinator.** Morpholog plus an outbox worker stays single-substrate. We do not invent a saga orchestrator, do not coordinate across multiple databases, do not implement two-phase commit. The "outside coordinator" is on the same machine as the runtime (or a sibling process); it does not talk to other Morpholog instances.
- **Not a message broker.** No Kafka, no RabbitMQ. The outbox table IS the queue. Workers poll it.
- **Not LISTEN/NOTIFY.** Polling first; LISTEN/NOTIFY is faster but couples tighter to PG, and the polling cost is bounded with `FOR UPDATE SKIP LOCKED` and a reasonable interval. Switch is a future concern.
- **Not native external state in the IR.** The IR does not gain `Network(...)`, `WaitForReply(...)`, `Timeout(...)`, or anything that would let a transformation pause for external acknowledgement. That is exactly the IR pollution Gemini's review warned against - it would turn the language from decidable to undecidable workflow orchestration.
- **Not a generic workflow engine.** Morpholog's job is admission-time invariant enforcement plus audit-replayable history. The coordinator's job is best-effort delivery plus compensation hooks. Anything more elaborate (long-running approvals, human-in-the-loop steps, parallel branches with joins) lives outside both, in whatever orchestration tool the deployment already uses.
- **Not "exactly-once" delivery.** The combination of at-least-once delivery + deterministic idempotency keys + idempotent receivers is the industry standard; Morpholog inherits it. "Exactly once" is a lie people tell themselves; Morpholog does not pretend otherwise.
- **Not a poison-message recovery UI.** A failed message stays in the outbox with `status='failed'` and full reason. Operator tooling (re-queue, manually invoke compensation, archive) is post-MVP.

## Open design questions

The implementation PR(s) have to answer these explicitly. The spike does not commit to any of them.

1. **Crate boundary.** New `morpholog-outbox` crate (current lean) or new module in `morpholog-postgres`? The new-crate lean wins on dependency hygiene; the in-postgres alternative wins on cognitive load.
2. **Worker process model.** One binary that runs the supervisor + workers (lean: `morpholog-worker` binary in the new crate)? Or a library that an embedder hooks into their own tokio runtime? Probably both, with the binary using the library.
3. **Per-target ordering guarantee.** Per-`intent_type` FIFO is a defensible default; cross-`intent_type` ordering is not preserved. Is that the contract? Or should we offer ordering by `transition_id` (causality across intent types)?
4. **Compensation declaration shape.** The lean is a typed `HashMap<intent_type, CompensationSpec>` passed at worker construction. Alternatives: declarative TOML config; a derive-macro on `Program`; a builder pattern. Lean stays Rust-typed for the first cut.
5. **Restart intensity / circuit breaker defaults.** The implementation PR picks numbers; we should agree on the algorithm now (lean: `ractor-supervisor`'s intensity-over-period model; `failsafe-rs` for circuit breakers with sensible defaults like 5 failures in 60s opens the circuit).
6. **Schema migration story.** Adding three columns to `morpholog.outbox` is breaking for existing databases. We have no migration framework. Lean: edit `schema.sql` directly + document the manual `ALTER TABLE` for upgraders. A migration framework is its own PR.
7. **Lease vs SKIP LOCKED.** `FOR UPDATE SKIP LOCKED` inside the worker's processing transaction is the lean (sqlxmq's pattern); the alternative is an explicit lease column with timeout. `FOR UPDATE` keeps the row blocked for the duration of the transaction; if delivery is slow, that's a long-held lock. A separate lease + claim ID would be more robust under slow deliveries. Decision deferred until the deliverer's expected duration is clearer.
8. **Failure of the compensation itself.** What if `propose_against_pg(compensating_tx, ...)` fails - business rejection, kernel error, or PG error? Lean: log loudly, leave outbox row in a new `status='compensation_failed'` state, do not retry compensation indefinitely. Requires operator intervention. This is the genuinely-broken state; the worker should not paper over it.

## What this PR delivers

- This document.
- A spike test (`crates/morpholog-postgres/tests/outbox_spike.rs`) demonstrating the full compensation flow + happy path via hand-rolled in-test code. The spike uses existing ledger transformations (so the worked examples stay focused on their own business stories). It uses a mock deliverer and a hand-rolled consumer loop. The semantics of compensation - that it goes through `propose_against_pg` and writes its own audit row - are pinned by the spike's assertions on the audit log.
- A README pointer.

The spike is meant to be ugly. Every caller wanting outbox delivery + compensation has to write something like it today. That ugliness is the case for the implementation PR(s).

### A note on the spike's compensation choice

The spike uses `post_simple_entry` with debit/credit accounts swapped as the compensation. That is structurally correct (balanced under the same invariant; goes through every gate; writes its own audit row), but it has a real-system implication worth surfacing: the compensation **emits its own `JournalEntryPosted` intent**, which the next consumer pass would try to deliver. In a real ledger this might be the wrong shape - the reversal should probably emit `JournalEntryReversed` (or no external intent at all) rather than re-broadcast as a fresh posting. The compensating transformation is ordinary; what the chosen compensating transformation *emits* is a design call the program author has to make deliberately. The spike does not adjudicate that; it just notes the duplication.

## What this PR does NOT deliver

- Any production code in a new crate. No `morpholog-outbox::Worker`, `Deliverer`, `CompensationSpec`.
- Any schema changes.
- The `StdoutDeliverer` or `HttpDeliverer` impls.
- The supervisor's `JoinSet` + restart-intensity algorithm.
- The per-target circuit breaker integration with `failsafe-rs`.
- Any CLI surface.
- An updated `forced-by-examples.md` entry. That belongs in the implementation PR's commit, after the open questions above are settled by the act of implementing.
