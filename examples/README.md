# The worked examples, by what they show you how to do

The gallery is named by domain, and that is the wrong index if you arrived
with a question. An embedder spent a week believing Morpholog could not admit
a set of records in one act. It can, and the example that shows it is called
`14_margin_call_run` - so they skipped it, because a margin-call run did not
sound like an invoice.

This table is the other index. The [main README](../README.md#worked-examples)
lists the same examples by the business problem each one governs; start there
if you want to read one end to end.

## I want to...

| ...do this | ...look here | the construct |
|---|---|---|
| admit a whole set of records in one act, all or nothing | [14_margin_call_run](14_margin_call_run/) | a collection parameter, plus `for x in c: admit ...` |
| find the version of a rate or term in force on a date | [10_trade_lifecycle](10_trade_lifecycle/) | `effective by (keys) on (date)` |
| promise that a dated record always exists where a rule needs one | [10_trade_lifecycle](10_trade_lifecycle/) | `total over P` on the invariant that guarantees it |
| correct a figure without rewriting what was decided on the old one | [02_verified_revenue](02_verified_revenue/) | `current pointer by` plus `superseded via` |
| stop a record ever being retracted | [04_approval_controls](04_approval_controls/) | `append only` |
| require that the person acting holds the authority | [04_approval_controls](04_approval_controls/) | `actor` in a `require` |
| name a gate, so a refusal says which rule said no | [05_insurance_claim_settlement](05_insurance_claim_settlement/) | `require some_name: ...` |
| find out what evidence a refusal is missing, and which act could supply it | [09_carbon_credit_provenance](09_carbon_credit_provenance/) | `explain` |
| cap a running total exactly, at the moment of admission | [05_insurance_claim_settlement](05_insurance_claim_settlement/) | `sum(... ) + amount <= limit` |
| bound a figure from both sides | [05_insurance_claim_settlement](05_insurance_claim_settlement/) | `min(a, b)` and `max(a, b)` |
| write a rule about the state before a change and after it | [07_chess_transition_invariants](07_chess_transition_invariants/) | `pre(...)` |
| insist a document was valid *on* a particular date | [06_clinical_trial_enrolment](06_clinical_trial_enrolment/) | `on_or_before` / `before` over `Date` |
| reuse one condition in several rules | [06_clinical_trial_enrolment](06_clinical_trial_enrolment/) | `define name(params): body` |
| work with exact instants and exact spans of time | [12_laytime_demurrage](12_laytime_demurrage/) | `Timestamp`, `Duration`, `at_or_before`, `no_longer_than` |
| put a unit on an amount, so tonnes cannot be added to dollars | [12_laytime_demurrage](12_laytime_demurrage/) | `Decimal[t]`, `Decimal[USD]` |
| round money to the penny, the way the contract says | [15_metered_billing](15_metered_billing/) | `round(x, quantum)` |
| name a figure the whole rulebook shares | [15_metered_billing](15_metered_billing/) | `const name = (value)` |
| hold a ratio, a rate or an advance limit | [11_borrowing_base](11_borrowing_base/) | multiplication and division, cross-multiplied for exactness |
| compute a read-side view from admitted claims | [03_double_entry_ledger](03_double_entry_ledger/) | `derived ... over ... value ...` |
| project several coordinates of a join into one view | [10_trade_lifecycle](10_trade_lifecycle/) | a `derived` head binding more than the key |
| record evidence *about* a claim, and refuse a claim without it | [09_carbon_credit_provenance](09_carbon_credit_provenance/) | claims whose subjects are other claims |
| expire a check, and re-flag when the world changes | [08_kyc_sanctions_screening](08_kyc_sanctions_screening/) | currentness with an expiry date |
| hand work to the outside world after a commit | [08_kyc_sanctions_screening](08_kyc_sanctions_screening/) | `intent` plus `emit` |
| refuse a combination that is fine one at a time | [01_settlement_netting](01_settlement_netting/) | `forall x in source: body` over the proposed set |
| show that a rule comes from a named statute | [13_biometric_identification_oversight](13_biometric_identification_oversight/) | the article-to-rule table in its README |
| watch a limit on a net position, long minus short | [10_trade_lifecycle](10_trade_lifecycle/) | `abs(...)` |
| make a skipped process step uncommittable, not just reviewable | [16_release_governance](16_release_governance/) | `require name: ...` gates, one per checklist step |
| gate an act on completeness over a declared set | [16_release_governance](16_release_governance/) | `forall p in PlatformDeclared(p): ...` inside a `require` |
| roll a date forward by calendar months, month-end safe | [17_covenant_reporting](17_covenant_reporting/) | `span(P3M)` shifting a `Date` |
| count the days between two dates and refuse any other figure | [17_covenant_reporting](17_covenant_reporting/) | date subtraction, `as_of - deadline` |
| record a value that follows which case the record shows | [18_scoped_charges](18_scoped_charges/) | `if(condition, a, b)` |
| keep a period inside one anniversary-anchored year | [19_charging_years](19_charging_years/) | `period_index(anchor, span, at)` |
| call Morpholog from an application | [etrm_embedder](etrm_embedder/) | the generated Python client |

## If the thing you want is not here

Two possibilities, and they are worth telling apart before you design around
the gap.

It may exist and be spelled differently:
[`docs/runtime-semantics.md`](../docs/runtime-semantics.md) has the full
surface-to-IR table, which is the complete list of what the language can say.
`morpholog --help` is the same for the tooling.

Or it may genuinely be missing, in which case the useful thing to send us is
**what you searched for**. Three of the first four capability requests we
received turned out to be features the asker could not find, and the search
term is what tells us whether an index like this one fixes it or whether an
example is misnamed.
