# Claim Standing

The third worked example. Demonstrates how Morpholog handles **admissibility-for-purpose**: when the same admitted claim is usable for one decision but not another, when standing comes from different authorities, and when standing is lost without the underlying claim being touched.

## The scenario

A battery storage asset's monthly revenue has been independently verified at £91k. The same verification figure may end up being used for several different decisions, by different parties, under different rules:

- The bank uses it to test a debt-service-coverage covenant. Standing for this purpose is granted by the bank's credit committee under the loan agreement.
- Investor relations uses it for shareholder reporting. Standing for this purpose is granted by an entirely different authority under entirely different rules.
- The asset owner might consume it on an internal dashboard, with no formal standing required at all.

The verification is *the same number*. What changes between use-cases is **who has admitted it as legitimate for what purpose**. In conventional systems this distinction is implicit - buried in process documents, side-table lookups, or convention. In Morpholog it is a first-class claim.

When a regulator, auditor, or counterparty asks *"was this figure admissible for this decision at the time the decision was made?"* the answer is a structured query over claims with a definite answer. When standing is later revoked - perhaps the verification is too old, perhaps the authority's terms changed - the historical decision that relied on standing-at-the-time is **not** invalidated. The system does not pretend the past did not happen.

## The program

See [`standing.morph`](standing.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `IndependentlyVerifiedRevenue(asset, period, amount, verification_id)` | The underlying revenue claim. **Never mutated by anything in this example.** |
| `AdmissibleFor(claim_id, purpose)` | Active admissibility of a claim for a specific purpose. **Retractable.** |
| `StandingGrantedBy(claim_id, purpose, authority, grant_id)` | Provenance of an admissibility grant. **Append-only.** |
| `StandingRevoked(claim_id, purpose, revocation_id)` | Provenance of a revocation. **Append-only.** |
| `DebtServiceRevenue(asset, period, amount, decision_id, verification_id)` | A decision that relied on a specific verification for the bank-debt-service purpose. |
| `InvestorReportedRevenue(asset, period, amount, report_id, verification_id)` | A decision that relied on a specific verification for the investor-reporting purpose. |

The split between `AdmissibleFor` (active, retractable) and `StandingGrantedBy` / `StandingRevoked` (append-only provenance) is the key move. The underlying verification is *never* edited, *never* retracted, *never* superseded by anything in this example. Standing is acquired and lost independently of it.

### Invariants

| Invariant | Says |
| --- | --- |
| `admissibility_has_provenance` | Every active `AdmissibleFor(c, p)` must be backed by some `StandingGrantedBy(c, p, _, _)`. No admissibility without a recorded grant. |
| `admissibility_excludes_revocation` | `AdmissibleFor(c, p)` cannot coexist with any `StandingRevoked(c, p, _)`. Once revocation is recorded for a (claim, purpose) pair, active admissibility must have been retracted. |

**Note what is deliberately not an invariant:** there is no rule saying "every `DebtServiceRevenue` claim implies an active `AdmissibleFor`." This is intentional. Decisions are gated at admission time via `require` clauses in the decision transformations. Once admitted, a decision is locked in. If an invariant tied decisions to live standing, revoking standing would force the runtime to either (a) reject the revoke (breaking the rule that standing can be lost) or (b) cascade-retract every historical decision that relied on it (breaking the rule that history is preserved). Neither is what we want.

The matching real-world principle: a bank's covenant calculation, made on June 30 against the then-current admissible figure, stays valid even if standing is later revoked or the figure is later restated. The legitimacy of a *past* decision was established when it was made; revoking standing prevents *future* decisions, not past ones.

### Transformations

| Transformation | Effect |
| --- | --- |
| `admit_independent_verification` | First admission of a verification figure. Same shape as Example 2. |
| `grant_standing(claim_id, purpose, authority, grant_id)` | Generic over purposes. Rejected if the (claim, purpose) pair has been revoked (terminal) or already has active admissibility. Records the grant provenance and admits the active `AdmissibleFor`. |
| `revoke_standing(claim_id, purpose, revocation_id)` | Generic over purposes. Requires current admissibility, retracts it, records the revocation. The underlying claim is not touched. |
| `admit_debt_service_revenue` | Decision transformation for the bank-debt-service purpose. Requires a matching `IndependentlyVerifiedRevenue` *and* active `AdmissibleFor(verification_id, bank_debt_service)`. |
| `admit_investor_reported_revenue` | Decision transformation for the investor-reporting purpose. Same shape; investor standing required. |

The decision transformations embed their target purpose (`bank_debt_service`, `investor_reporting`) as a literal subject in the IR - this is what motivated adding `Value::Subject` to the IR in this PR. Before that addition, the alternative would have been to pass the purpose in as a parameter the caller already knows.

## How to run it

```bash
cargo test -p morpholog-core -- standing revocation cannot_admit
```

The filter words (`standing`, `revocation`, `cannot_admit`) each match one or more of the tests below; the substring filters are forwarded to the libtest binary after `--`, and cargo's positional `TESTNAME` argument is left unset.

In-memory tests:

1. **`standing_is_purpose_specific`** - Bank standing only; debt-service decision accepted, investor report rejected. Proves the basic admissibility-for-purpose split.
2. **`parallel_standings_permit_corresponding_decisions`** - Both bank and investor standings granted on the same verification; both decision types accepted. Proves that multiple parallel `AdmissibleFor` claims can attach to the same underlying claim.
3. **`revocation_blocks_new_decisions_but_preserves_history`** - The load-bearing test. After bank standing is granted and a decision admitted, the standing is revoked. The historical decision survives. The underlying verification is unchanged. The grant provenance and the revocation are both in admitted state. A new decision against the same verification is rejected.
4. **`wrong_amount_rejected_even_with_valid_standing`** - Verified amount is 91; decision claiming 92 against the same verification id is rejected by the IV-match `require`. Standing alone is not enough; the cited number must match the verification.
5. **`cannot_admit_decision_without_iv`** - Standing granted on a verification id that has no underlying `IndependentlyVerifiedRevenue`. The grant succeeds (standing is its own claim); the decision fails when it tries to find the matching IV.

The durable PostgreSQL counterparts in `crates/morpholog-postgres/tests/integration.rs`:

- `claim_standing_full_chain_through_pg` - the full standing-acquired-then-revoked chain via `propose_against_pg`. Verifies the claims, audit rows, and outbox intents land in the expected causal order.
- `decision_after_revocation_rejects_and_writes_nothing` - durable counterpart of the revocation test. The rejected decision leaves all three tables untouched.

---

## Design notes

### Currentness vs standing

The single sentence that distinguishes Example 2 from Example 3:

| Example 2 (currentness) asks | Example 3 (standing) asks |
| --- | --- |
| *"Which claim is in force now?"* | *"Which claim may be relied on for **this purpose**?"* |

Currentness is a yes/no question about a single point of time. Standing is a question parameterised over purpose. The same claim can be in force *and* not admissible for a given purpose; can be admissible for one purpose and not another; can be admissible now and not at some prior time, or vice versa.

### `require` vs invariant for decision gating

This was the load-bearing design decision in this example. Three options:

1. **`require` for admission gate, invariants govern standing claims only.** Chosen here. Decisions check standing at admission time; once admitted they are locked in; revocation does not invalidate them.
2. *Cascading retraction.* Invariant ties decisions to standing; revoke must also retract every dependent decision. Historical decisions are not preserved. Rejected: contradicts the design goal that revocation be lossless on history.
3. *Decision-standing snapshot.* Decision claim records the specific `grant_id` it relied on; invariant says decision implies snapshot pins to a real `StandingGrantedBy` in history. More complex; would prove a deeper claims-about-claims-about-claims pattern. Deferred until a future example needs it.

### Standing is generic over what is being stood up

The `grant_standing` transformation will admit `AdmissibleFor(any_subject, any_purpose)` even if the named subject has no underlying claim in state - the test `cannot_admit_decision_without_iv` shows standing being granted on `ver_999` while no `IndependentlyVerifiedRevenue(_, _, _, ver_999)` exists. This is intentional. `AdmissibleFor(claim_id, purpose)` is a generic standing relation: the same shape applies to verifications, journal entries, curve snapshots, audit artefacts, valuation reports, and other claim kinds we may add later. The runtime has no way today to know which predicate a given subject is meant to identify, so it cannot enforce "this subject names a real verification" at standing-grant time.

The responsibility is pushed one layer down: each decision transformation requires the *specific underlying claim shape* it relies on (here, `IndependentlyVerifiedRevenue(asset, period, amount, verification_id)`). A decision against a stood-up-but-non-existent verification fails at admission, not at the standing grant.

A future typed-predicate or claim-identity affordance - declaring that a predicate's *n*th argument is a subject identifying a specific claim kind - would let `grant_standing` reject "standing on a verification id that names nothing" at grant time, not at decision time. Until then, generic standing is the honest position.

### Three things deliberately not in this example

1. **Re-granting after revocation.** The `grant_standing` transformation has `require not StandingRevoked(...)`. Once revoked, that (claim, purpose) pair is terminal. The real world sometimes wants the ability to re-grant after the reason for revocation has been addressed. Modelling this cleanly likely requires a `RevocationLifted` claim or a grant-supersession pattern. Deferred until a real example forces it.

2. **Cross-purpose constraints.** No invariant says "anything admissible for investor reporting is also admissible for owner dashboard." Such cross-purpose implications are reasonable in some domains and would be expressed as additional invariants. Deferred.

3. **Time-bounded standing.** Real grants typically expire ("admissible for 12 months from the verification date"). This needs *temporal qualification* - claims about effective windows - and probably the as-of operator named in [`docs/scope-and-ambition.md`](../../docs/scope-and-ambition.md). Deferred until the project has an example that genuinely forces it.

### What this example proves about the doctrine

The expansion principle from `docs/scope-and-ambition.md` says:

> Whatever you want to make legitimate, name it as a predicate and admit it as a claim. Whatever rules must hold, write as an invariant. Everything else lives outside.

Standing is a perfect test of that principle. It looks at first like it might want to be metadata on the verification, or a status field, or a permissions subsystem. It is none of those. It is *more claims* - `AdmissibleFor`, `StandingGrantedBy`, `StandingRevoked` - governed by ordinary invariants. The verification itself stays untouched.

The corollary: when a future concern looks like it needs a new subsystem, the first move is to see whether it can be expressed as claims about claims, with invariants in the existing kernel. Most of the time, it can.
