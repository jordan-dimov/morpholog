# KYC sanctions and PEP screening

A bank cannot open an account for a new customer until it has checked them against sanctions and PEP lists. Those checks are not a single event - they are a continuous obligation. A clean result has an expiry; a list update can flag a previously-clean customer; a flagged customer cannot be admitted until an analyst adjudicates the match. The legal weight is real: a missed alert is a regulatory breach, and a missed event in the outbox - a `SARFiled` quietly routed nowhere because someone mistyped `SARFilled` - is the same breach with no audit trail.

This example is the smallest Morpholog programme that holds those constraints honestly. The central rule is:

> No customer may hold *onboarded* standing without a current screening per list-type whose disposition is *clean* (or adjudicated-clear) and whose expiry has not been reached by the onboarding date - and no unresolved match against any current screening.

That single sentence becomes four invariants. Onboarding the wrong customer is structurally impossible.

## What it forces

This example is the worked-example reason for `IntentDecl` to exist in the kernel. The four intents - `ScreeningRequested`, `MatchRaised`, `CustomerOnboarded`, `CustomerRejected` - each route to a distinct downstream consumer (provider API, analyst queue, core banking, compliance reporting). If `MatchRaised` were a stringly-typed emit and someone wrote `MatchRased`, it would silently create a new outbox partition that no analyst reviews. Declaring intents as first-class vocabulary, parallel to predicates, makes that impossible at validation time.

The example also exercises the round-trip compute pattern from the three-zone doctrine (`docs/scope-and-ambition.md`):

```
request_screening -> outbox ScreeningRequested -> external provider
                                                          |
                                                          v
                                            (clean) record_clean_screening_result
                                            (match) record_match_screening_result
                                              |
                                              v
                              (false positive) adjudicate_match_as_false_positive
```

The bank's transactional state stays inside Morpholog. The screening call - the actual decision of whether the name matches anything on World-Check or Refinitiv - happens outside. The result lands as a separate transformation that admits the disposition into the commit zone, where the invariants gate onboarding.

## What stays out

- **Risk-tier-dependent refresh windows.** Real KYC has 365-day standard / 90-day high-risk / event-driven refresh schedules; this example uses one window (the screening's `expires_on` field carries it directly). Tier-dependent windows would need a `CustomerRiskTier` predicate and a per-tier comparison; deferred until a worked example forces them.
- **EDD branch and senior-management approval.** Politically-exposed persons trigger Enhanced Due Diligence requiring source-of-wealth documentation and senior sign-off. The shape is `approval_controls`-flavoured (authority + limit); a future example could combine the two.
- **SAR filing.** Suspicious Activity Reports are their own audit-trail story and would need a different downstream consumer model. Out of scope here.
- **Adverse media screening.** Real onboarding screens against sanctions, PEP, AND adverse media; this example covers the first two.
- **Continuous monitoring (re-screen on list deltas).** The example admits screening results when they arrive; the trigger to re-screen on a list update is operational, not a state transition the kernel needs to know about.
