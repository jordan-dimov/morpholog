# PostgreSQL Persistence (v0) — Design Pin

Status: design pin for the next implementation PR. **No code yet.**

PR #4 added a deterministic JSON codec for the runtime types. This document fixes the smallest acceptable design for `propose_against_pg()` so the implementation PR stays narrow and the kernel cannot get conceptually corrupted on the way to a database.

## Public goal

```rust
fn propose_against_pg(
    conn: ???,                          // see open question 1
    transformation: &Transformation,
    args: Vec<EvalValue>,
    invariants: &[Invariant],
) -> Result<Outcome, PgError>;
```

The function must:

1. Open **one** PostgreSQL transaction at `SERIALIZABLE` isolation.
2. Load the current `morpholog.claims` rows into an in-memory `State`.
3. Call the existing `propose(transformation, args, &state, invariants)` — unchanged kernel.
4. If `Rejected`: roll back. No governed changes. No audit row. Return `Rejected { reason }`.
5. If `Accepted`:
   - Delete `morpholog.claims` rows matching staged retractions.
   - Insert staged assertions into `morpholog.claims`.
   - Insert one `morpholog.audit` row.
   - Insert one `morpholog.outbox` row per emitted intent.
   - Commit atomically.

The kernel (`propose()`, evaluator, IR types) is not touched. PG persistence is a thin adapter.

## Explicit non-goals

- No parser, lexer, or surface syntax work.
- No migrations framework. `crates/morpholog-core/sql/schema.sql` is the canonical schema; apply it manually for now.
- No invariant lifecycle / versioned epochs. All invariants commit under `invariant_epoch = 1` with `version: 1` per invariant.
- No read models, projections, or query layer.
- No generic repository, service, or storage abstraction. The function is concrete and direct.
- No model checker.
- No background worker for outbox delivery; just enqueue rows. Worker is a later PR.
- No exactly-once delivery mechanics. At-least-once with idempotency keys, per the locked decisions.
- No schema changes. If the implementation discovers a real mismatch, **stop and revisit this doc first** rather than drifting silently.
- No business audit row for failed proposals. Operational rejection logging is a separate concern, deferred.

## Transaction semantics

- The PostgreSQL transaction begins **before** loading state.
- All `propose()` reads see the snapshot loaded at step 2. No read-your-writes inside the transformation body — that constraint is preserved by construction, because `propose()` does not see PG at all.
- The candidate state is still built by the existing in-memory kernel.
- DB writes happen only after `propose()` returns `Accepted`.
- A `Rejected` return commits no changes. The PG transaction is rolled back. No audit row is written.

On commit, claims mutations, the audit row, and outbox rows all land in one PG commit. Outbox workers run outside this transaction.

## Isolation: `SERIALIZABLE`, with caller-side retry

Two concurrent `propose_against_pg` transactions can each see `S_0`, stage non-overlapping changes, individually satisfy invariants against their own candidate states, and commit — producing a combined final state that violates invariants neither transaction's check saw. This is the classic write-skew anomaly.

- `READ COMMITTED` is unsafe: non-repeatable reads plus write skew.
- `REPEATABLE READ` (PostgreSQL's snapshot isolation) prevents non-repeatable reads but still allows write skew.
- `SERIALIZABLE` (PostgreSQL SSI) detects dangerous SI patterns at commit and aborts one transaction with SQLSTATE `40001`. The caller retries.

v0 pins `SERIALIZABLE`. The implementation PR must include a test that demonstrates the concurrent-violation case and shows the retry behaviour. Row-level `SELECT ... FOR UPDATE` may become an optimization later; it is **not** in v0.

## JSON column mapping (uses PR #4 codec verbatim)

| Column | Rust source | Notes |
| --- | --- | --- |
| `claims.predicate_name` | `ClaimInstance.predicate` (text) | Text column, not JSON. |
| `claims.arguments` | `ClaimInstance.args: Vec<EvalValue>` | JSONB array. CHECK `jsonb_typeof = 'array'`. |
| `claims.asserted_in` | UUIDv7 from `audit.transition_id` | Foreign-key-shaped, but no FK declared in v0 schema. |
| `audit.transition_id` | `Uuid::now_v7()` minted at function entry | UUIDv7 primary key. |
| `audit.transformation_name` | `Transformation.name` | Text. |
| `audit.arguments` | the function's `args: Vec<EvalValue>` | JSONB array. |
| `audit.invariant_epoch` | `1` (v0 constant) | Reserved for future lifecycle work. |
| `audit.invariants_checked` | `Vec<(String, u32)>` of `(name, version)` pairs | JSONB array of `[name, version]` tuples. **This is the one JSON shape not yet pinned by codec tests** — the implementation PR must add a contract test. |
| `audit.asserted_claims` | `Vec<ClaimInstance>` | JSONB array of `{predicate, args}` objects. |
| `audit.retracted_claims` | `Vec<ClaimInstance>` | JSONB array of `{predicate, args}` objects. |
| `audit.emitted_intents` | `Vec<IntentInstance>` | JSONB array of `{name, args}` objects. |
| `outbox.intent_id` | `Uuid::now_v7()` per intent | UUIDv7 primary key. |
| `outbox.transition_id` | from `audit.transition_id` | FK to audit. |
| `outbox.intent_type` | `IntentInstance.name` (text) | Text column. |
| `outbox.arguments` | `IntentInstance.args: Vec<EvalValue>` | JSONB array. CHECK `jsonb_typeof = 'array'`. |
| `outbox.idempotency_key` | see below | Unique. |

The split between full-object encodings (audit JSONB arrays) and split column writes (`claims`, `outbox`) is enforced by the contract tests in PR #4 (`claim_args_serialise_as_a_json_array`, `intent_args_serialise_as_a_json_array`).

## Idempotency key

For v0, deterministic per intent:

```
idempotency_key = hex(sha256(transition_id || "\x00" || intent.name || "\x00" || canonical_json(intent.args)))
```

Since `transition_id` is fresh per accepted transformation (UUIDv7), this prevents duplicate `outbox` rows under retry/redelivery mechanics. It does **not** prevent duplicate business events at the application layer — that would require an idempotency key derived from the inbound request, not the commit. Out of v0 scope.

SHA-256 in hex is the v0 default. The choice is non-load-bearing; BLAKE3 or any other content-stable hash would work. The "canonical_json" requirement matters: serialising args via the PR #4 codec is canonical because the wire shape is fully specified.

## Open implementation questions

These are **deliberately unresolved here.** The implementation PR must answer them with the smallest commitment possible.

1. **Driver: `sqlx`, `tokio-postgres`, or `postgres` (sync)?** `sqlx` adds compile-time-checked queries but pulls in async + offline-mode-for-CI. `tokio-postgres` is leaner async. The sync `postgres` crate keeps `morpholog-core` sync and avoids tokio entirely. The choice affects (2) below.

2. **Sync vs async core.** `morpholog-core` is currently sync. Plausible paths:
   - Keep core sync; add a sync adapter inside `morpholog-core` using the `postgres` crate.
   - Keep core sync; add a separate `morpholog-postgres` crate that bridges async PG to the sync kernel (caller decides whether to block-on).
   - Make core async (a real refactor; benefit unclear at this scale).

3. **Where does the adapter live?** New `morpholog-postgres` crate vs. additional module in `morpholog-core` vs. feature-gated module. Probably a new crate, but commit in the implementation PR.

4. **Error surface for callers.** A `SERIALIZABLE` retry-needed error (SQLSTATE `40001`) is not a business rejection. The adapter probably returns it as a distinct `PgError::SerializationFailure` so callers retry without conflating business semantics. Confirm in the implementation PR.

5. **Connection vs pool.** Most likely `&Pool` checkout for the duration of the call, but pin during implementation.

6. **Locking strategy.** As an alternative or supplement to `SERIALIZABLE`, the adapter could `SELECT ... FOR UPDATE` rows matching specific transformation read patterns. Skip in v0; revisit if `40001` retry rates become painful.

## What this doc is not

This is a *design pin*, not a spec. It deliberately leaves the listed questions open. If the implementation PR proposes resolving them differently from the framing here, update this doc first, then write the code.

## Pointers

- Canonical schema: `crates/morpholog-core/sql/schema.sql`.
- Kernel (`propose()`, evaluator, IR types, codec): `crates/morpholog-core/src/lib.rs`.
- Runtime doctrine: `docs/runtime-semantics.md`.
