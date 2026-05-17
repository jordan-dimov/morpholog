# Outbox worker: design sketch

Status: design sketch, not implementation. Pairs with the spike test at [`crates/morpholog-postgres/tests/outbox_spike.rs`](../crates/morpholog-postgres/tests/outbox_spike.rs), which demonstrates the target shape via hand-rolled audit-replay + compensation in user code. The implementation PR(s) that follow should answer the open questions at the bottom of this doc; deletion of this doc happens once `docs/forced-by-examples.md` carries the retrospective.

## Problem

Every committed transformation enqueues outbox rows. Nobody consumes them. The audit log says "we sent X" while the external world has heard nothing. This is a known v0 gap, but it has become structural: a regulated user who wires Morpholog into a real workflow today gets a runtime that commits to deliveries it never makes.

The deeper concern is what happens when a delivery *should have* succeeded but didn't. A lending drawdown commits locally; the runtime emits a `DispatchWire` intent; forty-five seconds later, the SEPA network rejects the wire due to an out-of-band AML routing lock. The Morpholog ledger now states a definitive lie: that money was legitimately drawn down. Reality contradicts the books, and the runtime has no path back to consistency.

The architectural answer - phrased crisply by a recent strategic review - is **Morpholog plus an Outside Coordinator**:

- Morpholog stays a strict local transactional gatekeeper. The IR does not learn about networks, retries, or distributed consensus. If it did, the language would stop being decidable and start being a worse-than-Camunda workflow engine.
- An outside coordinator (the outbox worker) owns the asynchronous conversation with the real world. It tries to deliver, retries on transient failure, gives up on systemic failure.
- When delivery fails terminally, the coordinator reconciles by invoking a Morpholog *compensating transformation* - a normal Morpholog transformation that goes through every invariant check and writes its own audit row. The ledger never lies; it just learns later that reality went a different way, and corrects itself through the same invariant-governed path as everything else.

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

## Likely API shape

Pins what the implementation PR will build. None of this lives in `morpholog-core`.

### A new crate `morpholog-outbox`

Separate from `morpholog-postgres` because it has a different dependency profile (tokio runtime, HTTP client for the eventual webhook deliverer, `failsafe-rs` for circuit breakers) and because the worker is replaceable - a different deployment might want a Lambda-based worker, a Kafka-relayed worker, or no worker at all.

The crate depends on `morpholog-core` (for `EvalValue`, `Intent`, `Transformation`, `propose`) and on `morpholog-postgres` (for `PgPool`, `PgError`, `list_pending_outbox`, `propose_against_pg`).

### The `Deliverer` trait

```rust
#[async_trait]
pub trait Deliverer: Send + Sync {
    async fn deliver(&self, intent: &IntentInstance) -> DeliveryOutcome;
}

pub enum DeliveryOutcome {
    Delivered,
    Transient { retry_after: Duration },
    NonRetryable { reason: String },
}
```

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

- Polls `morpholog.outbox` for pending rows of its intent_type, ordered by `enqueued_at`, with `SELECT ... FOR UPDATE SKIP LOCKED` (borrowed from `sqlxmq`'s pattern) so multiple workers can run safely.
- Calls its `Deliverer`. Routes the outcome.
- On `Delivered`: update outbox row `status='delivered'`, `delivered_at=now()`.
- On `Transient`: leave `status='pending'`, increment `attempt_count`, set `last_attempt_at`, defer next attempt by `retry_after` with jitter.
- On `NonRetryable`: update `status='failed'`, `failed_at=now()`; invoke the compensating transformation via `propose_against_pg`; record the compensation's `transition_id` in a new `compensation_transition_id` column for lineage.

The supervisor (root tokio task) owns the per-target tasks via `tokio::task::JoinSet`. On panic, restart the failed task with bounded restart-intensity (borrowing `ractor-supervisor`'s algorithm but re-implementing rather than depending on a full actor framework). Per-target circuit breakers via `failsafe-rs` short-circuit deliveries to a target that has been failing repeatedly, so one misbehaving target doesn't burn the others' budget.

### Idempotency for compensation

The existing `idempotency_key` column on outbox rows covers the forward intent. For compensation, the worker derives a key by hashing `(original_idempotency_key, "compensation")`, included in the args of the compensating `propose_against_pg` call (so the kernel's idempotency contract still applies). Retries of the compensation step are therefore safe.

### Schema additions

Three new nullable columns on `morpholog.outbox`:

- `failed_at: timestamptz` - set when `status='failed'`.
- `failure_reason: text` - the `NonRetryable.reason` from the deliverer.
- `compensation_transition_id: uuid` - the audit row of the compensating transformation, for lineage.

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
- A spike test (`crates/morpholog-postgres/tests/outbox_spike.rs`) demonstrating the full compensation flow + happy path via hand-rolled in-test code. The spike defines local compensable + compensation transformation fixtures (so the worked examples stay focused on their own business stories). It uses a mock deliverer and a hand-rolled consumer loop. The semantics of compensation - that it goes through `propose_against_pg` and writes its own audit row - are pinned by the spike's assertions on the audit log.
- A README pointer.

The spike is meant to be ugly. Every caller wanting outbox delivery + compensation has to write something like it today. That ugliness is the case for the implementation PR.

## What this PR does NOT deliver

- Any production code in a new crate. No `morpholog-outbox::Worker`, `Deliverer`, `CompensationSpec`.
- Any schema changes.
- The `StdoutDeliverer` or `HttpDeliverer` impls.
- The supervisor's `JoinSet` + restart-intensity algorithm.
- The per-target circuit breaker integration with `failsafe-rs`.
- Any CLI surface.
- An updated `forced-by-examples.md` entry. That belongs in the implementation PR's commit, after the open questions above are settled by the act of implementing.
