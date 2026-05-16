# Example 2: Revenue Restatement

Status: design sketch only. No Rust, no IR code, no parser. The point of this example is to test whether Morpholog's ontology survives contested, temporal, partially corrected reality without metadata on claims.

## What this example is for

Settlement netting was clean. Every claim was authoritative; the only question was admissibility against a single invariant set. This example deliberately introduces three forces that the clean kernel did not face:

1. **Multiple authorities** speaking about the same underlying event with different numbers.
2. **Time** — revenue is reported, then later corrected. The system must preserve both versions and know which is current.
3. **Read-side gating** — a downstream claim (bank recognition) may only be derived from one specific authority's *current* claim.

The design question this example must answer:

> Can authority, supersession, and "currentness" remain ordinary claims — or do we need claim metadata?

Bias: keep them as ordinary claims. Add metadata only if the example proves it unavoidable.

## Domain

A battery energy storage asset earns revenue in monthly periods. Three parties make claims about that revenue:

- **The optimiser** reports monthly revenue based on its own dispatch logs.
- **An independent verifier** issues a verified revenue figure, sometimes weeks later and sometimes disagreeing with the optimiser.
- **The bank** recognises a revenue figure for debt-service-coverage calculations. The bank may only recognise an amount that matches the current independent verification.

Either party may issue a correction. A correction does not erase the prior claim; it adds a new claim and records the supersession.

## Claims (predicates)

```
OptimiserReportedRevenue(asset, period, amount, statement_id)
IndependentlyVerifiedRevenue(asset, period, amount, verification_id)
BankRecognisedRevenue(asset, period, amount, recognition_id)
CurrentBankRecognition(asset, period, recognition_id)
Supersedes(new_subject, old_subject)
```

- `statement_id`, `verification_id`, `recognition_id` are subjects (externally supplied or `new Subject()`); they distinguish multiple admitted records for the same `(asset, period)`.
- `BankRecognisedRevenue` is **append-only history**. Every bank recognition ever admitted stays in state. Each carries its own `recognition_id` so multiple records can coexist for the same `(asset, period)`.
- `CurrentBankRecognition` is a **pointer claim** that confers "in-force" standing on one specific recognition. It is retracted and re-asserted as restatement occurs.
- `Supersedes(a, b)` records lineage between subjects.
- No claim carries authority/epoch/timestamp metadata. Authority lives in *predicate naming*. Currentness lives in *pointer claims*. Lineage lives in *Supersedes claims*.

## Invariants

**I1. The current bank recognition must match a current verification.**

```
CurrentBankRecognition(asset, period, r) and
BankRecognisedRevenue(asset, period, amount, r)
implies
    exists v:
        IndependentlyVerifiedRevenue(asset, period, amount, v)
        and not exists newer: Supersedes(newer, v)
```

Historical bank recognitions (those without a current pointer) are deliberately unconstrained. They were admitted in good faith against the verification current at the time; superseding the underlying verification does not retroactively invalidate them.

**I2. At most one current bank recognition per (asset, period).**

```
CurrentBankRecognition(asset, period, a) and
CurrentBankRecognition(asset, period, b) implies a == b
```

**I3. A subject is superseded by at most one direct successor.**

```
Supersedes(new_a, old) and Supersedes(new_b, old) implies new_a == new_b
```

I3 is not strictly required to make scenarios 1–3 work, but it prevents a class of supersession-graph anomalies. Drop it for v0 if it bloats the example.

## Transformations

```
admit_optimiser_statement(asset, period, amount, statement_id)
correct_optimiser_statement(asset, period, new_amount, new_statement_id, prior_statement_id)

admit_independent_verification(asset, period, amount, verification_id)
correct_independent_verification(asset, period, new_amount, new_verification_id, prior_verification_id)

recognise_bank_revenue(asset, period, amount, recognition_id)
restate_bank_revenue(asset, period, new_amount, new_recognition_id, prior_recognition_id)
```

Pseudo-code intent (not IR):

- **`admit_*`** transformations require that the supplied subject id is fresh for this `(asset, period)`, then assert the claim.
- **`correct_optimiser_statement`** requires the prior statement exists and is not superseded, then asserts the new statement and `Supersedes(new, prior)`.
- **`correct_independent_verification`** does the same — *and additionally retracts any `CurrentBankRecognition(asset, period, _)` for the affected (asset, period)*. This is the load-bearing detail: a verification correction invalidates dependent current recognitions immediately, so the bank cannot remain "in-force" against superseded evidence. Historical `BankRecognisedRevenue` is preserved; only the pointer moves.
- **`recognise_bank_revenue`** requires no `CurrentBankRecognition(asset, period, _)` exists. Asserts `BankRecognisedRevenue(...)` and `CurrentBankRecognition(...)`. Invariant I1 enforces alignment with the current verification at commit.
- **`restate_bank_revenue`** retracts the prior `CurrentBankRecognition`, asserts the new `BankRecognisedRevenue` and `CurrentBankRecognition`, asserts `Supersedes(new_recognition_id, prior_recognition_id)`. I1 enforces alignment with the (now newer) current verification.

## Scenario 1: Happy path

Pre-state: empty for `(asset_a, 2026-04)`.

```
admit_optimiser_statement(asset_a, 2026-04, 100k, opt_001)
admit_independent_verification(asset_a, 2026-04, 92k, ver_001)
recognise_bank_revenue(asset_a, 2026-04, 92k, rec_001)
```

Each commits. After the third, state contains the optimiser statement, the verification, `BankRecognisedRevenue(..., rec_001)`, and `CurrentBankRecognition(..., rec_001)`. I1 holds (current pointer + recognition match current verification). I2 holds trivially. I3 holds trivially.

## Scenario 2: Restatement

Continuing from Scenario 1's committed state.

```
correct_independent_verification(asset_a, 2026-04, 91.7k, ver_002, ver_001)
```

This is the load-bearing transformation. It:

1. Requires `ver_001` exists and is not superseded.
2. Asserts `IndependentlyVerifiedRevenue(asset_a, 2026-04, 91.7k, ver_002)`.
3. Asserts `Supersedes(ver_002, ver_001)`.
4. **Retracts `CurrentBankRecognition(asset_a, 2026-04, rec_001)`** — because the verification that supported it has just been superseded.

At commit, the candidate state has both verifications (with supersession), both verification claims, the historical `BankRecognisedRevenue(..., rec_001)`, and *no* current bank recognition for `(asset_a, 2026-04)`. I1 vacuously holds — there is no `Current + BR` pair to check. I2 holds. I3 holds (one supersession edge). Commits.

The bank must now explicitly re-issue:

```
restate_bank_revenue(asset_a, 2026-04, 91.7k, rec_002, rec_001)
```

This asserts `BankRecognisedRevenue(..., 91.7k, rec_002)`, `CurrentBankRecognition(..., rec_002)`, and `Supersedes(rec_002, rec_001)`. At commit, I1 checks the new `Current + BR` pair: `BR(asset_a, 2026-04, 91.7k, rec_002)` and there exists `IV(asset_a, 2026-04, 91.7k, ver_002)` which is not superseded. Commits.

History is preserved: `BankRecognisedRevenue(asset_a, 2026-04, 92k, rec_001)` is still in state, never retracted. The pointer moved.

## Scenario 3: Rejected bank recognition

Pre-state: only an optimiser statement exists; no independent verification yet.

```
admit_optimiser_statement(asset_b, 2026-04, 80k, opt_010)
recognise_bank_revenue(asset_b, 2026-04, 80k, rec_x)   # rejected
```

The `recognise_bank_revenue` transformation passes its `require` (no current pointer exists). It stages `BankRecognisedRevenue(asset_b, 2026-04, 80k, rec_x)` and `CurrentBankRecognition(asset_b, 2026-04, rec_x)`. At invariant check, I1 fails: there is no `IndependentlyVerifiedRevenue(asset_b, 2026-04, 80k, _)` in candidate state. Atomic rollback. No claim asserted.

This is the same rejection-on-invariant case we already proved in netting.

## What this example teaches

The instinct on first reading is to add metadata — an `admitted_at` field, a `stale` flag, an `authority` tag. **Try claims-about-claims first.** Metadata is a fallback for cases where standing, authority, validity, or lineage cannot be cleanly expressed as separate claims. In this example, claims-about-claims work.

The cleaner answer is to **split the concept**. What feels like "currentness as a property of the recognition" is better expressed as a *separate claim* — a pointer that confers in-force standing. Historical recognitions remain admitted; the pointer moves.

> A lot of what feels like claim metadata — *current, stale, valid, in-force, authority, exception* — can be re-expressed as a **claim about a claim**. Ask "what additional claim gives this claim standing in this context?" before asking "what field should this claim carry?"

In this example:

- **Authority** lives in *predicate naming*: `OptimiserReported...` vs `IndependentlyVerified...` vs `BankRecognised...`.
- **Currentness** lives in *pointer claims*: `CurrentBankRecognition(asset, period, recognition_id)`.
- **Lineage** lives in *Supersedes claims*.
- **Admission history** lives in the *audit log* (already designed) and in *append-only claim accumulation* (recognitions are never retracted, only the pointers to them are).

Three things are deliberately deferred and worth flagging:

1. **Cascading retraction.** `correct_independent_verification` retracts the dependent `CurrentBankRecognition`. This requires the transformation body to query state for affected pointers. The IR can express it, but if many predicates eventually depend on a given verification, the cascade grows. We'll need to decide whether such cascades stay as explicit retractions in transformation bodies or get derived automatically — probably the former until a pattern repeats three times.
2. **Cross-authority coupling.** The verifier's correction transformation "knows about" the bank's pointer structure. That coupling is structural, not authority-based: transformations are not owned by an authority, they are governed system transitions. The line `retract CurrentBankRecognition(asset, period, _)` is system-level correctness, not bank-side action. Reframe accordingly in any prose.
3. **Read-side.** The natural query "what is the current bank-recognised revenue for asset_a in 2026-04?" is a join over `CurrentBankRecognition` and `BankRecognisedRevenue` matching `recognition_id`. We have not addressed how reads work yet. The example shows they will be join-heavy.

If this sketch survives review, the next step is to encode it as Rust IR — same approach as netting — and prove with tests that the three scenarios commit/reject as described. That would be the second proof-of-concept transformation set, alongside settlement netting.
