# Revenue Restatement

The second worked example. Demonstrates how Morpholog handles **temporal correction**: when authoritative figures are revised after the fact, history is preserved, the "in-force" view moves cleanly, and no metadata is added to any claim.

## The scenario

A battery storage asset earns revenue each month. Three parties make claims about that revenue:

- **The optimiser** reports monthly revenue from its dispatch logs.
- **An independent verifier** issues a verified figure, sometimes weeks later, sometimes correcting itself.
- **The bank** recognises a revenue figure for debt-service-coverage calculations. The bank may only recognise an amount that matches the current independent verification.

When the verifier corrects a figure that the bank has already recognised against, the bank's previous recognition is no longer current — but it was correct at the time and should be preserved. The hard part: do this without adding `admitted_at` or `stale` fields to any claim.

## The program

See [`restatement.morph`](restatement.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `OptimiserReportedRevenue(asset, period, amount, statement_id)` | What the optimiser said. |
| `IndependentlyVerifiedRevenue(asset, period, amount, verification_id)` | What verification confirmed. |
| `BankRecognisedRevenue(asset, period, amount, recognition_id)` | The bank's recognition. **Append-only history.** |
| `CurrentBankRecognition(asset, period, recognition_id)` | **Pointer claim** that confers "in-force" standing on one specific recognition. |
| `Supersedes(new_subject, old_subject)` | Lineage between superseded claims. |

The split between `BankRecognisedRevenue` (history) and `CurrentBankRecognition` (pointer) is the key move. Currentness is a *separate claim*, not a field on the recognition.

### Invariants

| Invariant | Says |
| --- | --- |
| `current_recognition_matches_current_verification` | The current bank recognition must match a verification that is itself current (not superseded). |
| `at_most_one_current_recognition_per_asset_period` | There can be at most one current recognition per `(asset, period)`. |
| `at_most_one_direct_successor` | A subject is superseded by at most one direct successor. |

### Transformations

| Transformation | Effect |
| --- | --- |
| `admit_independent_verification` | First admission of a verified figure. |
| `recognise_bank_revenue` | Bank issues an initial recognition. |
| `correct_independent_verification` | Verifier supersedes a prior verification, *and retracts any dependent current bank pointer*. |
| `restate_bank_revenue` | Bank issues a new recognition that supersedes the prior one. |

The load-bearing detail is in `correct_independent_verification`: when the verifier corrects, the verifier's transformation also retracts the bank's current pointer. The historical `BankRecognisedRevenue` is preserved; only the pointer moves. The bank must then explicitly re-issue `restate_bank_revenue`.

## How to run it

The same scenario is proven at two layers — in-memory through the sync kernel, and durably through the PostgreSQL adapter.

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core full_restatement_chain
```

The chain test runs four transformations in sequence and verifies the final state has exactly seven claims:

```
IndependentlyVerifiedRevenue(asset_a, p, 92, ver_001)     ← original
IndependentlyVerifiedRevenue(asset_a, p, 91, ver_002)     ← corrected
Supersedes(ver_002, ver_001)                              ← lineage
BankRecognisedRevenue(asset_a, p, 92, rec_001)            ← preserved history
BankRecognisedRevenue(asset_a, p, 91, rec_002)            ← restated
Supersedes(rec_002, rec_001)                              ← lineage
CurrentBankRecognition(asset_a, p, rec_002)               ← pointer moved
```

No `CurrentBankRecognition(asset_a, p, rec_001)`. No metadata anywhere.

There is also a more focused test, `correct_independent_verification_retracts_dependent_current_pointer`, that isolates the load-bearing primitive: a single verifier correction against a pre-state that already has a current bank pointer. It verifies the staged retract happens and the historical recognition survives.

### Durable (PostgreSQL adapter)

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 revenue_restatement correct_verification
```

Two integration tests in `crates/morpholog-postgres/tests/integration.rs`:

- `revenue_restatement_full_chain_preserves_history_and_moves_pointer` — the full four-step chain (admit → recognise → correct → restate), end-to-end through `propose_against_pg`. Verifies all seven final claims are in `morpholog.claims`, four audit rows are recorded, four outbox intents are enqueued in causal order, the `CurrentBankRecognition(_, _, rec_001)` pointer is gone, and the historical `BankRecognisedRevenue(92, rec_001)` survives.
- `correct_verification_with_no_prior_rejects_and_writes_nothing` — a `correct_independent_verification` call with no matching prior verification fails its `require` and leaves all three tables empty.

This is the example that proves the *contested legitimacy* philosophy survives the durable boundary, not just the in-memory kernel: historical claims persist across PostgreSQL commits, current-standing pointers move via retraction in one atomic transaction, and supersession lineage is recorded as ordinary claims.

---

## Design notes

The instinct on first reading is to add metadata — an `admitted_at` field, a `stale` flag, an `authority` tag. **Try claims-about-claims first.** Metadata is a fallback for cases where standing, authority, validity, or lineage cannot be cleanly expressed as separate claims. In this example, claims-about-claims work.

> A lot of what feels like claim metadata — *current, stale, valid, in-force, authority, exception* — can be re-expressed as a **claim about a claim**. Ask "what additional claim gives this claim standing in this context?" before asking "what field should this claim carry?"

In this example:

- **Authority** lives in *predicate naming*: `OptimiserReported...` vs `IndependentlyVerified...` vs `BankRecognised...`.
- **Currentness** lives in *pointer claims*: `CurrentBankRecognition(asset, period, recognition_id)`.
- **Lineage** lives in *Supersedes claims*.
- **Admission history** lives in the *audit log* and in *append-only claim accumulation* (recognitions are never retracted, only the pointers to them are).

Three things are deliberately deferred:

1. **Cascading retraction.** `correct_independent_verification` retracts the dependent `CurrentBankRecognition`. If many predicates eventually depend on a given verification, the cascade grows. We'll need to decide whether such cascades stay as explicit retractions in transformation bodies or get derived automatically — probably the former until a pattern repeats three times.
2. **Cross-authority coupling.** The verifier's correction transformation "knows about" the bank's pointer structure. The coupling is structural, not authority-based: transformations are not owned by an authority, they are system-level transitions.
3. **Read-side.** "What is the current bank-recognised revenue for asset_a in 2026-04?" is a join over `CurrentBankRecognition` and `BankRecognisedRevenue` matching `recognition_id`. We have not built read-side machinery yet. The disciplined direction is *derived claims* — a first-class declaration of "this claim is true iff this expression over admitted claims is true, recomputable, materialisable, with provenance" — rather than an ad-hoc query DSL. The framing lives in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md); the example will be locked in by a third worked example focused on claim *standing* and admissibility-for-purpose.
