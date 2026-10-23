# Order blotter: a claim recorded in error

A desk's blotter is append only: every order, fill and cancellation it
ever recorded stays. Then an order is recorded as cancelled, and the
venue reports that it filled. The cancel line is wrong, it cannot be
removed, and the fill cannot be admitted while it stands.

This is the worked example for the erratum idiom: a claim about a
claim. `RecordedInError(claim_id, reason)` names the recorded line by
its own identity, is itself append only, and is admitted only by the
book's appointed corrector. Rules read cancels through `standing_cancel`,
a cancel with no erratum against it, so the record holds two things at
once, both permanently true: the cancel was recorded and admitted, and
it is no longer to stand. What was actually true, the fill, then goes
on the record through the ordinary act.

It differs from the supersession the verified-revenue and
trade-lifecycle examples teach. There, both figures were lawful in their
time and a pointer moves to the newer one. Here nothing moves: the line
never deserved to stand, it stays on the record marked, and the truth
is admitted fresh.

The idiom forced no kernel change. Two things it needs are written out
by hand, and the `.morph` says why: an erratum names a line that
exists, and uniqueness over standing claims, which a declared `unique
by` cannot express because it knows nothing of errata.

## The programme at a glance

| Claim | Meaning |
|---|---|
| `Order(order, book, qty)` | An order placed on a book. |
| `Fill(order, fill_id, qty)` | A fill the venue reported, each with its own identity. |
| `Cancel(order, cancel_id)` | The order recorded as cancelled, with the identity an erratum names. |
| `RecordedInError(claim_id, reason)` | A recorded line is not to stand, and why. One per line, never withdrawn. |
| `MayCorrect(book, corrector)` | Who may record an erratum on a book's lines. |

`standing_cancel(order, cancel_id)` is a cancel with no erratum against
it; every rule reads cancels through it.

| Rule | What it refuses |
|---|---|
| `fill_has_an_order`, `cancel_has_an_order` | A fill or cancel on an order never placed. |
| `erratum_names_a_cancel` | An erratum against a cancel that does not exist. |
| `a_cancel_closes_only_an_unfilled_order` | A fill while a cancel stands, and a cancel on a filled order. |
| `place` and `fill` gates | A quantity that is not positive. |
| `one_standing_cancel_per_order` | A second standing cancel on one order. |
| `fills_within_the_order` | Fills past the order's quantity. |

Transformations: `place`, `fill`, `cancel`, `appoint_corrector`,
`record_erratum`. The erratum act takes the cancel's identity and a
reason, and reads the order and book off the record, so authority is
checked against the book the cancel belongs to.

One assumption the record does not enforce: `appoint_corrector` checks
no authority, so it must be reachable only from the desk's trusted
setup. Anyone who can run it on an unappointed book becomes its
corrector, and with that the authority guarantee is gone.

Every rule is checked case by case (`check -v` reports `invariant
cases: all bound`): the rules start from the recorded cancel, so an
erratum reaches only its own order's case, and a breach on one order
blocks no act on another.

## Run it

```bash
morpholog check -v examples/24_order_blotter/order_blotter.morph
morpholog propose examples/24_order_blotter/order_blotter.morph place \
  --actor desk --args-named '{"order":"o1","book":"book_a","qty":"100"}'
```

The `.morph` teaches the domain from scratch - the guided tour lives
there, not here.

## Deliberately not covered

A fill recorded in error (the same shape, an erratum naming the fill),
partial fills and cancelling the unfilled remainder, a cancellation
request still on its way to the venue (here a cancel is the desk's
record that the venue cancelled), and how a desk's trust root appoints
the first corrector.
