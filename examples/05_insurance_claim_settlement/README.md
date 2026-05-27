# Insurance Claim Settlement

A claim is reported against a policy. A claims handler, acting under delegated settlement authority, authorises a payment. Three months later, a regulator asks: *who decided this £40,000 payment, under what authority on what day, and was the policy aggregate limit consumed before or after this settlement?* The answer should be one query against the audit log - not detective work across a claims platform, a finance ledger, and an authority matrix that no longer agrees with itself.

This is exactly the kind of question UK insurance is being asked to answer with growing precision. In December 2025 the FCA accelerated its work on home and travel claims handling - delayed settlements, weak management information, oversight of outsourced services, and cash settlements made without enough evidence that the outcome was suitable. At the same time, Lloyd's stepped back from a single-platform claims rebuild and pivoted toward incremental data standards. Neither pressure asks for a bigger system; both ask for *defensible evidence regimes* that can sit alongside the platforms a real insurer already has.

This example is the smallest evidence regime that makes the regulator's question answerable by construction.

## The scenario

A commercial property insurer issues `policy_001` with a **£100,000 aggregate limit** - the cumulative cap across every settlement on this policy for the period. Issuing the policy also opens an operational counter, the policy's **remaining headroom**, which starts at £100,000 (no claim has consumed any of it yet).

A storm-damage claim is reported. A claims handler, **alex**, has been granted **£100,000 of settlement authority** for this claim class. Alex authorises a first payment of £60,000. Three things happen at the same moment, in one atomic transaction:

- The audit log records that alex authorised £60,000 against `claim_001` on a specific date.
- The policy's remaining headroom drops from £100,000 to £40,000.
- The reporting view `PolicyLimitUsage(policy_001)` reads £60,000 used.

Weeks later, a second storm exposes a different roof. `claim_002` is reported. Alex authorises £40,000. The runtime checks: the £60,000 already paid plus the £40,000 proposed equals £100,000 - exactly the aggregate. Admitted. Remaining headroom is now £0.

Then a third loss is reported. Alex attempts £30,000. The runtime checks: £100,000 already paid plus £30,000 proposed would be £130,000, past the aggregate. **Rejected at admission.** Nothing changes; no payment is staged; no outbox intent fires; the audit log records nothing because nothing was admitted.

The auditor three quarters later can ask:

- *Who authorised this £60,000 payment?* The `SettlementAuthorised` claim carries the actor.
- *Under what authority?* The `SettlementAuthority(alex, ...)` claim that was current on that day.
- *Was the policy still under its aggregate when this was admitted?* The reporting view evaluated as-of that transition gives the answer.
- *Could the third settlement have been admitted?* No - the runtime would not have admitted it. The absence of an audit row for that attempt is itself the answer.

## The program

See [`insurance_claim_settlement.morph`](insurance_claim_settlement.morph) for the surface form.

### Claims

| Predicate | Role |
| --- | --- |
| `Policy(policy_id, aggregate_limit)` | The policy and its cumulative cap across all settlements. The contractual figure, set once and never changed. |
| `PolicyHeadroom(policy_id, remaining)` | How much capacity is left on the policy right now. Starts equal to the aggregate limit; goes down as settlements are paid. The only claim in this example that is retracted and re-admitted as state changes. |
| `SettlementAuthority(actor, limit)` | An individual claims handler's per-settlement authority ceiling. |
| `ClaimReported(claim_id, policy_id, claimed_amount)` | A reported loss against a policy. The claimed amount is informational - it does not directly cap the settlement. |
| `SettlementAuthorised(claim_id, settlement_id, amount, actor)` | The audit record of an authorising decision. Records who decided what. |
| `SettlementPaid(policy_id, claim_id, settlement_id, amount)` | The payment record itself. |
| `CoverageTerms(policy_id, deductible, per_claim_limit)` | An optional per-claim layer: a deductible the insured bears and a per-claim limit, sitting *inside* the aggregate cap. |

### Invariants

| Invariant | What it says |
| --- | --- |
| `paid_implies_authorised` | Every payment must be backed by a matching authorisation. A hand-constructed payment with no authorisation is refused. |
| `paid_implies_headroom` | Every payment must be paired with a current `PolicyHeadroom` claim for its policy. Closes a gap in the conservation rule below (which is vacuously true if no headroom claim exists at all). |
| `at_most_one_policy_per_id` | A policy id identifies at most one policy. |
| `at_most_one_claim_report_per_id` | A claim id identifies at most one reported claim. |
| `at_most_one_headroom_per_policy` | A policy has at most one current headroom claim. Two competing headroom values would mean two answers to "how much is left?". |
| `settlement_id_uniquely_identifies_payment` | A settlement id identifies at most one payment. |
| `headroom_consumed_by_payment` | A *transition* invariant: the only one in this example that compares the state before a transaction to the state after. It says that the change in `PolicyHeadroom` for a policy must equal the total of newly-admitted `SettlementPaid` amounts for that policy. If a payment was admitted, the headroom must have gone down by exactly that amount. See "How payments consume headroom" below for the full story. |
| `settlement_within_eligible_payout` | When coverage terms are set, a settlement may not exceed the *eligible payout* - `min(per_claim_limit, max(0, loss - deductible))`: the loss above the deductible (`max(0, ...)` floors it at zero), capped at the per-claim limit (`min`). Vacuous for policies with no coverage terms, so it composes with, and sits inside, the aggregate cap. |
| `at_most_one_coverage_terms_per_policy` | A policy has at most one set of coverage terms. |
| `coverage_terms_within_range` | Coverage terms make business sense: the deductible is non-negative and the per-claim limit strictly positive. |

The absence of an invariant tying `SettlementAuthority` to historical `SettlementAuthorised` is deliberate. Future revocation of authority must not invalidate the historical record - same pattern as the verified-revenue and approval-controls examples. Authority is checked at admission; the record stands.

### Transformations

| Transformation | Effect |
| --- | --- |
| `issue_policy(policy_id, aggregate_limit)` | Opens a policy. Admits two claims: the immutable `Policy` and an initial `PolicyHeadroom` equal to the aggregate (no spend yet). |
| `report_claim(claim_id, policy_id, claimed_amount)` | Records a reported loss. Requires the policy to exist. |
| `grant_settlement_authority(actor, limit)` | Grants a claims handler their authority ceiling. |
| `set_coverage_terms(policy_id, deductible, per_claim_limit)` | Optionally sets the per-claim layer for a policy. Once set, the `settlement_within_eligible_payout` invariant bounds every settlement on the policy's claims. |
| `authorise_settlement(claim_id, settlement_id, amount)` | The main transformation. Does not take an `actor` parameter - the proposing actor flows through transition context. Checks three things up front: the claim was reported, the proposing actor has authority covering the amount, and the cumulative cap rule holds. If all three pass, it retracts the current headroom and admits a new one with `amount` subtracted, then admits the authorisation and payment records, then emits a payment-request intent. |

### Derived claims

| Derived | Definition |
| --- | --- |
| `PolicyLimitUsage(policy_id, used)` | A reporting view: total paid per policy, computed by summing all `SettlementPaid` amounts for that policy. Distinct from `PolicyHeadroom`, which is admitted operational state; this one is a reporting projection that can be replayed as-of any prior transition. |

## How to run it

```bash
# In-memory
cargo test -p morpholog-examples --test insurance_claim_settlement

# Durable (PostgreSQL adapter)
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    insurance_claim_settlement_full_chain_through_pg
```

In-memory tests cover: policy issuance and claim reporting; the actor authority gate (no authority, above limit, exact boundary); the aggregate cap gate (under, exact-fill, over, per-policy scoping); reporting-view enumeration; the existence invariants against hand-constructed orphan states; and the headroom-conservation invariant against hand-constructed buggy transformations. The PostgreSQL integration test walks the same scenario end to end and verifies the durable claims match.

---

## How payments consume headroom

Two rules in this example check the policy cap, and they answer different questions. Understanding the split is the main payoff of this example.

**The admission gate.** Inside `authorise_settlement`, a `require` clause checks that the sum of all prior payments plus the proposed amount fits inside the aggregate limit. If it doesn't, the settlement is refused at admission with a clear "would exceed the cap" rejection. This is what a claims handler interacts with: "is there enough capacity to make this payment?".

**The conservation invariant.** Separately, `headroom_consumed_by_payment` checks that *whenever* a payment is admitted, the policy's remaining headroom must drop by exactly the amount of the payment. The rule uses `pre(...)` to refer to the headroom value as it stood before the transaction, and compares it to the value after.

If both rules check the same idea, why have both? Because they answer different questions and would each catch different bugs:

- The require ensures the payment is **allowed**: it has enough room to fit.
- The invariant ensures the payment is **honest**: the transaction it sits in actually decremented the headroom by the right amount.

A buggy version of `authorise_settlement` that recorded a payment but forgot to update the headroom would pass the require (the cumulative sum still fits) and fail the invariant (the headroom did not move). A buggy version that decremented headroom by the wrong amount would also fail the invariant.

The conservation rule uses a sum rather than a per-payment equality for an important reason. If two payments of the same amount were admitted in one transaction while headroom was only decremented once, a per-payment check (`after = before - amt`) would pass for each payment individually, even though headroom should have gone down by twice the amount. Summing the newly-admitted payments and comparing once is the correct conservation law:

```text
after = before - sum(amt for each newly-admitted SettlementPaid)
```

This pattern - admission gate plus conservation invariant - generalises beyond insurance. Anywhere a system records actions that consume a finite resource (an account balance, a stock level, an entitlement, a budget) the same two-question split applies: "am I allowed to do this?" and "did I actually do it correctly?". The kernel mechanism that makes the second question expressible (the `pre(...)` wrapper) is also exercised, in a non-business setting, by the [chess example](../07_chess_transition_invariants/).

## What this example deliberately does not cover

Each pattern below is either already pinned by an earlier example or genuinely deferred until a forcing scenario arrives.

- **Coverage correction.** Restating a coverage basis with `Supersedes`, retracting standing on the prior basis, leaving historical decisions admitted - the verified-revenue example already pins this shape.
- **Reserve estimates.** Setting and revising loss reserves brings restatement-with-supersession into a domain where the current value is read frequently. A natural next example.
- **Standing for purpose.** "This coverage basis may be relied on for reserve-setting but not for final settlement" is the verified-revenue `AdmissibleFor` pattern. Adding it here would re-illustrate without teaching.
- **Effective time as a separate axis.** Whether the policy was in force at the loss date, whether the authority was active at the authorisation date - both are answerable today by admitting effective-time claims and querying as-of. A worked example that combines admission time with effective time across many transitions is on the roadmap.
- **Vulnerable customer handling and consumer-duty gates.** Real and load-bearing for the FCA story; they sit naturally as additional require clauses against admitted vulnerability flags.
- **Per-target reinsurance, treaty cessions, retrocession.** A full insurance evidence regime eventually reaches into reinsurance. Each layer is its own forcing scenario.

## What the FCA / Lloyd's framing buys

The regulatory context above is not an aspiration the runtime needs to grow into. It is the existing surface that this example already covers. `SettlementAuthorised` answers *who admitted what under what authority*. `PolicyLimitUsage` evaluated as-of any past transition answers *what was the aggregate position when this decision was made*. The audit log answers *what changed and in what order*. A real insurer's claims platform stays where it is; what Morpholog adds is the evidence kernel that can stand behind the decisions the platform records.
