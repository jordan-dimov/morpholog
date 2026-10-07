# Verified revenue

**A figure is approved for a particular use, relied on, then corrected - and
every decision made along the way stays defensible.** A correction keeps
the original figure, withdraws approval from it, and leaves the decisions
made while it was approved standing as a true record of what was decided.

## Why this matters

A battery-storage asset earns monthly revenue. Several parties care about
that number for different reasons: the asset owner, the bank that financed
it, investor relations, sometimes a regulator. The figure is the same; what
differs is *what each party may do with it*, and *what happens when the
verifier later corrects it*.

Usually the answers come from detective work: the verifier's email, the
bank's spreadsheet, the investor deck, and hope that they tie out. Here the
verifier's figure, each party's approval to rely on it for their purpose,
each decision made under that approval, and the link from a correction to
what it corrected are all records. The question that would start an
investigation - *what did we recognise for Q1, who verified it, who approved
it for which use, what corrected it, and what did we decide while it held?*
- is a query.

## What Morpholog enforces

- **Approval for a purpose.** The bank's covenant test may rely only on a
  verification approved for debt-service use; investor reporting only on
  one approved for investor reporting. Morpholog calls such an approval
  *standing*. The two approvals on one figure are independent.
- **Approval only on the figure in force.** A corrected figure cannot
  receive new approval, and a revoked approval cannot be granted again.
- **Correction withdraws approval.** Correcting the figure withdraws every
  approval of the old one; whoever relies on the new figure must approve it
  afresh.
- **Decisions stay.** A decision made under a valid approval stays on the
  record after the approval is withdrawn or the figure is corrected.

## What it refuses

- A covenant test on a figure with no debt-service approval, including one
  approved only for investor reporting.
- An approval granted to a figure that has since been corrected.
- An approval granted again after it was revoked.
- A second "first" verification for an asset and period that already has
  one in force.

## What you can show afterwards

A verifier signs off Q1 revenue at £92,000; the bank approves it for its
covenant test and runs the test on 30 June. On 15 July the verifier
corrects the figure to £91,000. The record still shows the original
figure, the corrected one and the link between them; which figure each
decision relied on; who approved each figure for which use, and when it was
withdrawn; and that the 30 June test was made under a valid approval.

## Where it fits

The verifier, the bank and investor relations keep their own systems. Each
step - a verification, a correction, an approval, a decision that relies on
the figure - is proposed through Morpholog, and each accepted step sends a
notification (`VerificationCorrected`, `StandingGranted`,
`DebtServiceRevenueAdmitted` and the rest) to the systems that need it.

## The program

See [`verified_revenue.morph`](verified_revenue.morph) for the surface syntax.

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
| `current_verification_unique_by_asset_period` (from `current pointer by (asset, period)`) | The singleton pointer property. |
| `supersedes_unique_by_prior_verification_id` (from `superseded via Supersedes`) | A verification is superseded by at most one direct successor; parallel chains are forbidden. |

**No invariant ties a decision to live approval.** A decision is checked against its approval when it is made (a `require`), and stays a valid record afterwards. As an invariant, revoking approval later would have to be refused, or would have to erase the decisions made under it. [`verified_revenue.morph`](verified_revenue.morph) walks through why.

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
# In-memory
cargo test -p morpholog-examples --test verified_revenue

# Durable (PostgreSQL adapter)
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test integration -- --test-threads=1 \
    verified_revenue_full_chain_through_pg
```

In-memory tests cover restatement, standing, and the combined load-bearing test where a correction retracts standings on the prior verification while historical decisions survive. The PG integration test walks the whole story end to end through `propose_against_pg`.

## What this example deliberately does not cover

- **Who may verify or approve.** `admit_independent_verification` is not
  gated, and `grant_standing` records the approving authority as a value it
  is given. A real system would gate both on the actor's own authority,
  the pattern [approval controls](../04_approval_controls/) shows.
- **Approval again after revocation.** A revocation here is final.
- **Effective dates.** A real verification is *for* a period, distinct from
  when it was recorded. [Trade lifecycle](../10_trade_lifecycle/) shows
  effective-dated records; this example does not use them.
- **Several revenue figures per asset.** A real battery stack has parallel
  figures (the optimiser's dispatch log, the bank's recognition, the
  owner's expectation). This example keeps one verification per asset and
  period and lets the approvals carry the several parties.
