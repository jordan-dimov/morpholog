# Verified Revenue

A battery-storage asset earns monthly revenue. Several parties care about that number for different reasons - the asset owner, the bank that financed the system, investor relations, sometimes a regulator. The figure is the same; what differs is *what each party is allowed to do with it*, and *what happens when the verifier later corrects it*.

In a conventional system, the answers come from detective work. Pull the verifier's email. Compare it to the bank's spreadsheet. Cross-reference the investor deck. Sometimes they tie out; sometimes they don't.

Morpholog makes them admitted claims. The verifier's figure, each party's standing to rely on it for *their* decision, each decision admitted under that standing, the lineage when the figure is corrected - all admitted claims. Three quarters from now, *what did we recognise as revenue for Q1, who verified it, who said it was admissible for which purpose, what later corrected it, and what decisions did we make while it was valid?* is one query against the audit log.

## The scenario

Two patterns weave through one programme.

**Currentness with restatement.** A verifier signs off on Q1 revenue at £92,000. Weeks later, after metering reconciliation, they correct it to £91,000. The original figure stays in the books; a singleton `CurrentVerification` pointer moves to the corrected figure; a `Supersedes` claim records the lineage.

**Admissibility-for-purpose.** The bank's credit committee grants *standing* for the verification to be relied on for debt-service-coverage covenant tests. Investor relations separately grants standing for the same verification for shareholder reporting. Two parallel admissibilities on the same figure, two different authorities. Either can be revoked without touching the other.

**Where they meet.** When the verifier corrects the figure, every standing granted on the prior verification is retracted by pattern - the authorities must re-issue standing on the corrected figure if they accept it. **Every historical decision admitted under the prior standing survives.** A covenant test computed on June 30 against the then-valid figure stays a valid record of what the bank decided that day, even after the verifier corrects the figure on July 15.

After correction, the prior verification remains queryable as history but cannot receive new standing - `grant_standing` requires a *currently in force* verification. The whole doctrine: **correction does not erase the old figure, does not erase old decisions, does remove future standing from the old figure, and requires future reliance to attach to the current figure.**

## The program

See [`verified_revenue.morph`](verified_revenue.morph) for the illustrative surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `IndependentlyVerifiedRevenue(asset, period, amount, verification_id)` | The verifier's figure. Append-only; corrections add new claims, never mutate originals. |
| `CurrentVerification(asset, period, verification_id)` | Singleton retractable pointer to the verification currently in force. Moves on correction. |
| `Supersedes(new_verification_id, prior_verification_id)` | Restatement lineage. Append-only. |
| `AdmissibleFor(verification_id, purpose)` | Active admissibility for a specific purpose. Retractable. Multiple per verification (parallel admissibilities coexist). |
| `StandingGrantedBy(verification_id, purpose, authority, grant_id)` | Grant provenance. Append-only - survives revocation as the record of who granted what. |
| `StandingRevoked(verification_id, purpose, revocation_id)` | Revocation provenance. Append-only and **terminal** - revocation cannot be undone. |
| `DebtServiceRevenue(asset, period, amount, decision_id, verification_id)` | A bank-debt-service decision relying on a specific verification. Append-only. |
| `InvestorReportedRevenue(asset, period, amount, report_id, verification_id)` | An investor-reporting decision relying on a specific verification. Append-only. |

Content claims (figures, grants, revocations, decisions) are append-only. Pointer claims (`CurrentVerification`, `AdmissibleFor`) are retractable. Lineage (`Supersedes`) is append-only.

### Invariants

| Invariant | Says |
| --- | --- |
| `admissibility_has_provenance` | Every active `AdmissibleFor(v, p)` must be backed by some `StandingGrantedBy(v, p, _, _)`. |
| `admissibility_excludes_revocation` | `AdmissibleFor(v, p)` cannot coexist with any `StandingRevoked(v, p, _)`. |
| `at_most_one_current_verification_per_asset_period` | The singleton pointer property. |
| `at_most_one_direct_successor` | A verification is superseded by at most one direct successor; parallel chains are forbidden. |

**No invariant ties decision claims to live `AdmissibleFor`.** Decisions are gated at admission via `require`; once admitted, locked in. An invariant tying them to live standing would force either rejecting revocation (because historical decisions would break the rule) or cascade-retracting those decisions (which destroys the record). Neither matches the business. This is the require-vs-invariant lesson running through every example in the project.

### Transformations

| Transformation | Effect |
| --- | --- |
| `admit_independent_verification(asset, period, amount, verification_id)` | First admission for an (asset, period). Asserts the IV claim and the `CurrentVerification` pointer. Rejected if a current verification already exists. |
| `correct_independent_verification(asset, period, new_amount, new_verification_id, prior_verification_id)` | The combined-doctrine transformation. Asserts new IV + `Supersedes`; retracts the prior pointer AND every `AdmissibleFor` on the prior verification (pattern retraction); asserts the new pointer. |
| `grant_standing(verification_id, purpose, authority, grant_id)` | Grants `purpose` standing. Rejected if (a) no IV references the supplied id, (b) the verification is not currently in force, (c) the (verification, purpose) pair has been revoked, or (d) it already has active admissibility. |
| `revoke_standing(verification_id, purpose, revocation_id)` | Requires currently-active standing; retracts `AdmissibleFor`; asserts terminal `StandingRevoked`. |
| `admit_debt_service_revenue(asset, period, amount, decision_id, verification_id)` | Requires a matching IV AND `AdmissibleFor(verification_id, bank_debt_service)`. The purpose is embedded as a literal in the require. |
| `admit_investor_reported_revenue(asset, period, amount, report_id, verification_id)` | Same shape with `investor_reporting`. |

## How to run it

The same scenario is proven at two layers.

```bash
# In-memory (sync kernel)
cargo test -p morpholog-core --test verified_revenue

# Durable (PostgreSQL adapter)
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    verified_revenue_full_chain_through_pg
```

In-memory tests cover restatement, standing, and the combined load-bearing test where a correction retracts standings on the prior verification while historical decisions survive. The PG integration test walks the whole story end to end through `propose_against_pg`.

---

## Design notes

### Pattern-based retraction in correction

`correct_independent_verification` includes `retract AdmissibleFor(prior_verification_id, _)`. The `_` is `Term::Wildcard`; the runtime iterates pre-state for every `AdmissibleFor` claim whose first argument matches the prior verification, and retracts each one. Zero or many - the transformation does the right thing in all cases. This is the cleanest demonstration of wildcard retraction in the project.

### Why decisions are not tied to live standing by an invariant

The natural rule "every `DebtServiceRevenue` claim implies an active `AdmissibleFor`" is an invariant trap. As an invariant, revoking standing later would either reject the revoke (breaking the rule that standing can be lost) or cascade-retract every historical decision that relied on it (breaking the rule that history is preserved). The legitimacy of a decision made under valid standing at time T stays valid even if standing is revoked at T + 1.

`require` is the *admission gate* - checked at admission, never again. `invariant` is the *eternal rule* - must always hold against admitted state. Different questions.

### What this example deliberately does not cover

- **Authority over the verifier itself.** `admit_independent_verification` is ungated. A real system would gate it on `MayVerify(actor, asset)` using the actor-authority pattern from approval controls.
- **Re-grant after revocation.** `StandingRevoked` is terminal in v0; a `RevocationLifted` shape would unlock re-granting.
- **Effective time as a separate axis.** Real verifications have an effective period (the figure is *for* Q1) distinct from when the runtime admitted them. Effective time as a first-class axis combined with as-of replay would give full bitemporal addressability.
- **Multi-party revenue claims.** A real BESS stack has parallel revenue claims (optimiser dispatch log, bank recognition, owner expectation). Modelling several `*ReportedRevenue` predicates with their own currentness pointers would scale the pattern; v0 uses one verification per (asset, period) and lets the standing layer carry the multi-party story.
