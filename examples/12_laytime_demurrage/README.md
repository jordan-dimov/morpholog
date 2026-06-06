# Laytime and demurrage: the argument about minutes

A ship arrives at Sines to load. The master tenders Notice of Readiness at
14:00. The charterer says the clock should not have started until the next
working period; the owner says it started six hours after the notice, as the
charterparty plainly states. Rain stops the cranes for four and a half hours
on the second day - excepted time, says the charterer; counted, says the
owner, because the rain clause does not apply on demurrage. Weeks later a
demurrage claim arrives, built from a Statement of Facts the two sides do not
quite share, and someone has to audit every line of it.

This is not an edge case. Demurrage is how voyage chartering prices delay -
an expected, agreed outcome settled to the minute - and disputing the minutes
is an industry in itself. The question a tribunal asks is the question this
example makes mechanical: **which events were admitted, under what rules did
each minute count, and does the claimed total follow?**

## What the model governs

[`laytime.morph`](laytime.morph) models the first stage of that story:

- **The fixture** - the agreed numbers: vessel, port, and the laytime
  allowance, as a duration (`PT120H` is five days).
- **The notice** - NOR tendered at an exact instant, one per voyage.
- **Commencement** - *computed*, not asserted: the tendering instant shifted
  by the agreed turn time. There is no way to start the clock anywhere else.
- **Counting intervals** - the Statement of Facts, one stretch at a time,
  each carrying its ends and its exact computed length. The gates police the
  order of events (no counting before commencement, no interval that ends
  before it begins); the invariants make the arithmetic permanent (a recorded
  length can never disagree with its ends, counted time can never precede the
  clock, the clock starts once).
- **Time on demurrage** - the answer both parties care about, *derived*:
  counted time minus the allowance, floored at zero. Never stored, so it can
  never drift from the intervals it is computed from.

The deliberate non-rule is the one a first draft always gets wrong: exceeding
the allowance is **not** an invariant violation. Going on demurrage is the
normal commercial story - the model records it calmly and prices it later;
forbidding it would make the model unable to describe the very situation it
exists to settle.

## What this example forced

This is the forcing programme for Morpholog's time values: `Timestamp` (an
exact UTC instant) and `Duration` (an exact span), with instant-minus-instant
producing a span, instant-plus-span producing an instant, spans summing
against an allowance, and the comparator families that read like the domain
sentences they enforce (`commenced_at at_or_before from`, counted
`no_longer_than` allowed).

Deliberately not yet here, each waiting for its stage of this example:

- **Port-local time.** Every instant above is UTC. The 25-hour local day when
  the clocks fall back, "weather working days" with local calendar
  boundaries - that interpretation will enter as *admitted claims* from an
  external calendar authority, not as a hidden runtime timezone database.
- **Corrections.** The terminal revises the Statement of Facts after the
  demurrage claim settles. The prior settlement must stand on its own record
  while future calculation follows the corrected facts - the pattern the
  verified-revenue and trade-lifecycle examples pin, in a new costume.
- **Units.** The demurrage *rate* (USD per day) and the cargo's tonnage are
  deliberately absent: money enters when unit-tagged quantities do.

## Running it

```bash
morpholog check examples/12_laytime_demurrage/laytime.morph
morpholog inspect guarantees examples/12_laytime_demurrage/laytime.morph
```

The typed walkthrough lives in
`crates/morpholog-examples/tests/laytime_demurrage.rs`: a full voyage from
fixture to ten hours on demurrage, the zero-floor for a voyage that finished
early, and each gate refusing the out-of-order event it exists to refuse.
