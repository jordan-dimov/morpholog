# Trade lifecycle

A commodity trade does not have a status. It has a history.

When a trading desk strikes a deal, it travels a path: a trader *captures*
it, the middle office *confirms* it against the counterparty and fixes an
official price, and in time it *settles*. Most systems record where a trade
is on that path in a single mutable field - `status = "confirmed"` - and
then spend the next three years reconstructing, from logs and emails, what
that field used to say and who was allowed to change it.

This example takes the other view. A trade is "captured" because a capture
claim exists for it; "confirmed" because a confirmation claim exists;
"settled" because a settlement claim exists. The phase is the accumulation
of admitted claims, and earlier claims never stop being true - so the
history reads itself, with no status column to overwrite and no audit
reconstruction to perform.

## The scenario

A power desk books a trade: buy 100 lots for calendar-2026 delivery, terms
effective from the trade date of 15 January, at a trader-captured price of
50. That captured figure is the trader's word for it - recorded, but
nothing may be settled against it yet.

The middle office holds authority to confirm power trades. Acting under
that authority it confirms the trade against the counterparty and sets the
official price at 52 - call that official figure `op1`. Settlement will
rely on `op1`, not on the trader's 50: existence and
admissibility-for-settlement are different things.

The trade begins to settle in slices over the delivery period, each
recorded against `op1` and each effective on a business date. Then two
ordinary things happen, the kind that make trade books hard to keep
honest.

First, the desk finds it keyed the official price wrong and corrects it to
49 - a new figure `op2`. The old figure `op1` stays on the record; `op2`
becomes the one in force; a lineage link records that `op2` supersedes
`op1`. A settlement already made under `op1` is left untouched - it remains
a true record of what was settled that day. The correction governs only
what may be settled next.

Second, the counterparty re-agrees the quantity: 120 lots, effective from 1
February. This is an *amendment*, and it is recorded after the fact -
booked today, effective from a date already in the past. It does not
overwrite the original terms: a new terms version is admitted alongside,
the original stays on the record, and each version carries the date its
quantity takes force. The quantity a trade carries is not one number but a
timeline.

An auditor later can ask, and the model answers by construction:

- *What price did this settlement rely on?* The settlement carries `op1`;
  `op1`'s figure (52) is still on the record, even though the current
  official price is now 49.
- *What quantity was effective on 20 February?* The version in force on
  that date - 120, once the amendment is known. Ask the same question of
  the book as it stood on 16 January, before the amendment was recorded,
  and the answer is 100. Same date, different answer - because new
  knowledge arrived, not because the past was rewritten.
- *Who confirmed it, and under what authority?* The confirmation event
  records the actor who confirmed it (its `confirmed_by`); the
  `MayConfirm(_, power)` authority that satisfied the gate when the
  confirmation was admitted.
- *Could a trade be settled for more than its terms allowed - or before
  any terms were effective at all?* No to either - the runtime would not
  have admitted it.

## The two clocks

That "same date, different answer" is not a trick. A trade's terms live on
two independent clocks, and the example keeps them apart:

- **When a claim is effective in the world** - the date a quantity takes
  force. This is just a date carried on an ordinary claim
  (`effective_from` on a terms version, `effective_on` on a settlement
  slice). Effective time can be written retroactively: an amendment booked
  in March can be effective from February.
- **When the system recorded a claim** - the append-only audit log. This
  clock only moves forward; you cannot record into the past. Any past
  moment can be replayed "as of" a transition.

Combine them and the book answers bitemporal questions - *what was
effective on date D, as we knew it at time T?* - with nothing more than a
date on a claim and the audit log. No valid-time columns, no temporal
database. The `TermsTimeline` read-side view, replayed as-of an earlier
transition, is what gives 100 on 16 January and 120 today for the same 20
February question.

## The two controls

The example is also built around the distinction between the two kinds of
rule, because they answer different questions:

- **Who may confirm, correct, or amend** is settled at the moment someone
  acts. It is a gate (`require`): confirming a power trade today stays valid
  even if that authority is withdrawn tomorrow. The authority is scoped per
  commodity and tied to the trade's *own* commodity, so authority over gas
  does not let you touch a power trade.
- **A trade may never be over-settled** must hold for all admitted state,
  forever. It is an invariant: no path, however the books are reached, may
  leave an over-settled trade behind. And "over-settled" is judged on the
  effective clock - the total settled effective on or before a date may not
  exceed the quantity the terms in force on that date allow.

`settle_trade` shows the split cleanly. Its gates are operational
preconditions - there must be an official price in force to settle on, and
the settlement id must be unused, so replaying a settlement cannot request
a second downstream payment. The structural truths (a settled trade was
confirmed; the cumulative effective cap holds) are left to the invariants.
This is what lets a backdated amendment *lift* the cap: a slice of 110
effective 20 February is refused while the terms in force say 100; once the
amendment to 120 (effective 1 February) is recorded, the very same slice
becomes admissible. The amendment changed what is true, and the runtime
re-judges admissibility against it.

## The position limit

Another invariant guards a different exposure. A desk's *net position* on a
commodity is its buys minus its sells - a signed figure, positive when net
long and negative when net short. A `PositionLimit` caps how far that net
may swing in *either* direction, and the rule says so in one comparison:

```
abs(buys - sells) <= limit
```

`abs` is the magnitude of the net, its distance from zero, so a single
bound holds the position both ways - too long and too short are the same
breach. A buy and an offsetting sell net against each other, so it is the
net that is bounded, never the gross: a 40-lot buy and a 35-lot sell sit at
a net of 5 even though they gross 75. Each trade contributes its current
quantity, so an amendment that grows a position can cross the limit just as
a new capture can, and the runtime refuses the change either way.

## The program

See [`trade_lifecycle.morph`](trade_lifecycle.morph) for the surface form.

### Claims

| Predicate | Role |
| --- | --- |
| `TradeCaptured(trade, commodity, direction)` | The trade's immutable identity (direction is `#buy` or `#sell`). Fixed once captured; the quantity lives on the versioned terms. |
| `TradeTerms(trade, version_id, quantity, delivery_period, effective_from)` | A versioned terms record carrying the date its quantity takes force. Capture admits the first; an amendment admits a later one. |
| `TradeTermsSupersedes(new_version_id, prior_version_id)` | Amendment lineage: which terms version amended which. Audit only - which version is in force on a date is decided by effective dates, not this chain. |
| `CapturedPrice(trade, price)` | The trader's price estimate. Recorded, but never settleable on its own. |
| `MayConfirm(principal, commodity)` | A desk's authority to confirm or amend for a commodity. Granted by `grant_confirm_authority`. |
| `TradeConfirmed(trade, counterparty, confirmation_id, confirmed_by)` | The confirmation event against the counterparty, stamping in the actor who confirmed it. Happens once. |
| `OfficialPrice(trade, price, official_price_id)` | An official price figure. Append-only; a correction admits a new one alongside the old. |
| `CurrentOfficialPrice(trade, official_price_id)` | The retractable pointer naming which official figure is in force - the settlement figure. The one moving part. |
| `OfficialPriceSupersedes(new_official_price_id, prior_official_price_id)` | Price-correction lineage: which official figure replaced which. |
| `TradeSettled(trade, settled_qty, settlement_id, official_price_id, effective_on)` | A settlement slice, recording the official price it relied on and the business date it is effective on. Several can stand for one trade. |
| `PositionLimit(commodity, limit)` | The risk cap on a commodity's net position, bounding `abs(buys - sells)`. One per commodity. |
| `TermsTimeline(trade, version_id, delivery_period, effective_from, quantity)` | Read-side view: the terms as a timeline, one row per version. Replayed as-of a transition, the bitemporal answer. |

`effective_on` on a settlement is the *business* date the slice belongs to
- the delivery day or invoice period it is for. It is deliberately not the
date the transition committed (that is the audit clock) and not the date
money actually moves (that is computed downstream).

### Invariants

| Invariant | What it says |
| --- | --- |
| `current_official_price_has_a_figure` | The in-force pointer must name an official figure that actually exists. |
| `at_most_one_current_official_price` | At most one official price is in force per trade. |
| `official_price_id_identifies_one_figure` | An official price id names one figure (same trade, same price), so the in-force pointer and the audit price lookup are unambiguous. |
| `official_price_has_captured_trade` | An official price belongs to a trade that was actually captured. With the rule above, a clean pointer -> figure -> trade provenance chain. |
| `official_price_chain_no_fork` | The price-correction chain stays linear: a figure has at most one direct successor. |
| `at_most_one_capture_per_trade` | A trade id identifies one captured trade (one commodity, one direction). |
| `terms_belong_to_captured_trade` | Every terms version belongs to a trade that was actually captured. |
| `terms_version_id_identifies_one_record` | A terms version id names one terms record, so the lineage and lookups are unambiguous. |
| `one_terms_version_per_effective_date` | At most one terms version per effective date, so "the quantity effective on that date" is a single number. An amendment must take a distinct effective date. |
| `trade_terms_chain_no_fork` | The amendment chain stays linear: a terms version has at most one direct successor. |
| `settled_within_effective_terms` | The total settled effective on or before any date may never exceed the quantity the terms in force on that date allow (inclusive) - the cumulative cap, judged on the effective clock. |
| `settled_date_has_effective_terms` | Every settled date has a terms version effective by it, so a slice cannot fall before the trade had any terms and escape the cap by vacuity. |
| `settlement_id_identifies_one_settlement` | A settlement id names one slice, so slices cannot be double-counted or hidden under a shared id. |
| `settled_trade_was_confirmed` | Settlement can never run ahead of confirmation, for all admitted state by any path. |
| `trade_terms_quantity_is_positive` | A terms quantity is positive - no zero-size or negative trade. |
| `settled_quantity_is_positive` | A settled slice is positive - a negative slice cannot make room under the running cap for an over-large one. |

### Transformations

| Transformation | Effect |
| --- | --- |
| `capture_trade(...)` | Books a trade: its identity, the trader's price, and the first terms version effective from the trade date. Refuses a second capture under the same id. |
| `amend_trade_terms(...)` | Admits a new terms version - re-agreed quantity, corrected delivery - effective from a date that may be in the past. The old version stays; lineage is recorded; the new date must be distinct. Gated on commodity-scoped authority. |
| `grant_confirm_authority(principal, commodity)` | Grants a desk per-commodity authority to confirm and amend. |
| `confirm_trade(...)` | The middle office confirms the trade and sets the official price in force. Gated on commodity-scoped authority tied to the trade's own commodity, and on the trade not already being confirmed. |
| `correct_official_price(...)` | Restates the official price: admits the corrected figure, moves the in-force pointer, records the supersession. The prior figure and any settlement made under it stay on the record. |
| `settle_trade(...)` | Records a settlement slice against the official price in force, effective on a business date, and emits a downstream settlement-request intent. Gated on an in-force official price, on terms effective by the settlement date, and on a fresh settlement id (an idempotency key, so replaying one is refused before the emit). May run more than once per trade. The cumulative effective cap is an invariant, not a gate. |
| `set_position_limit(commodity, limit)` | Sets the net-position limit for a commodity, gated on confirm authority for it and refused if one is already set. The `within_position_limit` invariant then holds it against every future capture and amendment. |

## How to run it

```bash
# The effective (valid-time) axis: cap, amendment, restatement.
cargo test -p morpholog-examples --test trade_lifecycle

# The transaction-time axis: bitemporal as-of replay (needs PostgreSQL).
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --test trade_lifecycle -- --test-threads=1
```

The sync tests cover capture and duplicate-capture; the commodity-scoped
confirmation authority; amendment (new version, unknown-version and
already-amended rejection, the distinct-effective-date and version-id
uniqueness rules); official-price correction as restatement; settlement
before confirmation, within and over the effective quantity, in slices and
with cumulative overrun rejected; settlement before any terms are effective,
and the positive-quantity rules on terms and slices; the headline pair - a backdated amendment
lifting the cap so a previously-refused slice admits, and a settlement made
under the prior terms staying standing after a later amendment; and the
price-axis counterpart, a correction after settlement leaving the prior
settlement standing. The PG test proves the bitemporal property: the same
effective-date question answered 100 as of the earlier transition and 120
once the backdated amendment is recorded.

## What this example deliberately does not cover

"Lifecycle" here means the capture-to-settlement path *with price
correction and effective-dated amendment* - not the exception-handling
lifecycle. Each item below is a simplification, not a gap, deferred until a
forcing scenario arrives.

- **Exception handling.** Cancellation, novation, partial termination -
  the exception taxonomy of a real trade lifecycle. These are the natural
  home for validity-window and exception/repair claims, deferred until an
  example forces them. This example covers capture, confirmation,
  correction, amendment, and settlement, and deliberately claims no more.
- **Same-effective-date correction.** Amendment here uses *distinct*
  effective dates, so "the version in force on D" is unambiguous. Two
  versions sharing an effective date - a same-date correction - would need
  a supersession tiebreak, the way the official price already does. A
  separate move, deferred until forced.
- **Pricing and mark-to-market.** How much money a settlement moves is
  computed outside Morpholog and returns, where it carries legitimacy
  weight, as admitted claims. The kernel governs only that the lifecycle
  steps are legitimate.
