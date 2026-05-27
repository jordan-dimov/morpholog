# KYC sanctions and PEP screening

A bank cannot open an account for a new customer until it has checked them against sanctions and PEP lists. Those checks are not a single event - they are a continuous obligation. A clean result has an expiry; a list update can flag a previously-clean customer; a flagged customer cannot be admitted until an analyst adjudicates the match. The legal weight is real: a missed alert is a regulatory breach, and a missed event in the outbox - a mistyped `emit MatchRased(...)` routed to a partition no analyst queue reads, so the alert that should reach a human silently never fires - is the same breach with no audit trail.

This example is the smallest Morpholog programme that holds those constraints honestly. The central rule is:

> No customer may hold *onboarded* standing without a current screening per list-type whose disposition is *clean* (or adjudicated-clear) and whose expiry has not been reached by the onboarding date - and no unresolved match on any of their screenings, current or not.

That single sentence becomes a handful of invariants - structural uniqueness of the currentness pointer, a current-clean screening per list-type, and a no-unresolved-match gate. Onboarding the wrong customer is structurally impossible.

## The scenario

A bank registers a new customer, `cust_alice`, on **2026-05-01**, and requests two screenings - one against the **sanctions** list, one against **PEP** - each emitting a request to the external provider.

The sanctions result returns **clean** on 2026-05-02, valid through 2026-11-02, and becomes her current sanctions screening. The PEP result comes back a possible **match**: it is recorded, flagged `MatchUnderReview`, and routed to the analyst queue - and pointedly does **not** become her current PEP screening. Onboarding now is refused: the no-unresolved-match invariant fails.

An analyst reviews the PEP match, judges it a false positive (a name collision), and adjudicates it clear on 2026-05-03; the flag is cleared and the adjudicated result becomes her current PEP screening. Now onboarding `cust_alice` on **2026-05-04** admits - both lists carry a current, in-date, clean-or-cleared result, and no match is outstanding.

Three years later an auditor can ask: was she screened against both lists, were the results current and clean on the day she was onboarded, and was every match resolved before the account opened? Each is a claim in the log; none could have been skipped, because the invariants gate the onboarding itself.

## The program

See [`kyc.morph`](kyc.morph) for the surface syntax.

### Claims

| Predicate | Role |
| --- | --- |
| `Customer(customer_id)` | A registered customer. Append-only. |
| `Screening(screening_id, customer, list_type, requested_on)` | A screening request against a named list (`#sanctions`, `#pep`). Append-only. |
| `ScreeningResult(screening_id, disposition, completed_on, expires_on)` | The provider's result - disposition `#clean`, `#match`, `#adjudicated_clear`, or `#adjudicated_confirmed` - with an inclusive expiry. Append-only. |
| `CurrentScreening(customer, list_type, screening_id)` | Singleton retractable pointer to the screening that counts now for a `(customer, list_type)`. Moves as fresh clean / cleared results arrive; a match never becomes current. |
| `MatchUnderReview(screening_id, raised_on)` | A possible match awaiting analyst adjudication. Retractable - cleared when adjudicated. |
| `MatchAdjudicated(screening_id)` | A match that an analyst has reviewed and resolved (either way). Append-only - the durable marker the exactly-one-outcome invariant keys off. |
| `OnboardedCustomer(customer, onboarded_on)` | Onboarded standing. Append-only. |

### Intents

Each outbox intent routes to a distinct downstream consumer, declared by name so a misspelled `emit` is a validation error rather than a silent route-to-nowhere: `ScreeningRequested` (external provider), `MatchRaised` (analyst queue), `MatchConfirmed` (compliance, on a confirmed hit), `CustomerOnboarded` (core banking), `CustomerRejected` (compliance reporting).

### Invariants

| Invariant | Says |
| --- | --- |
| `at_most_one_current_screening_per_customer_and_list_type` | The singleton currentness pointer: at most one current screening per `(customer, list_type)`. |
| `onboarded_requires_current_clean_sanctions` | An onboarded customer must have a current sanctions screening, clean or adjudicated-clear, not expired by the onboarding date. |
| `onboarded_requires_current_clean_pep` | The same for the PEP list - separate legal weight, so a clean sanctions result does not cover the PEP obligation. |
| `onboarded_requires_no_unresolved_match` | An onboarded customer has **no** `MatchUnderReview` on *any* of their screenings, current or not - a re-screen hit blocks onboarding even when an older clean result is still current. |
| `adjudicated_match_resolves_exactly_one_way` | A reviewed match carries **exactly one** outcome: `#adjudicated_clear` **xor** `#adjudicated_confirmed`. Never both, and - once marked adjudicated - never neither. This is `xor`, reading as the rule sounds rather than as `(clear or confirmed) and not (clear and confirmed)`. |
| `onboarded_requires_no_confirmed_match` | A confirmed hit is a durable bar: no onboarding while any `#adjudicated_confirmed` result is on file, even behind an older clean current screening. |

### Transformations

| Transformation | Effect |
| --- | --- |
| `register_customer(customer_id)` | Registers a customer; rejected if already on file. |
| `request_screening(screening_id, customer, list_type, requested_on)` | Records a screening request and emits `ScreeningRequested` to the provider; requires the customer to exist. |
| `record_clean_screening_result(screening_id, completed_on, expires_on)` | Records a `#clean` result and makes it the current screening, replacing the prior pointer. |
| `record_match_screening_result(screening_id, completed_on, expires_on, raised_on)` | Records a `#match`, flags `MatchUnderReview`, emits `MatchRaised`. Does **not** become current. |
| `adjudicate_match_as_false_positive(screening_id, adjudicated_on, expires_on)` | An analyst clears a flagged match: requires the match under review, records `#adjudicated_clear`, marks it adjudicated, clears the review flag, makes it current. |
| `adjudicate_match_as_confirmed(screening_id, adjudicated_on, expires_on)` | The other side of the fork: an analyst confirms a genuine hit. Records `#adjudicated_confirmed`, marks it adjudicated, clears the review flag - but does **not** become current, and the confirmed-match invariant bars onboarding for good. Emits `MatchConfirmed`. |
| `onboard_customer(customer, onboarded_on)` | Opens the account. Only checks the customer exists and is not already onboarded; the invariants do the heavy lifting. Emits `CustomerOnboarded`. |
| `reject_customer(customer, reason)` | Rejects a customer outright; nothing is removed, so the decision and its reason survive in the audit trail. Emits `CustomerRejected`. |

## How to run it

```bash
cargo test -p morpholog-examples --test kyc_sanctions_screening
```

The in-memory tests pin the load-bearing path: a clean screening becomes current; a match is flagged and does not; onboarding is refused while a match is unresolved; adjudication clears it and onboarding then admits; and the per-list / expiry / no-unresolved-match invariants each reject their own failure mode.

---

## Design notes

### What this example forces

This is the worked-example reason for `IntentDecl` to exist in the kernel. Each emitted intent - `ScreeningRequested`, `MatchRaised`, `MatchConfirmed`, `CustomerOnboarded`, `CustomerRejected` - routes to a distinct downstream consumer (provider API, analyst queue, core banking, compliance reporting). If `MatchRaised` were a stringly-typed emit and someone wrote `MatchRased`, it would silently create a new outbox partition that no analyst reviews. Declaring intents as first-class vocabulary, parallel to predicates, makes that impossible at validation time.

It is also the forcing home for the `xor` connective. The adjudication fork - a reviewed match is either a false positive or a confirmed hit - is a genuine exactly-one decision, and `adjudicated_match_resolves_exactly_one_way` states it as `clear xor confirmed` over two full `ScreeningResult(...)` patterns. `xor` adds no expressiveness (it lowers to `(a or b) and not (a and b)`), but with operands that long, the hand-written form buries the intent. The date fields are wildcards so the operands stay ground and the `xor` means true exactly-one; its distinctive bite is the totality half, rejecting an adjudicated marker that records *neither* disposition - which a plain `not (clear and confirmed)` exclusion would let through.

### The round-trip compute pattern

The example exercises the round-trip compute pattern from the three-zone doctrine (`docs/scope-and-ambition.md`):

```
request_screening -> outbox ScreeningRequested -> external provider
                                                          |
                                                          v
                                            (clean) record_clean_screening_result
                                            (match) record_match_screening_result
                                              |
                                              v
                              (false positive) adjudicate_match_as_false_positive
                              (confirmed hit)  adjudicate_match_as_confirmed
```

The bank's transactional state stays inside Morpholog. The screening call - whether the name matches anything on World-Check or Refinitiv - happens outside. The result lands as a separate transformation that admits the disposition into the commit zone, where the invariants gate onboarding.

### What this example deliberately does not cover

- **Risk-tier-dependent refresh windows.** Real KYC has 365-day standard / 90-day high-risk / event-driven refresh schedules; this example uses one window (the screening's `expires_on` field carries it directly). Tier-dependent windows would need a `CustomerRiskTier` predicate and a per-tier comparison; deferred until a worked example forces them.
- **EDD branch and senior-management approval.** Politically-exposed persons trigger Enhanced Due Diligence requiring source-of-wealth documentation and senior sign-off. The shape is `approval_controls`-flavoured (authority + limit); a future example could combine the two.
- **SAR filing.** Suspicious Activity Reports are their own audit-trail story and would need a different downstream consumer model. Out of scope here.
- **Adverse media screening.** Real onboarding screens against sanctions, PEP, AND adverse media; this example covers the first two.
- **Continuous monitoring (re-screen on list deltas).** The example admits screening results when they arrive; the trigger to re-screen on a list update is operational, not a state transition the kernel needs to know about.
