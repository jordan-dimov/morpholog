# As-of evaluation: design sketch

Status: design sketch, not implementation. Pairs with the spike test at [`crates/morpholog-postgres/tests/as_of_spike.rs`](../crates/morpholog-postgres/tests/as_of_spike.rs), which demonstrates the target shape via hand-rolled audit-log replay. The implementation PR that follows should answer the open questions at the bottom of this doc; deletion of this doc happens once `docs/forced-by-examples.md` carries the retrospective.

## Problem

A regulated user does not only care what the system believes *now*. They care what the system believed at a specific moment in the past. An auditor asks "what did the trial balance look like at quarter-end before the late restatement?" A risk officer asks "what was net exposure 30 seconds before the settlement feed adjusted the numbers?"

Today, Morpholog has no way to answer those questions through its own surface. The audit log carries every transition with a UUIDv7 `transition_id`, a `committed_at` timestamp, and full asserted/retracted/intent payloads - exactly what an honest replay needs - but the kernel's read path (`list_derived`, `list_claims`, `enumerate_derived`) only operates against the *current* `State`. To get a historical answer, a caller has to write bespoke SQL outside Morpholog: query the audit table, replay each transition's asserts/retracts into a hand-rolled `Vec<ClaimInstance>`, wrap it in `State::from_claims`, and call the kernel against that.

The spike test in this PR is exactly that hand-rolled glue. It works, but every caller would have to write the same code. As-of evaluation is the minimum operational surface that closes the gap.

## The smallest forcing example: trial balance before vs after a restatement

Reuses the double-entry ledger:

1. Post `entry_001` with `cash` debit 100, `revenue` credit 100. Capture `transition_id_1`.
2. Post `entry_002` with `cash` debit 200, `revenue` credit 200. Capture `transition_id_2`.
3. Restate `entry_001` with corrected amount 150 (new entry, supersedes the original). Capture `transition_id_3`.

Three different trial balances exist in the same database:

| As-of | cash | revenue |
|---|--:|--:|
| `transition_id_1` (after first post) | 100 | -100 |
| `transition_id_2` (after second post, before restatement) | 300 | -300 |
| `transition_id_3` / current (after restatement) | 450 | -450 |

The current `list_derived` only returns the third. An auditor wanting the second has no Morpholog-native answer.

## Likely API shape

Two layers, both small. The kernel does not change.

```rust
// morpholog-postgres
pub async fn reconstruct_state_at(
    pool: &PgPool,
    as_of: Uuid,
) -> Result<State, PgError>;

// Convenience wrapper that callers will reach for most often.
pub async fn list_derived_at(
    pool: &PgPool,
    derived: &DerivedClaim,
    as_of: Uuid,
) -> Result<Vec<ClaimInstance>, PgError>;
```

`reconstruct_state_at` is the primitive: it queries the audit log for every transition committed at or before `as_of`, applies each transition's asserted and retracted claims to a running `State`, and returns the result. `list_derived_at` is the natural wrapper: reconstruct, then `enumerate_derived` against the reconstructed state. The kernel's `enumerate_derived` is unchanged - it already takes `&State`, and the reconstructed `State` is just a `State` of a particular epoch.

Symmetric helpers for the rest of the inspection surface follow naturally:

```rust
pub async fn list_claims_at(
    pool: &PgPool,
    as_of: Uuid,
) -> Result<Vec<ClaimInstance>, PgError>;
```

CLI surface:

```bash
morpholog inspect claims --as-of <transition_id>
morpholog inspect derived <program> <name> --as-of <transition_id>
```

`morpholog inspect audit` already lists transitions; users discover the right `transition_id` by reading the audit output.

## What it is NOT (in v0)

Worth pinning so the implementation PR does not drift:

- **Not materialised.** Every as-of call replays from the start of the audit log. Cost is O(transitions up to T). For long-lived audit logs this becomes painful; snapshots / materialisation are the next-forced optimisation, but not in scope here. The bench has to show the pain before the cure is built.
- **Not visible to invariants or transformations.** As-of is a read-side operator. An invariant that says "no transaction whose retroactive effect would change the closed Q1 trial balance" is a real concern, but pulls in evaluation-order questions that need their own forcing example.
- **Not differentiated from effective time.** "Knowledge time" (when the system learned something) is what as-of provides. "Effective time" (when something becomes true in the modelled world) is already expressible as a claim - e.g. `EffectiveFor(subject, period)` - and queryable via any normal predicate match. The two axes can be combined by the caller; the kernel does not need a built-in operator for that.
- **Not historical invariant versioning.** v0 pins every invariant at `version: 1`, so the question of "which invariant version applied at T?" has no current answer that varies. The audit row records `invariant_epoch`; the implementation PR should pass that through honestly so a future version-changing PR can use it without rewriting the as-of operator.
- **Not a query DSL.** As-of is a coordinate, not a query language. Filters, joins, projections - all still expressed through derived claims (which then run at the chosen as-of).
- **Not historical write.** "Commit this transformation as if it happened at T" is meaningless under append-only semantics. The audit log records when something was admitted, not when it became true. Time-travelling writes would silently corrupt the audit contract.

## Open design questions

The implementation PR has to answer these explicitly. The spike test does not commit to any of them.

1. **Coordinate shape: `transition_id` (UUIDv7), `committed_at` timestamp, or both?** Lean: `transition_id` as the primary. It is the deterministic primary key of the audit row, it is what `morpholog inspect audit` already prints, and UUIDv7 is byte-wise time-ordered so SQL comparisons are cheap. `committed_at` could be a CLI convenience (`--at-time 2026-05-17T14:30:00Z` translates to "the last `transition_id` whose `committed_at` is `<=` the timestamp"), but the primary coordinate stays UUID-typed.

2. **Inclusive or exclusive of `T`?** Lean: inclusive. "As of T" canonically means "what was the state right after T committed." The exclusive case ("what did we know just before T?") is useful but rarer; expose it via a separate `--before <id>` flag or `before_transition_at` helper if a user genuinely needs it.

3. **Ordering: `transition_id` byte order or `committed_at`?** They usually agree (UUIDv7's prefix is the timestamp) but diverge under concurrent commits where the wall-clock interleaves differently from the per-client generation. Lean: order by `committed_at`, tie-break on `transition_id`. This matches `list_audit_rows`'s existing contract, and matches the strict commit-time chronology the audit log promises.

4. **Replay performance / when does materialisation become forced?** v0 ships full replay. The bench will need a new scenario - long audit log, single as-of query - to show the cost. Once it does, the optimisation is either incremental snapshots, materialised views, or kernel-side incremental state delta caching. Each has trade-offs; pick the one the bench forces.

5. **Invariant version at T.** v0 invariants are all `version: 1`, so the choice does not bite yet. But the API has to commit: when an as-of evaluation involves invariant checks (which, today, it does not - as-of is read-only), should the invariant version active *at T* govern (per `audit.invariant_epoch`) or the *current* version? Lean: at-T for audit-style queries; deferred until invariant versioning is actually live.

6. **Failure mode for an unknown `transition_id`.** Reconstructing as-of a `transition_id` that does not exist in `audit` is an error. Reconstructing as-of `Uuid::nil()` (or the all-zero UUID, smaller than every real UUIDv7) is the empty state, which is meaningful. Reconstructing as-of a `transition_id` larger than any committed one is the current state. The implementation should commit to all three behaviours explicitly.

7. **Interaction with predicate-scoped loading (PR #25).** `list_derived` was just rewired to load only the predicates a derived claim references. The as-of version should preserve that property - i.e. `list_derived_at` should walk the audit log but only retain claims whose predicate is in the derived's footprint. The implementation has to thread `predicates_referenced_by_derived` into the replay loop and discard out-of-footprint claims as they appear. Not hard, but needs explicit handling.

8. **CLI exit codes and JSON shape.** `morpholog inspect ... --as-of <transition_id>` against a missing transition should fail with a clear error (exit 1, stderr message). Against a valid transition the output shape should be byte-identical to the current `inspect` output. No special JSON envelope advertising "this is an as-of result" - the caller asked for it; the output is just the answer.

## What this PR delivers

- This document.
- One spike test in `crates/morpholog-postgres/tests/as_of_spike.rs`. The test sets up the trial-balance-before-and-after scenario above, manually queries the audit log, manually replays it up to a chosen `transition_id`, and asserts that the resulting trial balance matches the historical state, not the current one. The same test also verifies that the current state shows the post-restatement values, so the as-of semantics are pinned by a positive AND a negative case.

The spike is meant to be ugly: every caller would have to do this themselves today. That ugliness is the case for the implementation PR.

## What this PR does NOT deliver

- `reconstruct_state_at` or any other helper on `morpholog-postgres`.
- The CLI `--as-of` flag.
- Any kernel change. The kernel's `enumerate_derived` and `eval_invariant` already take `&State`; reconstruction is purely an adapter concern.
- Materialisation, snapshots, or any other replay optimisation.
- An updated `forced-by-examples.md` entry. That belongs in the implementation PR's commit, after the open questions above are settled by the act of implementing.
