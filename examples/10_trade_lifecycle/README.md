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

A power desk books a trade: buy 100 lots for calendar-2026 delivery, at a
trader-captured price of 50. That captured figure is the trader's word for
it - recorded, but nothing may be settled against it yet.

The middle office holds authority to confirm power trades. Acting under
that authority it confirms the trade against the counterparty and sets the
official price at 52 - call that official figure `op1`. Settlement will
rely on `op1`, not on the trader's 50: existence and
admissibility-for-settlement are different things.

The trade settles: 100 lots, recorded against `op1`. Then the middle office
finds it had keyed the official price wrong and corrects it to 49 - a new
figure `op2`. The old figure `op1` stays on the record; `op2` becomes the
one in force; a lineage link records that `op2` supersedes `op1`. The
settlement already made under `op1` is left untouched - it remains a true
record of what was settled that day. The correction governs only what may
be settled next.

An auditor later can ask, and the model answers by construction:

- *What price did this settlement rely on?* The settlement carries `op1`;
  `op1`'s figure (52) is still on the record, even though the current
  official price is now 49.
- *What is the official price now?* `op2` - the in-force pointer.
- *Who confirmed it, and under what authority?* The confirmation event
  records the actor who confirmed it (its `confirmed_by`); the
  `MayConfirm(_, power)` authority that satisfied the gate when the
  confirmation was admitted.
- *Could a trade be settled for more than was captured?* No - the runtime
  would not have admitted it.

## The two controls

The example is built around the distinction between the two kinds of rule,
because they answer different questions:

- **Who may confirm or correct a price** is settled at the moment someone
  acts. It is a gate (`require`): confirming a power trade today stays valid
  even if that authority is withdrawn tomorrow. The authority is scoped per
  commodity and tied to the trade's *own* commodity, so authority over gas
  does not let you confirm a power trade.
- **A trade may never be settled for more than the quantity captured** must
  hold for all admitted state, forever. It is an invariant: no path,
  however the books are reached, may leave an over-settled trade behind.

`settle_trade` shows the split cleanly. Its gates are operational
preconditions - there must be an official price in force to settle on, and
the settlement id must be unused, so replaying a settlement cannot request
a second downstream payment. The structural truths (a settled trade was
confirmed; the *total* settled stays within the captured quantity) are
left to the invariants.

## The program

See [`trade_lifecycle.morph`](trade_lifecycle.morph) for the surface form.

### Claims

| Predicate | Role |
| --- | --- |
| `TradeCaptured(trade, commodity, direction, quantity, delivery_period)` | The trade's terms, as the trader booked them. Fixed once captured. |
| `CapturedPrice(trade, price)` | The trader's price estimate. Recorded, but never settleable on its own. |
| `MayConfirm(principal, commodity)` | A desk's authority to confirm prices for a commodity. Granted by `grant_confirm_authority`. |
| `TradeConfirmed(trade, counterparty, confirmation_id, confirmed_by)` | The confirmation event against the counterparty, stamping in the actor who confirmed it. Happens once. |
| `OfficialPrice(trade, price, official_price_id)` | An official price figure. Append-only; a correction admits a new one alongside the old. |
| `CurrentOfficialPrice(trade, official_price_id)` | The retractable pointer naming which official figure is in force - the settlement figure. The one moving part. |
| `Supersedes(new_official_price_id, prior_official_price_id)` | Correction lineage: which official figure replaced which. |
| `TradeSettled(trade, settled_qty, settlement_id, official_price_id)` | A settlement slice, recording the official price id it relied on. A trade settles in slices, so several can stand for one trade. |

### Invariants

| Invariant | What it says |
| --- | --- |
| `current_official_price_has_a_figure` | The in-force pointer must name an official figure that actually exists. |
| `at_most_one_current_official_price` | At most one official price is in force per trade. |
| `official_price_id_identifies_one_figure` | An official price id names one figure (same trade, same price), so the in-force pointer and the audit price lookup are unambiguous. |
| `official_price_has_captured_trade` | An official price belongs to a trade that was actually captured. With the rule above, a clean pointer -> figure -> trade provenance chain. |
| `at_most_one_direct_successor` | The correction chain stays linear: a figure has at most one direct successor. |
| `settled_quantity_within_captured` | The *total* settled across every slice can never exceed the captured quantity (inclusive) - the cumulative-cap shape the insurance example pins against a policy aggregate. |
| `settlement_id_identifies_one_settlement` | A settlement id names one settlement (same trade, quantity, official price), so slices cannot be double-counted or hidden under a shared id. |
| `at_most_one_capture_per_trade` | A trade id identifies one captured trade, so "the captured quantity" is a single well-defined number. |
| `settled_trade_was_confirmed` | Settlement can never run ahead of confirmation, for all admitted state by any path. |

### Transformations

| Transformation | Effect |
| --- | --- |
| `capture_trade(...)` | Books a trade: its terms and the trader's price. Refuses a second capture under the same id. |
| `grant_confirm_authority(principal, commodity)` | Grants a desk per-commodity confirmation authority. |
| `confirm_trade(...)` | The middle office confirms the trade and sets the official price in force. Gated on commodity-scoped authority tied to the trade's own commodity, and on the trade not already being confirmed. |
| `correct_official_price(...)` | Restates the official price: admits the corrected figure, moves the in-force pointer, records the supersession. The prior figure and any settlement made under it stay on the record. |
| `settle_trade(...)` | Records a settlement slice against the official price in force, and emits a downstream settlement-request intent. May run more than once per trade, each slice under a fresh settlement id - the id is an idempotency key, so replaying one is refused before the emit. The cumulative cap is an invariant, not a gate. |

## How to run it

```bash
cargo test -p morpholog-examples --test trade_lifecycle
```

The tests cover capture and duplicate-capture; the commodity-scoped
confirmation authority (unauthorised, wrong-commodity, authorised); double
confirmation; correction moving the in-force pointer while preserving
history; wrong-commodity correction; settlement before confirmation,
within quantity, and over quantity; settling in slices within the captured
total, with rejection of slices that overrun it or reuse a settlement id;
and the heart of the example - a correction after settlement leaving the
prior settlement standing, still pointing at the official price it relied on.

## What this example deliberately does not cover

"Lifecycle" here means the capture-to-settlement path *with price
correction* - not the exception-handling lifecycle. Each item below is a
simplification, not a gap, deferred until a forcing scenario arrives.

- **Exception handling.** Cancellation, novation, partial termination -
  the exception taxonomy of a real trade lifecycle. These are the natural
  home for validity-window and exception/repair claims, deferred until an
  example forces them. This example covers the happy path plus correction,
  and deliberately claims no more.

- **Effective time as a separate axis.** "Amend a trade effective as of an
  earlier date, then replay the book" needs effective time distinct from
  the transaction-time replay as-of already supports. A later slice.
- **Trade re-versioning.** Amending the *terms* (quantity, delivery) by
  superseding the whole trade with a new version, rather than correcting a
  price. A different amendment shape, deferred.
- **Pricing and mark-to-market.** How much money a settlement moves is
  computed outside Morpholog and returns, where it carries legitimacy
  weight, as admitted claims. The kernel governs only that the lifecycle
  steps are legitimate.
