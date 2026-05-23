# Insurance Claim Settlement

A claim is reported against a policy. A claims handler, acting under delegated settlement authority, authorises a payment. Three months later, a regulator asks: *who decided this £40,000 payment, under what authority on what day, and was the policy aggregate limit consumed before or after this settlement?* The answer should be one query against the audit log - not detective work across a claims platform, a finance ledger, and an authority matrix that no longer agrees with itself.

This is exactly the kind of question UK insurance is being asked to answer with growing precision. In December 2025 the FCA accelerated its work on home and travel claims handling - delayed settlements, weak management information, oversight of outsourced services, and cash settlements made without enough evidence that the outcome was suitable. At the same time, Lloyd's stepped back from a single-platform claims rebuild and pivoted toward incremental data standards. Neither pressure asks for a bigger system; both ask for *defensible evidence regimes* that can sit alongside the platforms a real insurer already has.

This example is the smallest evidence regime that makes the regulator's question answerable by construction.

## The scenario

A commercial property insurer issues `policy_001` with a **£100,000 aggregate limit** - the cumulative cap across every settlement on this policy for the period.

A storm-damage claim is reported. A claims handler, **alex**, has been granted **£100,000 of settlement authority** for this claim class. Alex authorises a first payment of £60,000. Reality holds:

- The audit log records that alex authorised £60,000 against `claim_001` on a specific date.
- `PolicyLimitUsage(policy_001)` reads **£60,000** used; **£40,000** of aggregate remains.

Weeks later, a second storm exposes a different roof. `claim_002` is reported. Alex authorises **£40,000**. The runtime evaluates the cumulative-cap rule against the *candidate* state: £60,000 already paid plus £40,000 proposed equals £100,000, exactly the aggregate. Admitted. Boundary equality is inclusive.

Then a third loss is reported. Alex attempts £30,000. The runtime evaluates £100,000 already paid plus £30,000 proposed, which would be £130,000 - past the aggregate. **Rejected at admission.** Nothing changes; no payment is staged; no outbox intent fires; the audit log records nothing because nothing was admitted.

The auditor three quarters later can ask:

- *Who authorised this £60,000 payment?* The `SettlementAuthorised` claim carries the actor.
- *Under what authority?* The `SettlementAuthority(alex, ...)` claim that was current on that day.
- *Was the policy still under its aggregate when this was admitted?* `PolicyLimitUsage` evaluated as-of that transition gives the answer.
- *Could the third settlement have been admitted?* No - the runtime would not have admitted it. The absence of an audit row for that attempt is itself the answer.

## The program

See [`insurance_claim_settlement.morph`](insurance_claim_settlement.morph) for the illustrative surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `Policy(policy_id, aggregate_limit)` | The policy and its cumulative cap across all settlements. Append-only. |
| `PolicyHeadroom(policy_id, remaining)` | Operational remaining-capacity counter. Distinct from `Policy` (the immutable contractual cap) and from `PolicyLimitUsage` (the read-side reporting view). Retract-and-reassert per settlement; the only retractable claim family in this example. |
| `SettlementAuthority(actor, limit)` | An actor's per-settlement authority ceiling. The runtime supports `Stmt::Retract`, so revocation is expressible; this example deliberately does not include a revoke transformation - the approval-controls example already pins that pattern. |
| `ClaimReported(claim_id, policy_id, claimed_amount)` | A reported loss against a policy. Append-only. The claimed amount is informational; the binding constraint is the aggregate limit. |
| `SettlementAuthorised(claim_id, settlement_id, amount, actor)` | The audit-grade record of an authorising decision. Append-only. Fourth arg is the proposing actor; the audit row carries the same identity. |
| `SettlementPaid(policy_id, claim_id, settlement_id, amount)` | The payment claim the cumulative-cap rule reads from. Append-only. |

### Invariants

| Invariant | What it pins |
| --- | --- |
| `paid_implies_authorised` | Every `SettlementPaid` must be backed by a matching `SettlementAuthorised`. The transformation never asserts one without the other, but the runtime contract is "candidate state is admissible under invariants regardless of how it got there." A hand-constructed orphan payment is refused. |
| `paid_implies_headroom` | Every `SettlementPaid(p, ...)` must be paired with a current `PolicyHeadroom(p, _)` claim. Closes a gap in the conservation invariant: the conservation rule's pre/post guard fails (and the implies is vacuously true) when no PolicyHeadroom exists for the policy. Without this existence pairing, a candidate state with payments but no headroom would slip through. |
| `at_most_one_policy_per_id` | A `policy_id` admits at most one `Policy` claim. Pins the structural uniqueness `authorise_settlement`'s `ValueOf(Policy(policy_id, _))` depends on. |
| `at_most_one_claim_report_per_id` | Same shape for `ClaimReported`: duplicate reports against one `claim_id` are refused. Pins the structural uniqueness `authorise_settlement`'s `ValueOf(ClaimReported(claim_id, _, _))` depends on. |
| `at_most_one_headroom_per_policy` | A `policy_id` admits at most one `PolicyHeadroom` at any moment. Two competing headroom claims would mean two answers to "how much capacity remains?" - exactly the ambiguity a governed model exists to forbid. |
| `settlement_id_uniquely_identifies_payment` | A `settlement_id` identifies at most one payment. Two `SettlementPaid` claims sharing a `settlement_id` must agree on every other field. Identity-side guarantee for audit-grade settlement evidence. |
| `headroom_consumed_by_payment` | **Transition invariant.** Per-policy headroom delta conservation: the change in `PolicyHeadroom(p, _)` must equal the total of newly-admitted `SettlementPaid` amounts for `p` in this transition. Uses `pre(...)` to reference the pre-transition state; the runtime evaluates `pre(PolicyHeadroom(p, before)) and PolicyHeadroom(p, after) implies after = before - sum(amt | SettlementPaid(p, _, s, amt) and not pre(SettlementPaid(p, _, s, amt)))`. Distinct from the aggregate-limit `require` gate: the require asks "is there enough headroom?", the invariant asks "does the headroom delta equal the total new payments?". A buggy transformation that staged `SettlementPaid` without retracting and re-asserting `PolicyHeadroom` would pass the require and fail the invariant; a multi-payment transition that under-decremented headroom would similarly fail. |

The absence of an invariant tying `SettlementAuthority` to historical `SettlementAuthorised` is deliberate. Future revocation of authority must not invalidate the historical record - same require-vs-invariant doctrine as the verified-revenue and approval-controls examples. Authority is checked at admission; the record stands.

### Transformations

| Transformation | Effect |
| --- | --- |
| `issue_policy(policy_id, aggregate_limit)` | Opens a policy with its aggregate cap. Admits two claims: `Policy(policy_id, aggregate_limit)` (immutable contract) and an initial `PolicyHeadroom(policy_id, aggregate_limit)` (operational counter, starts equal to the aggregate). |
| `report_claim(claim_id, policy_id, claimed_amount)` | Records a reported loss. Requires the policy to exist. |
| `grant_settlement_authority(actor, limit)` | Asserts `SettlementAuthority`. Ungated in v0 - administrative authority for granting is out of scope. |
| `authorise_settlement(claim_id, settlement_id, amount)` | The load-bearing transformation. **Declares no `actor` parameter.** The proposing actor flows through transition context as `$actor`. Gates admission on three conditions: the claim was reported, the proposing actor has authority covering the proposed amount, and the cumulative `Le(Add(Sum(paid), amount), aggregate_limit)` rule holds against the policy. On admission, retracts the current `PolicyHeadroom` and asserts a new one with `amount` consumed; the `headroom_consumed_by_payment` transition invariant verifies the delta. |

### Derived claims

| Derived | Definition |
| --- | --- |
| `PolicyLimitUsage(policy_id, used)` | `used = sum { paid | SettlementPaid(policy_id, _, _, paid) }`. One row per distinct policy that has at least one admitted payment. Enumerated on demand; not persisted, not visible to invariants or transformations. As-of replay reconstructs the usage as-it-stood at any prior transition. |

## How to run it

```bash
# In-memory
cargo test -p morpholog-examples --test insurance_claim_settlement

# Durable (PostgreSQL adapter)
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    insurance_claim_settlement_full_chain_through_pg
```

In-memory tests pin: policy issuance and claim reporting; actor authority gate (no authority, above limit, exact boundary); cumulative aggregate gate (under cap, exact-fill boundary, over cap, per-policy scoping); `PolicyLimitUsage` enumeration; the `paid_implies_authorised` invariant against a hand-constructed orphan payment; the conservation invariant on a hand-constructed buggy transformation that admits a payment without consuming headroom; and the conservation invariant on a hand-constructed buggy multi-payment transformation that decrements headroom only once - the case that justifies the sum-based form over per-row equality. The PG integration test walks the same story end to end through `propose_against_pg`.

---

## Design notes

### What `Expr::Pre` earned its place for

The aggregate-limit `require` rule (`Le(Add(Sum(paid), amount), aggregate_limit)`) is an *admission gate*: it asks whether the proposed payment fits inside the remaining cap. It runs against pre-state. It cannot say anything about the relationship between the pre-state and the post-state - if a buggy transformation admitted `SettlementPaid` without retracting and re-asserting `PolicyHeadroom`, the require would still pass on the next call, because the require recomputes the sum every time.

`headroom_consumed_by_payment` closes that gap. It is a *transition invariant*: it asks whether the post-state stands in the right relationship to the pre-state. The rule is **per-policy delta conservation**: for each policy whose headroom exists both before and after, the change in remaining capacity must equal the total amount of newly-admitted `SettlementPaid` claims for that policy in this transition. A state invariant could not say this: both `PolicyHeadroom(p, 100k)` and `SettlementPaid(p, ..., 30k)` are perfectly admissible singly; only the pre-vs-post relationship between them falsifies the rule.

The sum-based encoding matters: an earlier draft compared each newly-admitted payment individually to the headroom delta (`after = before - amt`). For the current one-payment transformation that degenerates to the same check, but as a general conservation law it is too weak. A hypothetical multi-payment transition admitting two same-amount settlements while decrementing headroom only once would pass each per-row equation (`70 = 100 - 30`) while consuming twice the headroom it credits. The sum form is the actual conservation law: `after = before - sum_of_new_payments`. It catches the multi-payment edge case, headroom mutation without payment, payment without headroom mutation, and wrong-decrement-amount in one shape.

The two checks coexist by design. The require is the lawful business-outcome gate ("not enough headroom"); the invariant is the kernel-error trap ("the transformation lied about what it did"). A regulator's view is that this kind of conservation rule - "a payment is not merely below the cap; it must actually consume the entitlement it claims to, in aggregate" - is the difference between a system that adds up correctly today and a system whose audit trail you can stand behind in five years.

### What `Expr::Add` earned its place for

This example forced `Expr::Add` into the kernel. The cumulative-cap rule

```text
Le(Add(running_paid, proposed), aggregate_limit)
```

is the natural shape: "the amount already paid, plus the amount about to be paid, must not exceed the cap." Encoding it as `Le(proposed, Sub(aggregate, running_paid))` works but contorts the business rule. Once the load-bearing rule reads cleanly with `Add`, the primitive earns its place. Together with `Expr::Sub` (forced by the trial-balance derived claim), this is the entire decimal-arithmetic surface in v0. No multiplication or division until a real example forces them.

### What this example deliberately does not cover

The scope of this example is intentionally narrow. Every pattern below is either already pinned by an earlier example or genuinely deferred until a forcing scenario arrives.

- **Coverage correction.** Restating a coverage basis with `Supersedes`, retracting standing on the prior basis, leaving historical decisions admitted - the verified-revenue example already pins this shape. The same pattern would apply to insurance if a forcing scenario combined coverage correction with cumulative-cap evolution; that is its own example, not this one.
- **Reserve estimates.** Setting and revising loss reserves is the natural next forcing function for a future example: it brings restatement-with-supersession into a domain where the current value is read frequently (regulator reports, capital adequacy). Out of scope here.
- **Standing for purpose.** "This coverage basis may be relied on for reserve-setting but not for final settlement" is the verified-revenue `AdmissibleFor` pattern. Adding it here would re-illustrate without teaching.
- **Effective time as a separate axis.** Whether the policy was in force at the loss date, whether the authority was active at the authorisation date - both are answerable today by admitting effective-time claims and querying as-of. A genuine effective-time worked example (one that combines admission time with effective time across many transitions) is in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md)'s roadmap.
- **Per-claim limits and deductibles.** `PolicyClaimLimit(policy_id, claim_type, limit)` and `PolicyExcess(policy_id, claim_type, excess)` add no new IR shape - they are more requires using the same primitives. Future work, not blocked on anything.
- **Vulnerable customer handling and consumer-duty gates.** These are real and load-bearing for the FCA story; they sit naturally as additional `require` clauses against admitted `VulnerableCustomerFlag(claim_id)` claims. Out of scope for the cumulative-cap proof.
- **Per-target reinsurance, treaty cessions, retrocession.** A full insurance evidence regime eventually reaches into reinsurance. Each layer is its own forcing scenario.

### What the FCA / Lloyd's framing buys

The regulatory context above is not an aspiration the runtime needs to grow into. It is the existing surface that this example's primitives already cover. `SettlementAuthorised` answers *who admitted what under what authority*. `PolicyLimitUsage` as-of answers *what was the aggregate position when this was decided*. The audit log answers *what changed and in what order*. A real insurer's claims platform stays where it is; what Morpholog adds is the evidence kernel that can stand behind the decisions the platform records.
