# Verified Revenue

A battery-storage asset earns monthly revenue. Several parties care about that number for different reasons - the asset owner, the bank that financed the storage system, investor relations writing the next earnings report, sometimes a regulator looking at grid contribution. The figure they care about is the same figure; what differs is *what each party is allowed to do with it*, and *what happens when the verifier later corrects it*.

In a conventional system the answers come from detective work. Pull the verifier's email. Compare it to the bank's spreadsheet. Cross-reference the investor deck. Ask the analyst who happened to do the close that quarter. Sometimes the answers tie out, and sometimes they don't, and when they don't, it makes the news.

Morpholog answers them differently. The verifier's figure is an admitted claim. Each party's standing to rely on it for *their specific decision* is an admitted claim. Each decision they make under that standing is an admitted claim. The lineage when the verifier corrects the figure is an admitted claim. Three quarters from now, the question *what did we recognise as revenue for Q1, who verified it, who said it was admissible for which purpose, what later corrected it, and what decisions did we make while it was valid?* is one query against the audit log - with a definite answer that the runtime guaranteed at the moment the claim landed.

## The scenario

Two patterns weave through one programme. Each handles a different shape of contested legitimacy a real business runs into.

**Currentness with restatement.** An independent verifier signs off on the asset's Q1 revenue at £92,000. Weeks later, after metering reconciliation, they correct it to £91,000. The original figure does not disappear from the books - three quarters from now, an auditor asking "what was the original number?" gets a real answer. A singleton pointer (`CurrentVerification`) moves to the corrected figure; the historical figure remains in admitted state; a `Supersedes` claim records the lineage.

**Admissibility-for-purpose.** The bank's credit committee grants *standing* for the verification to be relied on for debt-service-coverage covenant tests. Investor relations, separately and under different rules, grants standing for the same verification to be relied on for shareholder reporting. The same number, two parallel admissibilities, two different authorities. Either can be revoked without touching the other; neither revocation touches the underlying verification.

**Where they meet.** When the verifier corrects the figure, every standing granted on the prior verification is retracted by pattern - the authorities must re-issue standing if they accept the correction. **But every historical decision admitted under the prior standing survives in admitted state.** A debt-service-coverage covenant test computed on June 30 against the then-valid figure stays a valid record of what the bank decided that day, even after the verifier corrects the figure on July 15. The legitimacy of a *past* decision was established when it was made; revocation prevents *future* decisions, not past ones.

After correction, the prior verification remains queryable as history but cannot receive new standing - the runtime requires `grant_standing` to attach to a verification that is *currently* in force. Future reliance must attach to the current figure. The whole doctrine in one sentence: **correction does not erase the old figure, does not erase old decisions, does remove future standing from the old figure, and requires future reliance to be re-granted on the current figure.**

## The program

See [`verified_revenue.morph`](verified_revenue.morph) for the (illustrative) surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `IndependentlyVerifiedRevenue(asset, period, amount, verification_id)` | The verifier's figure. **Append-only.** Corrections add a new claim with a new `verification_id`; the original is never mutated. |
| `CurrentVerification(asset, period, verification_id)` | Singleton retractable pointer to the verification currently in force for an (asset, period). Moves on correction. |
| `Supersedes(new_verification_id, prior_verification_id)` | Restatement lineage. **Append-only.** |
| `AdmissibleFor(verification_id, purpose)` | Active admissibility of a verification for a specific purpose. **Retractable.** Cardinal - multiple parallel admissibilities for the same verification coexist. |
| `StandingGrantedBy(verification_id, purpose, authority, grant_id)` | Provenance of an admissibility grant. **Append-only.** Survives revocation as the historical record of who granted what. |
| `StandingRevoked(verification_id, purpose, revocation_id)` | Provenance of a revocation. **Append-only and terminal in v0** - revocation cannot be undone. |
| `DebtServiceRevenue(asset, period, amount, decision_id, verification_id)` | A decision that relied on a specific verification for the bank-debt-service purpose. **Append-only.** |
| `InvestorReportedRevenue(asset, period, amount, report_id, verification_id)` | A decision that relied on a specific verification for investor reporting. **Append-only.** |

The append-only / retractable split is total. Content claims (verification figures, grants, revocations, decisions) are append-only. Pointer claims (CurrentVerification, AdmissibleFor) are retractable. Lineage claims (Supersedes) are append-only.

### Invariants

| Invariant | Says |
| --- | --- |
| `admissibility_has_provenance` | Every active `AdmissibleFor(v, p)` must be backed by some `StandingGrantedBy(v, p, _, _)`. No admissibility without recorded provenance. |
| `admissibility_excludes_revocation` | `AdmissibleFor(v, p)` cannot coexist with any `StandingRevoked(v, p, _)`. Revocation retracts admissibility by construction; this catches accidental inconsistency. |
| `at_most_one_current_verification_per_asset_period` | The singleton pointer property. At most one `CurrentVerification` claim per `(asset, period)`. |
| `at_most_one_direct_successor` | A verification is superseded by at most one direct successor. Parallel restatement chains are forbidden by construction. |

**What is deliberately not an invariant:** nothing ties decision claims (`DebtServiceRevenue`, `InvestorReportedRevenue`) to live `AdmissibleFor`. Decisions are gated at admission time via `require`; once admitted, they are locked in. If a decision were tied to live standing via an invariant, revoking standing would force the runtime to either reject the revocation (because historical decisions now break the rule) or cascade-retract those decisions (which destroys the record). Neither matches the business - the require-vs-invariant lesson that runs through every example in this project.

### Transformations

| Transformation | Effect |
| --- | --- |
| `admit_independent_verification(asset, period, amount, verification_id)` | First admission for an (asset, period). Asserts both the IV claim and the `CurrentVerification` pointer. Rejected if a current verification already exists - use `correct_independent_verification` to replace. |
| `correct_independent_verification(asset, period, new_amount, new_verification_id, prior_verification_id)` | The combined-doctrine transformation. Asserts the new IV and `Supersedes` lineage; retracts the prior `CurrentVerification` pointer AND every `AdmissibleFor` on the prior verification (pattern-based retraction); asserts the new pointer. Historical decisions survive. |
| `grant_standing(verification_id, purpose, authority, grant_id)` | Grants `purpose` standing on the verification, recorded with authority and grant provenance. Rejected if (a) no `IndependentlyVerifiedRevenue` claim references the supplied `verification_id` (phantom id), (b) the verification is not currently in force (superseded by correction), (c) the (verification, purpose) pair has been revoked (terminal), or (d) the pair already has active admissibility. |
| `revoke_standing(verification_id, purpose, revocation_id)` | Requires the standing to be currently active; retracts `AdmissibleFor`; asserts `StandingRevoked` (terminal). The historical `StandingGrantedBy` survives. |
| `admit_debt_service_revenue(asset, period, amount, decision_id, verification_id)` | Requires a matching IV claim AND `AdmissibleFor(verification_id, bank_debt_service)`. The purpose is embedded as a literal in the require, so the transformation is intrinsically tied to its purpose. |
| `admit_investor_reported_revenue(asset, period, amount, report_id, verification_id)` | Same shape, but the embedded purpose is `investor_reporting` and the asserted predicate is `InvestorReportedRevenue`. |

## How to run it

The same scenario is proven at two layers - in-memory through the sync kernel, and durably through the PostgreSQL adapter.

### In-memory (sync kernel)

```bash
cargo test -p morpholog-core --test verified_revenue
```

In-memory tests cover both patterns and the combined doctrine:

- **Restatement**: admission with current pointer; correction preserves history, moves pointer, records lineage; parallel restatement chains are forbidden; a second admission against an existing current verification is rejected.
- **Standing**: parallel standings coexist; decisions admit only with matching standing; investor standing does not satisfy a bank-decision require; revoked standing blocks future decisions but preserves past ones; revocation is terminal (no re-grant after revoke).
- **The combined load-bearing test**: a verifier corrects a figure that has multiple active standings and a historical decision. Both standings are retracted by pattern; the historical decision and the historical grant-provenance survive; new decisions on the corrected figure require the bank to re-grant.

### Durable (PostgreSQL adapter)

```bash
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    verified_revenue_full_chain_through_pg
```

The PG integration test walks the whole story end to end through `propose_against_pg`: admission, multi-authority standing, decisions, correction (which retracts standings on the prior verification), rejection of decisions without standing (with row-count negative pin), and re-grant on the corrected figure.

---

## Design notes

### The pattern-based retraction in correction

`correct_independent_verification` includes:

```
retract AdmissibleFor(prior_verification_id, _)
```

The `_` is `Term::Wildcard`. Pattern-based retraction iterates the pre-state for all `AdmissibleFor` claims whose first argument matches the prior verification, and retracts each one. There may be zero (if no one granted standing) or many (every authority that did). The transformation does the right thing in all cases.

This is the cleanest place the project demonstrates wildcard retraction. The doctrine: when an upstream fact is corrected, every retractable claim that depended on it gets cleared in one move - and the authorities must re-affirm if they still agree with the corrected figure.

### Why decisions are not tied to live standing by an invariant

The natural-sounding rule "every `DebtServiceRevenue` claim implies an active `AdmissibleFor`" turned out to be an invariant trap. Encoded as an invariant, revoking standing later would either:

- reject the revoke (breaking the rule that standing can be lost), or
- force cascade-retraction of every historical decision that relied on it (breaking the rule that history is preserved).

Neither matches the real-world semantics. The legitimacy of a decision made under valid standing at time T stays valid even if standing is revoked at T + 1.

So `require` is the *admission gate* - checked at the moment of admission, and only then. `invariant` is the *eternal rule* - must always hold against admitted state. The two answer different questions; they are not interchangeable.

### What this example deliberately does not cover

1. **Authority over the verifier itself.** Who may *admit* a verification? In v0 the `admit_independent_verification` transformation is ungated; a real system would gate it on an authority claim about the proposing actor (a `MayVerify(actor, asset)` shape) using the actor-authority pattern from Approval Controls. A worked example combining standing with proposing-actor authority would force the next layer.
2. **Re-grant after revocation.** `StandingRevoked` is terminal in v0. A real system might allow re-granting after a clearance event; the `RevocationLifted` shape and the lifecycle that goes with it are deferred until a real example forces them.
3. **Effective time as a separate axis.** The example uses transition order ("first admit, later correct"). A real verification has an *effective period* (the figure is for Q1) that may be distinct from when it was admitted and when it became current. Effective time as a first-class temporal axis combines with as-of replay to give full bitemporal addressability; out of scope here.
4. **Optimiser-reported revenue and three-party reconciliation.** A real BESS revenue stack has multiple parties producing revenue claims (the optimiser's dispatch log, the bank's recognition, the owner's expectation). Modelling several parallel `*ReportedRevenue` claims with their own currentness pointers would scale the pattern; the v0 example uses one verification per (asset, period) and lets the standing layer carry the multi-party legitimacy story.
