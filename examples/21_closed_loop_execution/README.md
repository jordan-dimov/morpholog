# Closed-loop execution: the order nobody authorised

A trading agent proposes orders at a venue and is trusted with nothing
else: it never holds the venue's credential. The record admits an order
only under the agent's mandate, and a separate executor, the one holder
of the credential, sends each admitted order and decides nothing. That
prevents a great deal. It cannot see an order placed with a second
credential somebody left lying around, or a login a person uses by hand.

So the loop closes. The venue's own record of what it accepted comes
back in as claims, admitted like any other, and a deterministic read
compares it with what this record authorised. Four findings come out,
and one of them is the point: an order the venue reports that no
admitted act authorised. Prevention may have been bypassed elsewhere.
The venue's own record exposes it, provided this record is willing to
admit the evidence - which is why there is deliberately no rule saying
a venue report must name an authorised order. Such a rule would make
the bypass uncommittable here while leaving it done at the venue.

Three parties act on the record, each bound to its own database login,
so that none can speak another's name: an operator who appoints the
other two, the agent, and a reporter who brings the venue's reports in.
The executor is a fourth party that never proposes.

## The programme at a glance

Claims: the operator and the login binding of each name
(`ActorAssertionRestricted`, `ActorAssertionAuthority`), the venue and
who speaks for it, a `TradingMandate` per trader and instrument with
its per-order cap, `OrderAuthorised` (append only) and `VenueReport`
(append only, unique by venue and message sequence, so a replayed
message is refused by name). One intent, `PlaceOrder`, carrying the
order's own subject, which the executor sends as the venue's client
reference on every attempt.

| Act | Who | What it refuses |
|---|---|---|
| `appoint_operator` | anyone, once | A second operator. Arms the name and grants its login in the same act. |
| `enrol_login`, `declare_venue`, `grant_mandate`, `grant_feed` | the operator | Any other proposer; a mandate or a feed to a name not yet bound to a login; a mandate of nothing. |
| `place_order` | a trader | No mandate on the instrument, a size over the cap, a non-positive figure, a side other than `#buy` or `#sell`, an undeclared venue. |
| `observe_venue_report` | the feed operator | Any other proposer; the same message twice. Not an unmatched order: that is evidence. |

The reconciliation, computed from the claims and never stored:

| Finding | Over | What it says |
|---|---|---|
| `Matched(order, report)` | an authorised order and a report under its reference on the same venue, instrument, side, size and price | The venue did what was authorised. |
| `Mismatched(report, order)` | a report under an authorised reference on other terms | The reference was honoured and the order was not. |
| `Unobserved(order)` | an authorised order with no report naming it | In flight, lost or refused by the venue; no clock, so "as of this read". |
| `Unauthorised(report, venue_order_id)` | a report whose reference names no authorised order | The headline. The venue's identifier is named so someone can go and look. |
| `Ambiguous(order)` | two reports under one reference with different venue identifiers | The observations disagree about which external order this is: possibly one order sent twice. Not proof of a duplicate. |

## Run it

```bash
morpholog check -v examples/21_closed_loop_execution/closed_loop_execution.morph
morpholog inspect derived examples/21_closed_loop_execution/closed_loop_execution.morph Unauthorised
```

The whole loop runs against PostgreSQL in
`crates/morpholog-postgres/tests/closed_loop_execution.rs`, with the
venue as a fixture that keeps a book of client references and hands
out its own order identifiers:

1. The agent proposes a lawful order; the record admits it and emits
   one intent.
2. The executor claims the intent through the outbox lease and places
   the order at the venue under the order's subject.
3. The reporter brings the venue's report in.
4. `Matched` names the order and the report; nothing else fires.
5. Somebody places an order at the venue with a credential the
   executor does not hold; the reporter brings that in too.
6. `Unauthorised` names it, with the venue's own identifier.

The same file runs the other shapes: an executor that keeps the
reference and sends ten times the size (`Mismatched`), an order the
venue never reports (`Unobserved`), a worker that loses its lease after
the venue accepted, so the intent is delivered again with the same
identity and the venue acts twice (`Ambiguous`), and the agent's login
proposing under the operator's and the reporter's names, refused before
anything is admitted.

```bash
cargo test -p morpholog-examples --test closed_loop_execution
DATABASE_URL=postgres:///morpholog_dev cargo test -p morpholog-postgres --test closed_loop_execution
DATABASE_URL=postgres:///morpholog_dev cargo test -p morpholog-cli --test receipt_e2e a_receipt_states
```

The last is a receipt: the record checkpointed after the unauthorised
report was ingested, exported as a complete prefix, and `audit receipt
... --derived Unauthorised` issued offline, then verified offline. The
receipt states the finding over that committed history. It does not
state that the venue's feed was complete or true.

## Deliberately not covered

Undoing an unauthorised order: this rung reports, and a cancelling
order is a later act. The venue itself, beyond the fixture. Partial
fills, amendments and cancellations. A position or exposure limit: the
mandate is a cap per order. And three things the deployment asserts
and the record cannot prove: that the executor alone holds the venue
credential, that the reports brought in are all of them, and that the
venue echoes client references faithfully. A report nobody brought in
is invisible to every read here.
