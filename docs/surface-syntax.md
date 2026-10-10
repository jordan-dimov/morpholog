# The surface syntax on one page

Everything a `.morph` file can say, each form once, in a programme small
enough to read whole. Every block on this page is parsed, validated and
held lint-clean by a test, so what it shows is what the parser accepts.
What each form *means*, and why it is spelled the way it is, is in
[runtime semantics](runtime-semantics.md); this page is for the hand that
is writing the file.

## A programme

A file is one programme: declarations, then rules, then acts. Comments
start with `--`. Indentation is layout: a body is the lines indented under
its head, and parentheses switch layout off, so a long expression may
break across lines inside them.

```morph
program sheet

-- A claim's shape: its fields and their kinds.
predicate Account(account: Subject, owner: Subject, balance: Decimal)
    unique by (account)

-- An intent is what the outside world is asked to do after a commit.
intent AccountOpened(account: Subject)

-- A constant is written out in place before the rules take effect.
const opening_minimum = (100)

invariant accounts_open_in_credit:
    Account(a, _, balance) implies opening_minimum <= balance

transformation open(account, owner, deposit):
    require not Account(account, _, _)
    admit Account(account, owner, deposit)
    emit AccountOpened(account)
```

Kinds: `Subject`, `Decimal`, `Bool`, `Date`, `Timestamp`, `Duration`,
`Collection`, and a quantity `Decimal[USD]` whose unit is a label of your
own. A subject literal is `#name`; a variable is bare. `true` and `false`
are reserved and not yet writable, so a two-way status is a subject,
`#open` or `#closed`.

## Claims and patterns

A pattern matches claims by position, `_` for a field you do not care
about, or by field name with `..` for the rest.

```morph
program patterns

predicate Line(line: Subject, invoice: Subject, net: Decimal, vat: Decimal)
    unique by (line)

transformation post(line, invoice, net, vat):
    admit Line(line, invoice, net, vat)

invariant nothing_negative:
    Line(line, _, net, _) implies 0 <= net

invariant vat_never_exceeds_net:
    Line(line: l, net: n, vat: v, ..) implies v <= n
```

## Acts: the statements of a transformation

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
    -- A gate: a yes or no on the pre-state. It keeps none of what it
    -- matched, so name it and read its bindings in the same line.
    require within_mandate: Mandate(actor, desk, cap) and 0 < qty and qty <= cap
    -- A lookup that must match exactly one claim, and keeps its bindings.
    bind the_desk: Desk(desk)
    -- A value read from one claim, kept for the lines below; a computed
    -- value; and a fresh identifier nobody chose.
    let cap = value Mandate(actor, desk, _)
    let half = min(qty, cap) / 2
    let order = new Subject()
    admit Order(order, actor, half)
    emit Placed(order)

transformation flag(order):
    require Order(order, _, _)
    admit Flagged(order)

transformation unflag(order):
    require Flagged(order)
    retract Flagged(order)

transformation flag_all(orders):
    for o in orders:
        require Order(o, _, _)
        admit Flagged(o)
```

`actor` is whoever proposes; it is read in acts only, never in a rule. A
`require` exports nothing: a variable it matched and a later statement
names again is matched afresh, and `check` says so. To keep a value, `bind`
it or `let` it, as `let cap = value Mandate(actor, desk, _)` does above.

## Rules: invariants

A rule is a proposition the record never lets any change break. The
forms: `implies`, `and`, `or`, `not`, `xor`, `forall x in source: body`,
`exists x: body`, and the comparators.

```morph
program rules

predicate Period(period: Subject, status: Subject)
    unique by (period)
predicate Entry(entry: Subject, period: Subject, amount: Decimal)
    unique by (entry)
    append only
predicate Reviewed(entry: Subject, by: Subject)
predicate Outcome(entry: Subject, verdict: Subject)
    unique by (entry)

transformation post(entry, period, amount):
    require Period(period, _)
    admit Entry(entry, period, amount)

transformation decide(entry, verdict):
    require Entry(entry, _, _)
    admit Outcome(entry, verdict)

invariant entries_are_positive:
    Entry(e, _, amount) implies 0 < amount

invariant large_entries_are_reviewed:
    Entry(e, _, amount) and 10000 <= amount implies (exists r: Reviewed(e, r))

invariant one_verdict_or_the_other:
    Outcome(e, v) implies (v = #accepted xor v = #rejected)

invariant nothing_posts_into_a_closed_period:
    Entry(e, p, _) implies not Period(p, #closed)

invariant bounded_entries:
    Entry(e, _, amount) implies 0 < amount <= 1000000
```

A named `total over P` on a rule promises a `P` exists wherever the rule
needs one; see effective dating below.

## Definitions: one condition, used in several places

```morph
program definitions

predicate Consent(participant: Subject, form: Subject, on: Date)
predicate Form(form: Subject, valid_from: Date, valid_to: Date)
predicate Enrolled(participant: Subject, on: Date)

transformation issue_form(form, valid_from, valid_to):
    admit Form(form, valid_from, valid_to)

transformation consent(participant, form, on):
    admit Consent(participant, form, on)

define consented_on(participant, day):
    Consent(participant, form, _) and Form(form, from, to) and from on_or_before day and day on_or_before to

transformation enrol(participant, on):
    require consented_on(participant, on)
    admit Enrolled(participant, on)

invariant every_enrolment_was_consented:
    Enrolled(p, on) implies consented_on(p, on)
```

A body is context-free: no `actor` and no `pre(...)` inside it; pass the
actor as an argument, wrap the call in `pre(...)`.

## Values

```morph
program values

predicate Rate(tariff: Subject, pence_per_kwh: Decimal)
    unique by (tariff)
predicate Reading(meter: Subject, kwh: Decimal)
    unique by (meter)
predicate Line(line: Subject, meter: Subject, tariff: Subject, net: Decimal, source: Subject)
    unique by (line)
predicate Limit(account: Subject, cap: Decimal)
    unique by (account)
predicate Exposure(position: Subject, account: Subject, amount: Decimal)
    unique by (position)

const penny = (0.01)

transformation set_rate(tariff, pence_per_kwh):
    admit Rate(tariff, pence_per_kwh)

transformation read_meter(meter, kwh):
    admit Reading(meter, kwh)

transformation set_limit(account, cap):
    admit Limit(account, cap)

transformation expose(position, account, amount):
    admit Exposure(position, account, amount)

transformation bill(line, meter, tariff, net, source, proposed):
    let used = if(source = #meter, value Reading(meter, _), proposed)
    let rate = value Rate(tariff, _) default 0
    require net = round(used * rate * penny, penny)
    admit Line(line, meter, tariff, net, source)

invariant exposure_within_limit:
    Limit(a, cap) implies sum(amount | Exposure(_, a, amount)) <= cap

invariant largest_position_within_limit:
    Limit(a, cap) implies max(amount | Exposure(_, a, amount)) <= cap

invariant exposure_is_bounded_either_way:
    Exposure(p, _, amount) implies abs(amount) <= 1000000 and min(amount, 0) <= 0
```

Arithmetic is `+ - * / %` over decimals, same-unit quantities, and the
date and time forms below. `sum(target | body)` adds a target over every
binding of the body and is the typed zero when nothing matches;
`max(target | body)` and `min(target | body)` pick an end. `value P(args)`
reads one field of one claim, `_` marking the field wanted, with a
`default` for when there is none. `if(test, then, otherwise)` evaluates
only the branch it selects.

## Dates, instants, durations, spans and quantities

```morph
program time

predicate Contract(contract: Subject, signed_on: Date, starts_at: Timestamp, notice: Duration, cargo: Decimal[t])
    unique by (contract)
predicate Delivery(delivery: Subject, contract: Subject, delivered_at: Timestamp, tonnes: Decimal[t])
    unique by (delivery)
predicate Invoice(invoice: Subject, contract: Subject, issued_on: Date, charging_year: Decimal)
    unique by (invoice)

const epoch = (@2000-04-01)

transformation sign(contract, signed_on, starts_at, notice, cargo):
    admit Contract(contract, signed_on, starts_at, notice, cargo)

transformation deliver(delivery, contract, delivered_at, tonnes):
    -- `bind` keeps what it matched; a `require` would not.
    bind Contract(contract, _, starts_at, notice, cargo)
    require starts_at at_or_before delivered_at
    require delivered_at - starts_at no_shorter_than notice
    require tonnes <= cargo
    admit Delivery(delivery, contract, delivered_at, tonnes)

transformation invoice(invoice, contract, issued_on):
    bind Contract(contract, signed_on, _, _, _)
    require signed_on on_or_before issued_on
    require issued_on before signed_on + span(P1Y)
    let year = 2000 + period_index(epoch, span(P1Y), issued_on)
    admit Invoice(invoice, contract, issued_on, year)

invariant delivered_within_the_hold:
    Contract(c, _, _, _, cargo) implies sum(t | Delivery(_, c, _, t)) <= cargo

invariant notice_is_at_least_a_day:
    Contract(c, _, _, notice, _) implies duration(PT24H) no_longer_than notice
```

Literals: a date `@2026-05-22`, an instant `@2026-10-24T14:00:00Z` (always
UTC), a duration `duration(PT6H)`, a calendar span `span(P3M)` (months then
days, month ends clamped; expressions only, never stored), a quantity
`25000 USD` or `0 t`. The comparators are spelled per kind: decimals and
quantities `<= < >= >`; dates `on_or_before`, `before`, `on_or_after`,
`after`; instants `at_or_before`, `strictly_before`, `at_or_after`,
`strictly_after`; durations `no_longer_than`, `shorter_than`,
`no_shorter_than`, `longer_than`; and `a <= x <= b` chains. A date minus a
date is days as a decimal; an instant minus an instant is a duration.
`period_index(anchor, span, at)` and `period_start_of(anchor, span, index)`
count periods from an anchor.

## Disciplines: what a declaration promises

```morph
program disciplines

-- One claim per key: the keys determine the whole claim.
predicate Balance(account: Subject, amount: Decimal)
    unique by (account)

-- Never retracted: an event that happened stays on the record.
predicate Posting(posting: Subject, account: Subject, amount: Decimal)
    unique by (posting)
    append only

-- The current one of a lineage: at most one per key, replaced through
-- the named lineage claim, which may not fork.
predicate CurrentPrice(trade: Subject, price_id: Subject)
    current pointer by (trade)
    superseded via PriceSupersedes
predicate PriceSupersedes(successor: Subject, prior: Subject)

-- Versions dated from a day: "the one in force on a date" is a
-- definition the declaration writes for you, here `terms_in_force_on`,
-- taking the keys, the date, then the other fields in declared order.
predicate Terms(trade: Subject, version: Subject, qty: Decimal, effective_from: Date)
    effective by (trade) on (effective_from)
predicate Settled(trade: Subject, on: Date, qty: Decimal)

transformation post(posting, account, amount):
    admit Posting(posting, account, amount)

transformation set_balance(account, amount):
    admit Balance(account, amount)

transformation agree_terms(trade, version, qty, effective_from):
    admit Terms(trade, version, qty, effective_from)

transformation set_price(trade, price_id):
    require not CurrentPrice(trade, _)
    admit CurrentPrice(trade, price_id)

transformation correct_price(trade, prior, successor):
    require CurrentPrice(trade, prior)
    retract CurrentPrice(trade, prior)
    admit CurrentPrice(trade, successor)
    admit PriceSupersedes(successor, prior)

transformation settle(trade, on, qty):
    require Terms(trade, _, _, _)
    admit Settled(trade, on, qty)

invariant settlements_have_terms total over Terms:
    Settled(t, on, _) implies (exists v: Terms(t, v, _, from) and from on_or_before on)

invariant settled_within_terms:
    Settled(t, on, q) and terms_in_force_on(t, on, _, qty) implies q <= qty
```

A declaration may carry several clauses. `effective by` with `partial`
says the gaps are intended, and no rule need vouch for them.

## Reads: derived views

```morph
program reads

predicate Journal(entry: Subject, account: Subject, debit: Decimal, credit: Decimal)
    unique by (entry)
    append only

transformation post(entry, account, debit, credit):
    admit Journal(entry, account, debit, credit)

predicate TrialBalance(account: Subject, balance: Decimal)

derived TrialBalance(account):
    over Journal(_, account, _, _)
    value balance = sum(d | Journal(_, account, d, _)) - sum(c | Journal(_, account, _, c))
```

A view is computed from the claims whenever asked, never stored. Its head
lists the keys, one row per distinct key tuple the `over` domain binds,
and each `value` clause computes a field. At least one `value` clause is
required.

## Idioms worth knowing before the second week

- **Many small claims, never a wider one.** A predicate's shape is part of
  every claim ever admitted under it, so a field added later makes every
  earlier claim the wrong arity. When a rule needs one more figure, admit
  it as a sibling claim keyed the same way, `Quote(order, bid, ask)` beside
  `Entry`, and join them in the rule.
- **Gate or rule.** A `require` checks the moment of one act against the
  authority and figures in force then; an invariant re-checks every
  record it reads whenever any of them changes. A limit compared against
  append-only history belongs in the gate of the act that admits the
  record, unless re-checking history is what you mean; `check` says which
  you wrote.
- **A rule reads no clock and no actor.** A date a rule needs is a field of
  a claim; who may act is a `require` in the act.
- **A reserved word is not a name.** `value`, `from`, `over` and the rest
  of the vocabulary cannot name a field; `check` names the ones the
  generated Python client would refuse too.

## The tools, by what you are doing

`morpholog check file.morph` parses, validates and hints; `--strict` makes
hints errors; `--json` is the machine shape. `propose` decides and records;
`explain` decides and records nothing, with the same diagnostics, which is
what a dry run and a property test want. `inspect` reads: `claims`,
`derived`, `audit`, `rejections`, `guarantees`, `controls`, `coverage`.
The exact envelopes each prints are in
[embedding Morpholog](embedder-integration.md).
