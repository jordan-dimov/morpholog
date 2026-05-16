# PostgreSQL Persistence (v0) — Design Pin

Status: design pin for the next implementation PR. **No code yet.**

PR #4 added a deterministic JSON codec for the runtime types. This document fixes the smallest acceptable design for `propose_against_pg()` so the implementation PR stays narrow and the kernel cannot get conceptually corrupted on the way to a database.

## Public goal

The **conceptual shape** (not the final signature):

```rust
fn propose_against_pg(
    conn: ???,                          // see open question 1
    transformation: &Transformation,
    args: Vec<EvalValue>,
    invariants: &[Invariant],
) -> Result<???, PgError>;
```

The return type is left open. Callers will likely want the committed `transition_id` on success — so the success branch probably carries it, and possibly the full `Outcome::Accepted` contents (asserted/retracted claims, emitted intents) — but the precise type is a deliberate non-commitment here. The function's *behaviour* is fixed; the type is not. The implementation PR will land a concrete return type, probably something like:

```rust
pub enum PgProposalOutcome {
    Committed {
        transition_id: Uuid,
        // possibly the Outcome::Accepted contents or a subset
    },
    Rejected { reason: String },
}
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

v0 pins `SERIALIZABLE`. The implementation PR should include **either**:

- a focused **SQLSTATE `40001` retry-classification test** that verifies the adapter distinguishes a `SERIALIZABLE` retry-needed error from a business `Rejected`, **or**
- a fully deterministic concurrent-violation test (two connections, orchestration barriers) — **only if it can be written without large test scaffolding**.

Do not let the first PG adapter PR become a concurrency lab. The core value is distinguishing retryable database conflicts from business rejection. Row-level `SELECT ... FOR UPDATE` may become an optimization later; it is **not** in v0.

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

SHA-256 in hex is the v0 default. The choice is non-load-bearing; BLAKE3 or any other content-stable hash would work.

The "canonical_json" requirement matters because the idempotency key depends on byte stability. For v0, **`canonical_json` means `serde_json` output using the PR #4 pinned wire shape.** This is stable for the current structs and enums because field order is fixed by the derived `Serialize` implementations and there are no map-like values. If map-like runtime values (e.g. a `HashMap<String, EvalValue>`) are introduced later, canonicalisation must be revisited — typically by sorting map keys — before the idempotency contract holds.

## Architecture (pinned)

- **`morpholog-core` remains synchronous and pure.** The semantic kernel evaluates `State + Transformation + Invariants → Outcome` with no I/O. Making it async would be conceptually wrong (the kernel does no awaiting) and mechanically infectious (async would spread through `find_matches`, `eval_value`, `propose` for zero gain). It also closes the door on running the kernel from CLI tools, model checkers, WASM, deterministic tests, or embedded simulations.

- **PostgreSQL persistence lives in a new `morpholog-postgres` crate.** This isolates the I/O adapter from the kernel and makes the sync/async boundary explicit. The workspace becomes three crates: `morpholog-cli`, `morpholog-core`, `morpholog-postgres`.

- **The adapter may expose async functions** (likely via `sqlx` or `tokio-postgres`). Async at the adapter boundary is fine: an async function can call the sync `propose()` directly. Example shape:

  ```rust
  let state: State = load_claims_into_state(&mut tx).await?;
  let outcome: Outcome = propose(transformation, args, &state, invariants)?;
  // ... if Accepted, write claims/audit/outbox via tx ...
  ```

- **Async must not infect the core evaluator or `propose()` API.** If a future change makes the kernel itself want to await, treat that as a significant design event and revisit this doc before proceeding.

## Open implementation questions

These are **deliberately unresolved here.** The implementation PR must answer them with the smallest commitment possible.

1. **Driver: `sqlx` vs `tokio-postgres`?** Both are async. `sqlx` adds compile-time-checked queries but pulls in offline-mode-for-CI machinery. `tokio-postgres` is leaner. (The sync `postgres` crate is no longer on the table — see Architecture above.)

2. **Error surface for callers.** A `SERIALIZABLE` retry-needed error (SQLSTATE `40001`) is not a business rejection. The adapter probably returns it as a distinct `PgError::SerializationFailure` so callers retry without conflating business semantics. Confirm in the implementation PR.

3. **Connection vs pool.** Most likely `&Pool` checkout for the duration of the call, but pin during implementation.

4. **Locking strategy.** As an alternative or supplement to `SERIALIZABLE`, the adapter could `SELECT ... FOR UPDATE` rows matching specific transformation read patterns. Skip in v0; revisit if `40001` retry rates become painful.

## What this doc is not

This is a *design pin*, not a spec. It deliberately leaves the listed questions open. If the implementation PR proposes resolving them differently from the framing here, update this doc first, then write the code.

## Pointers

- Canonical schema: `crates/morpholog-core/sql/schema.sql`.
- Kernel (`propose()`, evaluator, IR types, codec): `crates/morpholog-core/src/lib.rs`.
- Runtime doctrine: `docs/runtime-semantics.md`.
