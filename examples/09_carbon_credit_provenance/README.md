# Carbon-Credit Provenance

**No green claim should become official without admissible provenance.**

A carbon credit is not valuable because of a calculation alone. It is valuable because a registry, buyer, auditor, or regulator can later justify its standing: who verified the measurement, who attested it, who was accredited at the time, who held the credit, and whether it has already been retired. A green claim without admissible provenance is not a weak green claim. It is not a green claim at all.

## The scenario

A credit that cannot be justified after the fact is commercially toxic: it exposes the holder to clawback, the registry to reputational collapse, and the buyer to a worthless offset. The failure is never a slightly-wrong number. It is a credit that was *allowed to become official* without the evidence chain that legitimises it - issued against an unverified measurement, attested by no one, or backed by a verifier whose accreditation had lapsed.

So the questions a controller asks are diagnostic, not arithmetic: why was this credit allowed to exist, who attested its measurement, was that verifier accredited at the time, who holds it now, and has it already been retired? This example makes the wrong answers uncommittable, and when an issuance is refused it names the exact missing link.

## What Morpholog governs - and what it does not

Morpholog governs the **admission of standing claims**: that a measurement is verified, that it was attested, that a credit is issued, held, and eventually retired.

It does **not** govern the measurement. The MRV maths - generation x emission factor -> tonnes - is the meter. It stays outside Morpholog and returns as an admitted `VerifiedMeasurement` claim. Admitting that claim records "this measurement was verified to this amount", never "this is the truth". `Attestation(measurement, verifier)` means the verifier attests the admitted measurement claim as a whole, including its quantity.

## The program

See [`carbon_credit_provenance.morph`](carbon_credit_provenance.morph) for the illustrative surface syntax and a guided tour of the domain.

### Claims

| Predicate | Role |
| --- | --- |
| `Accredited(verifier)` | the verifier is *currently* authorised to attest |
| `VerifiedMeasurement(measurement, quantity)` | the MRV result, admitted as evidence |
| `Attestation(measurement, verifier)` | the verifier attests the admitted measurement |
| `Issued(credit, measurement, quantity)` | the credit's official standing, backed by one measurement |
| `HeldBy(credit, account)` | current custody |
| `Retired(credit, account)` | the credit has been cancelled - terminal |
| `Obligation(obligation, account, quantity, due_on)` | the account must retire `quantity` tonnes by `due_on` |
| `ObligationSatisfied(obligation)` / `ObligationBreached(obligation)` | the obligation's outcome |

### Invariants - what this model makes impossible

| Invariant | The bad state it forbids |
| --- | --- |
| `at_most_one_verified_quantity_per_measurement` | a measurement verified to two conflicting amounts |
| `no_double_issuance` | two distinct credits backed by the same measurement |
| `credit_backed_by_one_measurement` | one credit backed by two different measurements |
| `single_custody` | one credit held by two accounts at once |
| `retirement_terminal` | a credit that is both retired and still held |
| `at_most_one_obligation_per_id` | one obligation id with two different sets of terms |
| `obligation_not_both_satisfied_and_breached` | an obligation that is both satisfied and breached |

### Transformations

`grant_accreditation` / `revoke_accreditation`, `verify_measurement`, `attest_measurement`, `issue_credit`, `transfer_credit`, `retire_credit`, plus `raise_obligation`, `discharge_obligation`, and `sweep_obligation`. The legitimacy gate lives in `issue_credit`: it binds the quantity from a verified measurement and requires both an attestation and a *currently* accredited verifier before any credit becomes official.

## What `explain` shows

The reason this example exists. Attempting to issue a credit whose measurement was never attested is refused, and the explanation names the missing claim and the transformation that would supply it:

```
Gate not satisfied:
  Attestation(measurement, verifier) and Accredited(verifier)

Directly missing claims:
  - Attestation(m1, acme_verifier)
      candidate supplier transformations:
        - attest_measurement
```

The same engine distinguishes a missing `VerifiedMeasurement`, a missing `Attestation`, and a missing `Accredited` - each with its own supplier - while a double-issuance is reported as an invariant violation, and an attempt to transfer or re-retire a retired credit is a faithful gate rejection with nothing missing (the credit is blocked, not lacking evidence).

## Obligations over time

A compliance scheme can oblige an account to retire enough credits by a deadline: `raise_obligation(obligation, account, quantity, due_on)`. Retirement discharges it - `discharge_obligation(obligation, current_date)` admits `ObligationSatisfied(obligation)` when, *on or before the deadline*, the account's retired total (summed across the credits it has retired) reaches the target. Discharge is date-aware too: a late retirement cannot quietly satisfy a "by `due_on`" obligation.

Morpholog keeps no clock. A deadline is about *now*, and "now" is known only outside the system - so neither discharge nor breach invents it: both take `current_date` from the outside scheduler. A breach is recorded when `sweep_obligation(obligation, current_date)` finds an obligation past the due date, not already decided, and still under target. This is the "Morpholog plus an Outside Coordinator" pattern: the kernel decides admissibility; the coordinator supplies the passage of time. The `obligation_not_both_satisfied_and_breached` invariant guarantees the two outcomes can never coexist, in whatever order things happen.

## How to run it

```bash
# Validate the surface source.
morpholog check examples/09_carbon_credit_provenance/carbon_credit_provenance.morph

# Exercise the model and the explanations (in-memory, no database).
cargo test -p morpholog-examples --test carbon_credit_provenance
```

## Design notes

### What this example proves about the doctrine

Provenance is modelled as ordinary **claims about claims** - `VerifiedMeasurement`, `Attestation`, `Accredited` - and that was enough. No kernel primitive was added; the existing claim model carried the entire evidence chain. The commercial pitch ("no green claim without admissible provenance") and the explanation engine turn out to be one bet: the runtime that refuses an illegitimate issuance is the same runtime that can say precisely why.

**Currentness without rewriting history.** Revoking a verifier's accreditation blocks *new* issuance through them from that moment on, but leaves credits already issued untouched. `issue_credit` requires `Accredited(verifier)` at proposal time (a gate); the `Issued` claims it produced earlier are admitted state and stay admitted. A gate governs what you may do next; it never reaches back to invalidate what was legitimately admitted.

### What this example deliberately does not cover

This example models one credit as backed by one measurement. Real registries may split a project period into many units or batches. We keep one-credit-per-measurement here so the legitimacy mechanics stay visible: `no_double_issuance` refuses two credits backed by the same measurement, and `credit_backed_by_one_measurement` refuses one credit backed by two - together pinning the credit-to-measurement correspondence one to one, so double-counting is impossible in either direction. Conservation across a batch (total issued against a measured quantity) is a later extension, not a change to the legitimacy story.

`morpholog inspect guarantees carbon_credit_provenance` lists what this model forbids - one entry per invariant, naming the forbidden state for the `not(...)` rules (retirement-terminal, the obligation outcomes). It is the static companion to `explain`: the model declares what is impossible, and `explain` says why a specific transition was refused.
