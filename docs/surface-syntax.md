# The surface syntax on one page

What a `.morph` file can say, each form once, inside programmes small
enough to read whole. Every block on this page is parsed, validated and
held lint-clean by a test, and the modelling claims the prose makes are
proposed through the kernel by a second one, so what it shows is what
the runtime does. What each form *means* is in
[runtime semantics](runtime-semantics.md); this page is for the hand that
is writing the file.

Three words first. A **claim** is a statement the record has accepted,
`Account(#acct_1, #alice, 100)`. A **transformation** is an act that proposes
claims to admit or retract. An **invariant** is a condition every
accepted change must leave true. Claims are admitted and retracted,
never edited: `admit` adds a claim, it does not update one.

## A programme

```morph
program sheet

-- Declarations: a claim's fields and their kinds; an intent is what
-- the outside world is asked to do after a commit; a constant is
-- written out in place before the rules take effect.
predicate Account(account: Subject, owner: Subject, balance: Decimal)
    unique by (account)
intent AccountOpened(account: Subject)
const opening_minimum = (100)

-- A rule: a condition every accepted change leaves true.
invariant accounts_open_in_credit:
    Account(a, _, balance) implies opening_minimum <= balance

-- An act: a gate on the state as it is, then the claims it proposes.
transformation open(account, owner, deposit):
    require not Account(account, _, _)
    admit Account(account, owner, deposit)
    emit AccountOpened(account)
```

A claim is replaced by retracting it and admitting another, never edited
in place; `rebalance` under declarations below shows the shape.

A file is one programme; declarations, rules and acts may come in any
order. Comments start with `--`. Indentation is layout: a body is the
lines indented under its head, and parentheses switch layout off, so a
long expression may break across lines inside them.

Kinds: `Subject`, `Decimal`, `Bool`, `Date`, `Timestamp`, `Duration`,
`Collection`, `Any`, and a quantity `Decimal[USD]` whose unit is a label
of your own. A subject literal is `#name`; a variable is bare. `true` and
`false` are reserved and not yet writable, so a two-way status is a
subject, `#open` or `#closed`.

## Acts: the statements, and what they read

```morph
program acts

predicate Desk(desk: Subject)
predicate Mandate(trader: Subject, desk: Subject, cap: Decimal)
    unique by (trader, desk)
predicate Order(order: Subject, trader: Subject, qty: Decimal)
    unique by (order)
    append only
predicate Flagged(order: Subject)
intent Placed(order: Subject)

transformation place(desk, qty):
    -- A gate: yes or no on the state as it is. It keeps none of what it
    -- matched, so name it, and read its bindings in the same line.
    require within_mandate: Mandate(actor, desk, cap) and 0 < qty and qty <= cap
    -- A lookup that must match exactly one claim, keeping its bindings.
    bind the_desk: Desk(desk)
    -- A value read from one claim; a computed value; a fresh identifier
    -- nobody chose.
    let cap = value Mandate(actor, desk, _)
    let half = min(qty, cap) / 2
    let order = new Subject()
    admit Order(order, actor, half)
    emit Placed(order)

transformation flag_all(orders):
    for o in orders:
        require Order(o, _, _)
        admit Flagged(o)

transformation unflag(order):
    require Flagged(order)
    retract Flagged(order)
```

Every statement reads the state as it was when the act began: a claim
admitted two lines up is not visible to a `require` below it. The rules
then judge the state the act proposes. `actor` is the identity supplied
with the proposal; the record can check what it may do against claims,
and who it really is must be settled at the integration boundary.

## Rules: the propositions

The forms: `implies`, `and`, `or`, `not`, `xor`, `forall x in source:
body`, `exists x: body`, `pre(...)` for the state before the change,
`x in xs` for membership, and the comparators. A rule may open with
`let` lines naming a figure, as `invoices_follow_signing` does under
dates below.

```morph
program rules

predicate Period(period: Subject, status: Subject)
    unique by (period)
predicate Entry(entry: Subject, period: Subject, amount: Decimal)
    unique by (entry)
    append only
predicate Outcome(entry: Subject, verdict: Subject)
    unique by (entry)
predicate Counter(counter: Subject, count: Decimal)
    unique by (counter)
predicate Allowed(verdicts: Collection)

transformation open_period(period):
    admit Period(period, #open)

-- Whether the period is open is checked at the moment of posting: a
-- gate. A rule saying no entry sits in a closed period would refuse
-- the closing of a period with entries lawfully posted while it was
-- open, which is the wrong question to ask of history.
transformation post(entry, period, amount):
    require Period(period, #open)
    admit Entry(entry, period, amount)

transformation close_period(period):
    require Period(period, #open)
    retract Period(period, #open)
    admit Period(period, #closed)

transformation decide(entry, verdict):
    require Entry(entry, _, _)
    admit Outcome(entry, verdict)

transformation count(counter, before, after):
    retract Counter(counter, before)
    admit Counter(counter, after)

transformation allow(verdicts):
    admit Allowed(verdicts)

invariant entries_are_positive:
    Entry(e, _, amount) implies 0 < amount < 1000000

-- A rule must describe a state the acts can reach: requiring a verdict
-- before a large entry may exist would leave `post` and `decide`
-- waiting on each other.
invariant outcomes_have_entries:
    Outcome(e, _) implies (exists amount: Entry(e, _, amount))

invariant one_verdict_or_the_other:
    Outcome(e, v) implies (v = #accepted xor v = #rejected)

invariant verdicts_are_allowed:
    Outcome(e, v) and Allowed(vs) implies v in vs

invariant every_allowed_verdict_is_a_symbol:
    Allowed(vs) implies (forall v in vs: v != #none)

invariant the_counter_only_rises:
    Counter(c, after) and pre(Counter(c, before)) implies before <= after
```

## Values

```morph
program values

predicate Rate(tariff: Subject, pence_per_kwh: Decimal)
    unique by (tariff)
predicate Reading(meter: Subject, reading: Subject, kwh: Decimal)
    unique by (reading)
predicate Line(line: Subject, meter: Subject, tariff: Subject, net: Decimal, source: Subject)
    unique by (line)
predicate Limit(account: Subject, cap: Decimal)
    unique by (account)
predicate Exposure(position: Subject, account: Subject, amount: Decimal)
    unique by (position)

const penny = (0.01)

transformation register_rate(tariff, pence_per_kwh):
    require not Rate(tariff, _)
    admit Rate(tariff, pence_per_kwh)

transformation read_meter(meter, reading, kwh):
    admit Reading(meter, reading, kwh)

transformation set_limit(account, cap):
    require not Limit(account, _)
    admit Limit(account, cap)

transformation expose(position, account, amount):
    admit Exposure(position, account, amount)

transformation bill(line, meter, reading, tariff, net, source, proposed):
    -- A missing rate refuses the bill; `default` would bill at nothing.
    bind Rate(tariff, rate)
    let used = if(source = #meter, value Reading(meter, reading, _), proposed)
    require net = round(used * rate * penny, penny)
    admit Line(line, meter, tariff, net, source)

invariant exposure_within_limit:
    Limit(a, cap) implies sum(amount | Exposure(_, a, amount)) <= cap

-- A sum of nothing is zero; the largest of nothing is an error, so the
-- extremum is guarded by the exposure that makes it answerable.
invariant largest_position_within_limit:
    Limit(a, cap) and (exists p: Exposure(p, a, _)) implies max(amount | Exposure(_, a, amount)) <= cap

invariant exposure_is_bounded_either_way:
    Exposure(p, _, amount) implies abs(amount) <= 1000000 and min(amount, 0) <= 0
```

Arithmetic is `+ - * / %` over decimals, same-unit quantities and the
date and time forms below. `sum(target | body)` adds a target over every
binding of the body and is the typed zero when nothing matches;
`max(target | body)` and `min(target | body)` pick an end and have no
answer for nothing. `value P(args)` reads one field of one claim, `_`
marking the field wanted, with an optional `default expr` for when there
is none. `if(test, then, otherwise)` evaluates only the branch it
selects.

## Dates, instants, durations, spans and quantities

```morph
program time

predicate Contract(contract: Subject, signed_on: Date, starts_at: Timestamp, notice: Duration, cargo: Decimal[t])
    unique by (contract)
predicate Delivery(delivery: Subject, contract: Subject, delivered_at: Timestamp, tonnes: Decimal[t])
    unique by (delivery)
predicate Invoice(invoice: Subject, contract: Subject, issued_on: Date, charging_year: Decimal, year_starts_on: Date)
    unique by (invoice)

const epoch = (@2000-04-01)

transformation sign(contract, signed_on, starts_at, notice, cargo):
    admit Contract(contract, signed_on, starts_at, notice, cargo)

transformation deliver(delivery, contract, delivered_at, tonnes):
    bind Contract(contract, _, starts_at, notice, cargo)
    require starts_at at_or_before delivered_at
    require delivered_at - starts_at no_shorter_than notice
    require tonnes <= cargo
    admit Delivery(delivery, contract, delivered_at, tonnes)

transformation invoice(invoice, contract, issued_on):
    bind Contract(contract, signed_on, _, _, _)
    require signed_on on_or_before issued_on
    require issued_on before signed_on + span(P1Y)
    let index = period_index(epoch, span(P1Y), issued_on)
    let year_starts_on = period_start_of(epoch, span(P1Y), index)
    let year = 2000 + index
    admit Invoice(invoice, contract, issued_on, year, year_starts_on)

invariant delivered_within_the_hold:
    Contract(c, _, _, _, cargo) implies sum(t | Delivery(_, c, _, t)) <= cargo

invariant notice_is_at_least_a_day:
    Contract(c, _, _, notice, _) implies duration(PT24H) no_longer_than notice

invariant invoices_follow_signing:
    let deadline = (signed_on + span(P1Y))
    Contract(c, signed_on, _, _, _) and Invoice(i, c, issued_on, _, _) implies signed_on on_or_before issued_on and issued_on before deadline
```

Literals: a date `@2026-05-22`; an instant `@2026-10-24T14:00:00Z`, or
with an offset, `@2026-10-24T16:00:00+02:00`, which names the same
instant; a duration `duration(PT6H)`; a calendar span `span(P3M)`, months
then days with month ends clamped, allowed in expressions and never
stored; a quantity `25000 USD` or `0 t`. The comparators are spelled per
kind: decimals and quantities `<= < >= >`; dates `on_or_before`,
`before`, `on_or_after`, `after`; instants `at_or_before`,
`strictly_before`, `at_or_after`, `strictly_after`; durations
`no_longer_than`, `shorter_than`, `no_shorter_than`, `longer_than`; and
`a <= x <= b` chains. A date minus a date is days as a decimal; an
instant minus an instant is a duration. `period_index(anchor, span, at)`
counts periods from an anchor and `period_start_of(anchor, span, index)`
goes back.

## Declarations that promise something

```morph
program disciplines

-- One claim per key: the keys determine the whole claim.
predicate Balance(account: Subject, amount: Decimal)
    unique by (account)

-- Never retracted: an event that happened stays on the record.
predicate Posting(posting: Subject, account: Subject, amount: Decimal)
    unique by (posting)
    append only

-- The current one of a lineage, replaced through a lineage claim that
-- may not fork.
predicate CurrentPrice(trade: Subject, price_id: Subject)
    current pointer by (trade)
    superseded via PriceSupersedes
predicate PriceSupersedes(successor: Subject, prior: Subject)

-- Versions dated from a day. "The one in force on a date" becomes a
-- definition the declaration writes for you, `terms_in_force_on`, taking
-- the keys, the date, then the other fields in declared order. Without
-- `partial`, a rule must vouch that a version exists wherever one is
-- read; with it, the gaps are intended.
predicate Terms(trade: Subject, version: Subject, qty: Decimal, effective_from: Date)
    effective by (trade) on (effective_from)
predicate Discount(trade: Subject, pct: Decimal, effective_from: Date)
    effective by (trade) on (effective_from) partial
predicate Settled(trade: Subject, on: Date, qty: Decimal)

transformation post(posting, account, amount):
    admit Posting(posting, account, amount)

transformation rebalance(account, old_amount, new_amount):
    require Balance(account, old_amount)
    retract Balance(account, old_amount)
    admit Balance(account, new_amount)

transformation set_price(trade, price_id):
    require not CurrentPrice(trade, _)
    admit CurrentPrice(trade, price_id)

transformation correct_price(trade, prior, successor):
    require CurrentPrice(trade, prior)
    retract CurrentPrice(trade, prior)
    admit CurrentPrice(trade, successor)
    admit PriceSupersedes(successor, prior)

transformation agree_terms(trade, version, qty, effective_from):
    admit Terms(trade, version, qty, effective_from)

transformation discount(trade, pct, effective_from):
    admit Discount(trade, pct, effective_from)

transformation settle(trade, on, qty):
    require Terms(trade, _, _, _)
    admit Settled(trade, on, qty)

invariant settlements_have_terms total over Terms:
    Settled(t, on, _) implies (exists v: Terms(t, v, _, from) and from on_or_before on)

invariant settled_within_terms:
    Settled(t, on, q) and terms_in_force_on(t, on, _, qty) implies q <= qty
```

## Definitions and reads

```morph
program definitions_and_reads

predicate Form(form: Subject, valid_from: Date, valid_to: Date)
predicate Consent(participant: Subject, form: Subject, on: Date)
predicate Enrolled(participant: Subject, on: Date)
predicate Journal(entry: Subject, account: Subject, debit: Decimal, credit: Decimal)
    unique by (entry)
    append only

-- One condition, named once, used by a gate and a rule. A body is
-- context-free: no `actor` and no `pre(...)` inside it.
define consented_on(participant, day):
    Consent(participant, form, given_on) and given_on on_or_before day and Form(form, from, to) and from on_or_before day and day on_or_before to

transformation issue_form(form, valid_from, valid_to):
    admit Form(form, valid_from, valid_to)

transformation consent(participant, form, on):
    admit Consent(participant, form, on)

transformation enrol(participant, on):
    require consented_on(participant, on)
    admit Enrolled(participant, on)

transformation post(entry, account, debit, credit):
    admit Journal(entry, account, debit, credit)

invariant every_enrolment_was_consented:
    Enrolled(p, on) implies consented_on(p, on)

-- A read computed from the claims whenever asked, never stored: the
-- head lists the keys, one row per distinct key tuple the domain binds,
-- and each `value` clause computes a field. At least one is required.
predicate TrialBalance(account: Subject, balance: Decimal)

derived TrialBalance(account):
    over Journal(_, account, _, _)
    value balance = sum(d | Journal(_, account, d, _)) - sum(c | Journal(_, account, _, c))
```

## Three things to know before the second week

**Gate or rule.** A `require` checks the moment of one act against the
authority and figures in force then. A rule re-checks every claim it
reads whenever any of them changes, and a rule the runtime cannot bound
to the claims an act touched is checked over the whole record, so one old
breach can refuse every later act. Ask of each rule: does it govern a
decision at a moment, or must it stay true of the whole record forever?
A limit compared against append-only history is usually the first.
`check -v` names the rules a change may check whole and the construct
responsible, so the question can be asked before the first act.

**Many small claims, never a wider one.** A predicate's shape is part of
every claim ever admitted under it, so a field added later makes every
earlier claim the wrong arity. When a rule needs one more figure, admit
it as a sibling claim keyed the same way, and join them in the rule:

```morph
program sibling

-- The shape the record already holds, with history under it.
predicate Entry(order: Subject, qty: Decimal, price: Decimal)
    unique by (order)
    append only

-- The figure needed later: a sibling keyed by the same subject, not two
-- more fields on `Entry`, and as permanent as the entry it sits beside.
predicate Quote(order: Subject, bid: Decimal, ask: Decimal)
    unique by (order)
    append only

transformation enter(order, qty, price):
    admit Entry(order, qty, price)

transformation quote(order, bid, ask):
    require Entry(order, _, _)
    admit Quote(order, bid, ask)

-- Checks the price where a quote exists; it does not require one.
invariant priced_inside_the_quote:
    Entry(o, _, price) and Quote(o, bid, ask) implies bid <= price <= ask
```

**A reserved word is not a name.** `value`, `from`, `over` and the rest of
the vocabulary cannot name a field; `check` also names the ones the
generated Python client would refuse.

## The tools, by what you are doing

`morpholog check file.morph` parses, validates and hints; `--strict` makes
hints errors; `--json` is the machine shape. `propose` decides and records;
`explain` decides and records nothing, with the same diagnostics, which is
what a dry run and a property test want. `inspect` reads: `claims`,
`derived`, `audit`, `rejections`, `guarantees`, `controls`, `coverage`.
The exact envelopes each prints are in
[embedding Morpholog](embedder-integration.md).
