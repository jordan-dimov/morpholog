# Outbox worker: doctrine

Status: doctrine doc. The substrate, single-row processor with compensation lease, polling worker with smart sleep, and `StdoutDeliverer` are all in tree as of this writing - see the [`morpholog-outbox`](../crates/morpholog-outbox/) and [`morpholog-postgres`](../crates/morpholog-postgres/) crates for the actual code. The remaining slice is the supervisor (JoinSet-based restart-with-intensity), per-target circuit breaker, and `HttpDeliverer`. This doc pins the load-bearing doctrine - the **Morpholog plus an Outside Coordinator** framing, what the runtime is and is not - while implementation detail lives in the code.

## Problem

Every committed transformation enqueues outbox rows. Nobody consumes them. The audit log says "we sent X" while the external world has heard nothing. This is a known v0 gap, but it has become structural: a regulated user who wires Morpholog into a real workflow today gets a runtime that commits to deliveries it never makes.

The deeper concern is what happens when a delivery *should have* succeeded but didn't. A lending drawdown commits locally; the runtime emits a `DispatchWire` intent; forty-five seconds later, the SEPA network rejects the wire due to an out-of-band AML routing lock. The Morpholog ledger now states a definitive lie: that money was legitimately drawn down. Reality contradicts the books, and the runtime has no path back to consistency.

The architectural answer - phrased crisply by a recent strategic review - is **Morpholog plus an Outside Coordinator**:

- Morpholog stays a strict local transactional gatekeeper. The IR does not learn about networks, retries, or distributed consensus. If it did, the language would stop being decidable and start being a worse-than-Camunda workflow engine.
- An outside coordinator (the outbox worker) owns the asynchronous conversation with the real world. It tries to deliver, retries on transient failure, gives up on systemic failure.
- When delivery fails terminally, the coordinator reconciles by invoking a Morpholog *compensating transformation* - a normal Morpholog transformation that goes through every invariant check and writes its own audit row.

The mental model worth being precise about: **the original transition was a locally legitimate admission**. The runtime knew what it knew at the time, an invariant gate cleared, a claim was admitted. Then the external world contradicted the intended side effect. Morpholog does not pretend the original transition never happened - claims are admitted assertions, not objective facts; that distinction is load-bearing throughout the runtime. Instead, the runtime records the later contradiction (status flips to `failed`; the reason is preserved) and the compensation (a new transition through the same invariant-governed path) as further admitted facts. An auditor reading the audit log sees the full sequence: legitimate admission, evidence of external failure, governed correction.

The rest of this doc pins the coordinator's doctrine - what it is, what it is not, and the constraints that produced the realised shape.

## The pattern as it now ships

The worker exists; the doctrine is realised in code. The shape:

- A new crate, [`morpholog-outbox`](../crates/morpholog-outbox/), separate from `morpholog-postgres` because it has a different dependency profile (tokio, HTTP client, eventual circuit breaker) and because the worker is replaceable - a deployment can write its own.
- A `Deliverer` trait with a three-way outcome: `Delivered`, `Transient { next_attempt_at }`, `NonRetryable { reason }`. The vocabulary is from MassTransit / NServiceBus. `Transient` carries an absolute retry instant so the deliverer chooses the schedule; `NonRetryable` carries a reason that lands in the failure audit trail.
- `StdoutDeliverer` ships as the canonical first deliverer. `HttpDeliverer` (POST to a configured URL; route 5xx/connection-errors to `Transient`, 4xx-except-408/429 to `NonRetryable`, 2xx to `Delivered`) is the remaining slice.
- Compensation pairing lives in worker configuration, not in the IR. A `CompensationSpec` per intent type names the compensating transformation, an args mapper, and the invariants to evaluate against. This keeps `Intent` ontologically simple (just name + args) and lets one program run under different compensation policies in different deployments.
- A two-step lease (`claim` then `deliver-no-transaction-held` then `resolve-via-lease-token`) is the locking pattern. The naive `FOR UPDATE SKIP LOCKED` held across the deliverer call would hold a row locked across the network round-trip; the lease shrinks that to a short atomic claim plus an absolute timeout. Compensation has its own lease that deliberately does **not** auto-reclaim on expiry - a stuck `compensation_in_progress` row requires operator intervention rather than risk a duplicate compensating transformation.

For full detail of any of these, the code is the source of truth. The doctrine lives below.

## What it is NOT

The doctrine that keeps the runtime narrow:

- **Not a distributed coordinator.** Morpholog plus an outbox worker stays single-substrate. We do not invent a saga orchestrator, do not coordinate across multiple databases, do not implement two-phase commit. The "outside coordinator" is on the same machine as the runtime (or a sibling process); it does not talk to other Morpholog instances.
- **Not a message broker.** No Kafka, no RabbitMQ. The outbox table IS the queue. Workers poll it.
- **Not LISTEN/NOTIFY.** Polling first; LISTEN/NOTIFY is faster but couples tighter to PG, and the polling cost is bounded with `FOR UPDATE SKIP LOCKED` and a reasonable interval. Switch is a future concern.
- **Not native external state in the IR.** The IR does not gain `Network(...)`, `WaitForReply(...)`, `Timeout(...)`, or anything that would let a transformation pause for external acknowledgement. That kind of IR pollution would turn the language from decidable to undecidable workflow orchestration.
- **Not a generic workflow engine.** Morpholog's job is admission-time invariant enforcement plus audit-replayable history. The coordinator's job is best-effort delivery plus compensation hooks. Anything more elaborate (long-running approvals, human-in-the-loop steps, parallel branches with joins) lives outside both, in whatever orchestration tool the deployment already uses.
- **Not "exactly-once" delivery.** The combination of at-least-once delivery + deterministic idempotency keys + idempotent receivers is the industry standard; Morpholog inherits it. "Exactly once" is a lie people tell themselves; Morpholog does not pretend otherwise.
- **Not a poison-message recovery UI.** A failed message stays in the outbox with `status='failed'` and full reason. Operator tooling (re-queue, manually invoke compensation, archive) is post-MVP.

## Doctrinal tension worth naming

What shipped puts compensation idempotency in *runtime state-machine columns* (`compensation_in_progress`, `compensation_failed`, `compensation_transition_id`) on `morpholog.outbox`. That is operationally fine, but it sits awkwardly against the project thesis - "only invariants and transformations are first-class" - which would prefer compensation idempotency expressed as a `CompensationApplied(original_intent_id)` claim, governed by an invariant the kernel checks at admission time. The shipped shape is convenient because no worked example yet forces compensation; the substrate was built against a doctrine sketch. When a worked example actually drives compensation end to end (a wire-dispatch programme with a real reversal transformation, say), it will be the right moment to revisit whether the lease-machinery scaffolding earns its keep or whether the invariant-driven shape replaces it. Until then, the columns stay; the tension is noted.
