# Morpholog for developers: a gentle introduction

You know Python. You have built things on an API framework and a SQL database.
And at some point you wrote a function like `validate_invoice()`, then found a
row months later that could only exist because some code path never called it.

This guide is about that problem. In about half an hour you will build a small
program that does something hard to get right in plain Python and SQL: correct a
figure without rewriting the decisions made on it, and answer "what did we know
then?" with one flag.

Morpholog is not a new language for your day job. Your UI, endpoints and
analytics stay where they are. Morpholog does one job: it decides whether a
record may enter the system at all, and it can show why. Use it the way you use
SQL, as a small tool next to the language you already write.

## Three ideas you already half know

**An invariant is a `CHECK` constraint without the ceiling.** A `CHECK`
constraint holds whichever code path writes the row. Morpholog's invariants work
the same way, but they can span many records, do exact decimal arithmetic, and
say "for all", "there exists" and "the sum of". Those are the rules that live in
application code today, and drift.

**A transformation is the only governed door for writes.** You propose a change: some
records to add, some to remove. The runtime checks that everything it touches
still obeys every invariant. If anything would break, nothing is written. There
is no second door, no ORM path that writes its own way. (Raw SQL access to the
tables is the honest exception; [the answer below](#questions-you-are-probably-asking)
says how to close it and how it is caught.)

**Morpholog stores claims, not objects.** There is no `Invoice` class and no row
you `UPDATE`. Morpholog stores the statements your system has accepted: "this
amount was reported for this asset", "this person may approve that". You never
edit a claim. You add new ones and retract old ones, and that history is the
system of record. This is the idea that pays off at the end.

## Setup

Morpholog needs PostgreSQL 18 or later and ships as one binary. [`install.md`](install.md)
covers a fresh machine, PostgreSQL included. To build from a clone instead:

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
cargo install --path crates/morpholog-cli
```

Then make a throwaway database:

```bash
createdb morpholog_intro
export DATABASE_URL=postgres:///morpholog_intro
morpholog init
```

`init` creates Morpholog's own tables from a copy inside the binary. You will
not design a table in this guide.

It also prints a note: the deployment is *open*, so any database login may act
under any name. That is right for a playground. A real deployment closes it, so
each login acts only as the actors it is granted ([closing a
deployment](embedder-integration.md#closing-a-deployment)).

## Your first program

The situation comes from lending. A company borrows against an asset, say a
battery-storage plant. The loan requires the asset's revenue to stay above a
level, so each quarter the company reports its revenue and the bank runs a
*covenant test* on the reported figure. The awkward part: a reported figure can
turn out to be wrong after the bank has already tested it.

Swap the nouns and this is every system that records decisions on figures that
can later change: approvals on invoice amounts, payouts on claim values,
settlements on prices.

The program is one file, `revenue.morph`. To have it all at once, paste
[the complete file](#the-complete-revenuemorph) from the end of this guide and
read along.

### The vocabulary

A `predicate` declares a kind of claim and the shape of its arguments. Think of
them as the tables you would have created, except each names a statement, not a
thing.

```morph
program reported_revenue

predicate Revenue(asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
predicate CurrentFigure(asset: Subject, period: Subject, figure_id: Subject)
predicate Supersedes(new_figure_id: Subject, prior_figure_id: Subject)
predicate CovenantTest(test_id: Subject, asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
```

Read them as sentences:

- `Revenue`: for this asset and period, this amount was reported, as this figure.
- `CurrentFigure`: this figure is the one in force for the asset and period. A
  pointer that moves when a figure is corrected.
- `Supersedes`: this figure replaced that one.
- `CovenantTest`: the bank tested this exact figure, at this amount.

A `Subject` is an opaque identifier, any string to you (`battery_07`, `f1`). A
`Decimal` is exact: `0.1 + 0.2` is `0.3`.

### Why `figure_id`

Your SQL instinct asks: isn't `(asset, period)` enough?

```morph
-- tempting, and wrong for what is coming
predicate Revenue(asset: Subject, period: Subject, amount: Decimal)
```

That shape makes "the Q1 revenue" a slot, one value per asset and period, which
you would update in place. The moment you do, the old figure is gone, and nothing
can say "I relied on *that* one". Giving each reported figure its own id turns
the slot into a series of statements, each one a later decision can point at. A
correction becomes a new figure beside the old one. Everything at the end rests
on this choice.

### Reading a rule

Inside a rule, a claim pattern asks a question. `CurrentFigure(asset, period, f)`
means "is there an admitted claim like this?", and the lowercase names pick up
the matching values.

| Form | Read it as |
|---|---|
| `Foo(a, b)` | is there an admitted `Foo` claim with these arguments? |
| `_` | any value |
| `not Foo(...)` | no matching claim exists |
| `A implies B` | whenever A holds, B must hold too |
| `require name: X` | a gate: this action may go ahead only if X is true now |
| `admit X` / `retract X` | add or remove this claim, if the whole change commits |
| `emit X` | stage a notification, delivered after the commit |

### One rule

```morph
invariant one_figure_in_force_per_period:
    CurrentFigure(asset, period, a) and CurrentFigure(asset, period, b) implies a = b
```

If two claims say a figure is in force for the same asset and period, they are
the same figure: at most one figure is in force at a time. If that looks like a
`UNIQUE` constraint, good. So far Morpholog has shown you nothing SQL cannot do.

### The ways state can change

```morph
transformation report_revenue(asset, period, amount, figure_id):
    require no_figure_yet: not CurrentFigure(asset, period, _)
    admit Revenue(asset, period, amount, figure_id)
    admit CurrentFigure(asset, period, figure_id)
    emit RevenueReported(figure_id)

transformation run_covenant_test(test_id, asset, period, amount, figure_id):
    require figure_exists: Revenue(asset, period, amount, figure_id)
    require in_force: CurrentFigure(asset, period, figure_id)
    admit CovenantTest(test_id, asset, period, amount, figure_id)
    emit CovenantTestRecorded(test_id)
```

`report_revenue` may run only where no figure is in force yet. A replacement
has to arrive as a correction, never by reporting over the top.

`run_covenant_test` has the line the whole story turns on: `in_force`. The bank
may test only the figure in force, and that is checked at the moment it acts.

The rest of the file is a second rule, that a correction chain never forks, and
the correction itself:

```morph
invariant correction_chain_never_forks:
    Supersedes(new_a, old) and Supersedes(new_b, old) implies new_a = new_b

transformation correct_revenue(asset, period, new_amount, new_figure_id, prior_figure_id):
    require prior_exists: Revenue(asset, period, _, prior_figure_id)
    require not_yet_corrected: not Supersedes(_, prior_figure_id)
    admit Revenue(asset, period, new_amount, new_figure_id)
    admit Supersedes(new_figure_id, prior_figure_id)
    retract CurrentFigure(asset, period, prior_figure_id)
    admit CurrentFigure(asset, period, new_figure_id)
    emit RevenueCorrected(new_figure_id, prior_figure_id)
```

(The `intent` lines the `emit`s refer to are in the complete file.)

Check that the program is well-formed:

```bash
morpholog check -v revenue.morph
```
```
ok: revenue.morph
program: reported_revenue
  predicates: 5
  definitions: 0
  invariants: 2
  transformations: 3
  intents: 3
  derived claims: 1
  invariant checks: compiled
  invariant cases: all bound
  body read plan: all keyed
```

Without `-v`, success is silent and a mistake comes back with a caret on its
line. The last three lines say how a proposal is checked: the invariants run as
SQL in the database, each only over the records a change touches, and every read
the actions make is keyed, looking up records by their arguments instead of
scanning a whole predicate.

## Make it happen

**Report Q1 revenue of 1000**, signed off by an analyst, `verifier_anna`. The
arguments are a JSON object (`morpholog schema revenue.morph report_revenue`
gives its JSON Schema). `amount` is a string, because an exact decimal does not
survive being a JSON number.

```bash
morpholog propose revenue.morph report_revenue --actor verifier_anna \
  --args-named '{"asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```

The receipt, trimmed:

```json
{
  "status": "committed",
  "transition_id": "01a1ee9a-41b0-77e9-8955-a808ac5f0db8",
  "actor": { "type": "subject", "value": "verifier_anna" },
  "asserted_claims": [
    { "predicate": "Revenue", "args": [ ... ] },
    { "predicate": "CurrentFigure", "args": [ ... ] }
  ],
  "retracted_claims": [],
  "emitted_intents": [ { "name": "RevenueReported", "args": [ ... ] } ],
  "drawn_subjects": []
}
```

That receipt is the audit record. The commit that wrote the claims wrote it too,
or wrote nothing.

**The bank's credit committee tests that figure:**

```bash
morpholog propose revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_june","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```

It commits. Keep this receipt's `transition_id`: it names the state right after
this commit, and we will come back to it.

```bash
JUNE=01a1ee9a-...   # use the id from your receipt
```

(`--as-of` also takes an RFC 3339 timestamp.) `--actor` records under whose
authority a change was proposed. On this open deployment it is a recorded name.
On a closed one, a login may assert only the actors it is granted.

## The turn

Weeks later the meter data is reconciled, and the real Q1 figure is 1200.

Your instinct is `UPDATE revenue SET amount = 1200`. The moment you run it, you
destroy the answer to a question an auditor will ask: what figure did the June
test rely on? The row now says 1200, and the test looks as if it ran on a number
that did not exist yet.

In SQL you can avoid that, and people do. But by the time the first correction
arrives, the simple table has been in production for a year, so you retrofit:

- a row and an id per figure version, back-filled for the rows already there;
- a covenant test that references the version it used, not "the Q1 figure";
- a current flag or a validity range, with a trigger so two versions are never
  current at once;
- effective-dated queries for "what did the books say in June?";
- and every writer, from the API to the backfill script, bound by all of it.
  One bare `UPDATE` anywhere and the history is quietly wrong.

None of it is exotic. The cost is that you integrate all of it, in every code
path that writes, forever.

## The payoff

**Correct the figure:**

```bash
morpholog propose revenue.morph correct_revenue --actor verifier_anna \
  --args-named '{"asset":"battery_07","period":"q1_2026","new_amount":"1200","new_figure_id":"f2","prior_figure_id":"f1"}'
```

It admits figure f2 and `Supersedes(f2, f1)`, and moves the pointer. The old
figure stays.

**Did the June test survive?** Ask for the state as it stood in June. This small
helper prints one claim per line:

```bash
flat() { jq -r '.[] | "\(.predicate)(\([.args[].value] | join(", ")))"'; }

morpholog inspect claims --as-of "$JUNE" | flat
```
```
Revenue(battery_07, q1_2026, 1000, f1)
CurrentFigure(battery_07, q1_2026, f1)
CovenantTest(covtest_june, battery_07, q1_2026, 1000, f1)
```

And today:

```bash
morpholog inspect claims | flat
```
```
Revenue(battery_07, q1_2026, 1000, f1)
CovenantTest(covtest_june, battery_07, q1_2026, 1000, f1)
CurrentFigure(battery_07, q1_2026, f2)
Revenue(battery_07, q1_2026, 1200, f2)
Supersedes(f2, f1)
```

Both answers are true. The Q1 figure is now 1200. The June test was made on
1000, figure f1, and the record still says so. You wrote no bitemporal schema and
no trigger; the as-of read is one flag.

The correction did retract the old pointer. A retraction takes a claim out of
current standing; it does not erase it. The audit log records it, which is why
the June read can still show the pointer where it stood.

**The hinge.** Try to run a new test against the superseded figure f1:

```bash
morpholog propose revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_august","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```
```json
{ "status": "rejected",
  "rule": "in_force",
  "reason": "require `in_force` failed: CurrentFigure(asset, period, figure_id) did not hold over pre-state",
  "related_parameters": ["asset", "period", "figure_id"],
  "witness": [ ... ] }
```

Refused, by name, and nothing is recorded. `related_parameters` names the
arguments the gate read, so a form can mark the fields to fix. The `in_force`
gate governs what may happen *next*: f1 is no longer in force. It did not reach
back and undo the June test, made while f1 was in force. New reliance on a stale
figure is refused; past reliance stays valid. That is the difference between a
gate and an invariant, and it is the part that is hard to keep honest by hand.

To see why something was refused, ask:

```bash
morpholog explain revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_august","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```
```
Rejected: run_covenant_test(covtest_august, battery_07, q1_2026, 1000, f1) proposed by bank_credit_cttee

Gate not satisfied:
  CurrentFigure(asset, period, figure_id)
  judged under:
    amount = 1000
    asset = battery_07
    figure_id = f1
    period = q1_2026
    test_id = covtest_august

Directly missing claims:
  - CurrentFigure(battery_07, q1_2026, f1)
      candidate supplier transformations:
        - report_revenue
        - correct_revenue
```

It names the missing claim and the actions that could supply it. A program, or
an AI agent proposing changes, can read that and repair its proposal. A service
can get the same account on the refusal itself with `propose --explain-on-reject`.

## Reading state back

Your screens still need data. Narrow a read to the predicates you want, and
decode arguments by field name:

```bash
morpholog inspect claims --predicate CurrentFigure --named revenue.morph
```
```json
[{ "predicate": "CurrentFigure",
   "args": { "asset": "battery_07", "figure_id": "f2", "period": "q1_2026" },
   "asserted_in": "01a1ee9a-..." }]
```

A screen usually wants the joined answer, not the pointer. Declare it next to the
rules:

```morph
predicate CurrentRevenue(asset: Subject, period: Subject, figure_id: Subject, amount: Decimal)

derived CurrentRevenue(asset, period, figure_id):
    over CurrentFigure(asset, period, figure_id)
    value amount = value Revenue(asset, period, _, figure_id)
```

One row per figure in force, carrying its amount. It is computed from the claims
whenever you ask, so it cannot drift from them, and the as-of flag works on it
too:

```bash
morpholog inspect derived revenue.morph CurrentRevenue | flat
morpholog inspect derived revenue.morph CurrentRevenue --as-of "$JUNE" | flat
```
```
CurrentRevenue(battery_07, q1_2026, f2, 1200)
CurrentRevenue(battery_07, q1_2026, f1, 1000)
```

Current state lives in an ordinary table, so everyday reads replay nothing. An
old read rebuilds the state from the audit log, starting at the nearest snapshot
if `morpholog refresh snapshots` keeps them, so its cost need not grow with the
history. For
a BI tool, `morpholog generate views` writes typed SQL views over the same claims
([how](embedder-integration.md#reading-governed-state-as-sql-generate-views)).

## Poke the model

The fastest way to trust a boundary is to try to get past it.

1. **Test the corrected figure,** f2 at 1200. It commits.
2. **Correct f1 again,** to 1300 as f3. Refused by `not_yet_corrected`: the
   chain must not fork.
3. **Report Q1 again.** Refused by `no_figure_yet`: a period with a figure in
   force takes a correction, never a silent replace.
4. **Break the invariant itself.** Every refusal so far came from a gate. Add a
   careless action with the same admits as `report_revenue` and no gate:

   ```morph
   transformation sloppy_report(asset, period, amount, figure_id):
       admit Revenue(asset, period, amount, figure_id)
       admit CurrentFigure(asset, period, figure_id)
       emit RevenueReported(figure_id)
   ```

   Run it on the already-reported period, and the invariant refuses it:

   ```json
   { "status": "rejected", "rule": "one_figure_in_force_per_period",
     "reason": "invariant `one_figure_in_force_per_period` violated", ... }
   ```

   A gate makes a bad proposal hard to attempt. The invariant makes the bad
   state impossible to commit, even for an action someone adds next year. You
   started with a validator some code path forgot to call; here the forgetting
   is harmless. (Delete `sloppy_report` now.)

## What would a different rule have done?

Rules change. Suppose the bank asks for a new policy: a correction of more than
10% needs a second sign-off, so `correct_revenue` alone may not make one. Before
adopting it, you want to know which past decisions it would have refused.

Write the candidate as a copy of the file with the changed gate:

```morph
transformation correct_revenue(asset, period, new_amount, new_figure_id, prior_figure_id):
    bind Revenue(asset, period, prior_amount, prior_figure_id)
    require modest_correction: abs(new_amount - prior_amount) <= prior_amount * 0.1
    ...
```

and rehearse it over the recorded history:

```bash
morpholog rehearse revenue_candidate.morph
```

```json
"totals": { "same": 2, "different_delta": 0, "would_reject": 1, ... },
"refusals": [ { "rule": "modest_correction", "would_refuse": 1,
                "refused_transitions": [ "01a1ee9a-41da-..." ] } ]
```

The candidate is not deployed. It re-decides every recorded proposal through the
ordinary rules, gates included, against the state the history before it builds.
Your 20% correction would have been refused; the report and the June test would
have gone through. `morpholog evaluate` asks the matching question of a candidate
invariant: which committed changes would it have refused?

## Auditing the rules

You can read the rules back as well as the records. That is what a controller
asks first: what does this system forbid, and is any of it dead text?

- `morpholog inspect guarantees revenue.morph` lists the states the model makes
  impossible: here, two figures in force at once, and a forked correction chain.
- `morpholog inspect controls revenue.morph` lists, per action, what must be true
  before it commits: its gates.
- `morpholog inspect coverage revenue.morph` replays the audit log and reports
  which rules have done work (trimmed):

```
invariants:

  one_figure_in_force_per_period - CONSTRAINED: refused 1 proposal(s); the rule has demonstrably done its job
  correction_chain_never_forks - fired in 1 transition(s)

transformations:

  run_covenant_test - 1 transition(s)
    refused: 1 proposal(s)
      require `in_force`: 1
```

The `in_force` line is the August attempt. A rule that has refused a real
proposal is the best evidence it is alive. A rule shown as `never fired` has only
ever been trivially true. That is a reason to look, not proof it does nothing: it
may guard a state no proposal has tried to reach yet.

## Say the rules on the declarations

The two invariants you wrote are shapes that come up everywhere: one current
pointer per key, and a correction chain that never forks. Morpholog lets you
declare them on the predicates instead:

```morph
predicate Revenue(asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
    append only
predicate CurrentFigure(asset: Subject, period: Subject, figure_id: Subject)
    current pointer by (asset, period)
    superseded via Supersedes
```

Revenue figures are permanent; `CurrentFigure` is a pointer, at most one per
asset and period, whose history lives in a `Supersedes` chain that may not fork.
The runtime generates the same rules from these clauses, and `inspect guarantees`
names the clause each came from. `append only` does more than a rule: an action
that retracts a `Revenue` is refused by `check`, before anything runs. This guide
keeps the hand-written form so each rule has a name you chose; the worked examples
use the declarations.

## In your application

Your service drives the same commands. Generate a typed, dependency-free Python
client from the program:

```bash
morpholog generate python-client revenue.morph --out .
```

```python
from decimal import Decimal

from morpholog_client import envelopes, open_client
from morpholog_client.models import CorrectRevenueRequest

client = open_client("revenue.morph", "postgres:///morpholog_intro")
outcome = client.submit(
    CorrectRevenueRequest(
        asset="battery_07",
        period="q1_2026",
        new_amount=Decimal("1300"),
        new_figure_id="f3",
        prior_figure_id="f1",
    ),
    actor="verifier_anna",
)
if isinstance(outcome, envelopes.Rejected):
    print("refused by", outcome.rule)  # refused by not_yet_corrected
```

A refusal is a value, not an exception: your endpoint turns it into a 422 with the
rule attached. The client is stamped with the program's hash and checks it against
the binary, so they cannot speak different contracts. `open_session()` keeps one
process running with the rules loaded; `morpholog serve` holds the rules and the
database logins behind a socket, for callers you do not trust. The contract under
all of it, for any language, is in [`embedder-integration.md`](embedder-integration.md).

The `emit` lines feed an outbox. Each commit queues its notifications in the same
transaction, and a worker delivers them afterwards, so the lender hears of a
correction that committed, and never of one that did not.

## Questions you are probably asking

**"Doesn't this mean writing everything twice?"**
No. Each governed action is written once, to Morpholog. The systems that need it
are kept up to date from the notifications it emits, retried on failure, with keys
that make a repeated delivery harmless. The tables are plain PostgreSQL and can sit
in your application's database, in their own schema.

**"Can't someone bypass the rules with raw SQL?"**
With superuser access, yes, as with any database. Two things limit it. Ordinary
permissions let only Morpholog's role write its tables (`init --least-privilege`).
And the claims and the audit log are two accounts of one history, so `audit verify`
catches an edit that makes them disagree, and one that rewrites both if you have
shared a checkpoint fingerprint outside the database: 32 bytes you can email to an
auditor, or have a timestamp authority sign (`audit checkpoint --witness`). It
protects history up to the last fingerprint you shared.

**"Isn't one generic table of records slow?"**
The table is storage, not the query engine. A rule that compiles to SQL is checked
in the same transaction, over the records a change touches; the in-memory version
is the specification, and a test holds the SQL to it. Reporting uses the generated
SQL views.

**"What about the right to erasure, if nothing is deleted?"**
Keep personal data in an ordinary store you can erase, keyed by an opaque id, and
keep it out of the governed records ([how](embedder-integration.md#personal-data-outside-the-record)).

**"How does a record's shape change once history exists?"**
Declare the new shape and carry the data forward with a governed transformation;
history stays as it was recorded. `rehearse` and `check --database-url` tell you
before you deploy what the new rules make of the old records.

**"Are `Subject` and `Decimal` the only types?"**
No. There are dates with calendar arithmetic, exact timestamps and durations,
amounts tagged with a unit (`Decimal[USD]` never meets `Decimal[t]`), booleans,
domain symbols and collections. Local days in a time zone, such as a gas day from
05:00 London, come from `morpholog generate local-days`, as rows an authority
publishes. Unit conversions are still to come, as admitted claims, never a hidden
lookup table.

**"Does this scale beyond one small file?"**
A `.morph` has no imports, so a rule is read whole. A large program can be split
into files that a `.parts` manifest lists; it is still one program with one hash.
`define` names a condition once for many rules, and `inspect guarantees` and
`explain` keep a large rule set legible: you are handed the rule that fired, with
a caret on its line.

## Where to go next

- The [worked examples](../examples/), each a real domain taught from scratch in
  its `.morph`. [Verified revenue](../examples/02_verified_revenue/) and
  [trade lifecycle](../examples/10_trade_lifecycle/) are the fuller cousins of this
  guide. [Gas day cash-out](../examples/27_gas_day_cash_out/) shows late evidence
  flagging the decisions it affects.
  [Closed-loop execution](../examples/21_closed_loop_execution/) lets an untrusted
  agent act only through a custodied executor.
- [`embedder-integration.md`](embedder-integration.md): driving Morpholog from an
  application, with every command's contract.
- [`runtime-semantics.md`](runtime-semantics.md): what the kernel means, construct
  by construct.
- [`scope-and-ambition.md`](scope-and-ambition.md) and [`roadmap.md`](roadmap.md):
  what Morpholog is for, what it must never become, and what comes next.

### The complete `revenue.morph`

```morph
program reported_revenue

predicate Revenue(asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
predicate CurrentFigure(asset: Subject, period: Subject, figure_id: Subject)
predicate Supersedes(new_figure_id: Subject, prior_figure_id: Subject)
predicate CovenantTest(test_id: Subject, asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
predicate CurrentRevenue(asset: Subject, period: Subject, figure_id: Subject, amount: Decimal)

intent RevenueReported(figure_id: Subject)
intent RevenueCorrected(new_figure_id: Subject, prior_figure_id: Subject)
intent CovenantTestRecorded(test_id: Subject)

invariant one_figure_in_force_per_period:
    CurrentFigure(asset, period, a) and CurrentFigure(asset, period, b) implies a = b

invariant correction_chain_never_forks:
    Supersedes(new_a, old) and Supersedes(new_b, old) implies new_a = new_b

transformation report_revenue(asset, period, amount, figure_id):
    require no_figure_yet: not CurrentFigure(asset, period, _)
    admit Revenue(asset, period, amount, figure_id)
    admit CurrentFigure(asset, period, figure_id)
    emit RevenueReported(figure_id)

transformation correct_revenue(asset, period, new_amount, new_figure_id, prior_figure_id):
    require prior_exists: Revenue(asset, period, _, prior_figure_id)
    require not_yet_corrected: not Supersedes(_, prior_figure_id)
    admit Revenue(asset, period, new_amount, new_figure_id)
    admit Supersedes(new_figure_id, prior_figure_id)
    retract CurrentFigure(asset, period, prior_figure_id)
    admit CurrentFigure(asset, period, new_figure_id)
    emit RevenueCorrected(new_figure_id, prior_figure_id)

transformation run_covenant_test(test_id, asset, period, amount, figure_id):
    require figure_exists: Revenue(asset, period, amount, figure_id)
    require in_force: CurrentFigure(asset, period, figure_id)
    admit CovenantTest(test_id, asset, period, amount, figure_id)
    emit CovenantTestRecorded(test_id)

derived CurrentRevenue(asset, period, figure_id):
    over CurrentFigure(asset, period, figure_id)
    value amount = value Revenue(asset, period, _, figure_id)
```
