# Morpholog for developers: a gentle introduction

You know Python. You have built things on top of an API framework and a SQL
database. And somewhere along the way you have written a function like
`validate_invoice()` - then, months later, found a row in the database that
could only have got there because some code path never called it.

This guide is about that problem, and a different way to make it go away. It
assumes you know nothing about Morpholog and asks for about half an hour of
your time. By the end you will have run a small program that does something
genuinely hard to build well in plain Python and SQL. Not because Morpholog is
clever - because it is built around the hard part of the problem.

One thing to settle up front: Morpholog is not a new language for your day
job. Your UI, your endpoints, your dataloaders, your analytics all stay where
they are. Morpholog does one job - it decides whether a record may enter the
system at all, and it can prove why. Think of it like SQL: a small tool you
reach for one thing, used next to the language you already write.

## The core ideas, in terms you already have

Most of Morpholog's core ideas you already know under different names.

**A SQL `CHECK` constraint is a rule the database enforces no matter which
code path writes the row.** That is its whole appeal. It does not matter where
the insert came from - your API, a migration, a cron job, someone typing into
`psql` at midnight. The rule holds, because it lives in the database, not in
application code that has to remember to run. Morpholog calls that an
**invariant**, and removes the ceiling SQL puts on it. An invariant can span
many rows and many tables. It can do exact decimal arithmetic. It can say "for
all", "there exists", "the sum of". Those are the rules a `CHECK` constraint
cannot express - the ones that live in application code today, and drift.

**The database has only one door for writes.** In Morpholog, state
changes one way and one way only: you propose a change - some records to add,
some to remove - and the runtime checks every invariant against the result. If
anything would break, nothing happens, and the database is byte-for-byte what
it was before. Morpholog calls that proposal a **transformation**. There is no
other door. No `UPDATE` from a forgotten script, no code path that skips the
checks.

The last idea is the one with no familiar name, so hold it lightly for now.
Morpholog stores **claims**, not objects. There is no `Invoice` class, no row
you `UPDATE` in place. Your application keeps its nouns - invoices, trades,
assets. What Morpholog stores is the statements your system has accepted about
them, the ones it is prepared to stand behind: "this amount was reported for
this asset", "this person may approve that". A claim is much like a row, with
one difference: you never edit one. You add new claims and retract old ones,
and that history is the system of record. If this sounds strange now, that is
fine - it is the part that pays off at the end of this guide.

## Setup

Morpholog runs on PostgreSQL 18+ and ships as a single CLI. One-time setup:

```bash
# install the CLI from the repo
cargo install --path crates/morpholog-cli

# a throwaway database to play in
createdb morpholog_intro
export DATABASE_URL=postgres:///morpholog_intro
morpholog init
```

Four explicit steps, no magic. `init` provisions Morpholog's own schema - a
claims table, an audit table, an outbox - from a copy embedded in the binary
itself, so there is nothing else to download and nothing to drift. You will
never design a table in this guide.

## Your first program

We are going to model one small, real situation, from the world of lending.

A company borrows money against an asset - say a battery-storage plant. The
loan terms require the asset's revenue to stay above an agreed level, so each
quarter the company reports its revenue and the bank checks the loan terms
against it. That check is called a *covenant test*, and lenders run them on
reported figures all the time. The awkward part: a reported figure can turn
out to be wrong after the bank has already run its check.

Swap the nouns and this is every system that records decisions made on figures
that can later change - approvals against invoice amounts, payouts against
claim values, settlements against prices. Handled casually, the correction
quietly corrupts your audit trail.

A Morpholog program is one `.morph` file, and we will build it in sections. If
you would rather have a runnable file from the start, paste
[the complete `revenue.morph`](#the-complete-revenuemorph) from the end of this
guide into your working directory now and read along; the snippets below are
the same file, taken a piece at a time.

### The vocabulary

A `predicate` declares a *kind of claim* the system can hold, and the shape of
its arguments. Think of these as the tables you would have created - except
each one names a statement, not a thing.

```morph
program reported_revenue

predicate Revenue(asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
predicate CurrentFigure(asset: Subject, period: Subject, figure_id: Subject)
predicate Supersedes(new_figure_id: Subject, prior_figure_id: Subject)
predicate CovenantTest(test_id: Subject, asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
```

Read these as English statements:

- `Revenue(asset, period, amount, figure_id)` - "for this asset in this period,
  this amount was reported, under this figure id."
- `CurrentFigure(asset, period, figure_id)` - "this is the figure currently in
  force for this asset and period." A pointer that will move when we correct.
- `Supersedes(new, prior)` - "this figure replaced that one." The correction
  lineage.
- `CovenantTest(test_id, asset, period, amount, figure_id)` - "the bank ran a
  covenant test, on this exact figure, for this amount." A decision - stamped
  with the precise figure it relied on.

`Subject` is Morpholog's identifier type. To you it is just an opaque string
(`battery_07`, `q1_2026`, `f1`); the runtime can also mint fresh unique ones,
but supplying your own is fine.

A `Decimal` is an exact number - as many digits as needed, never a float. The
famous `0.1 + 0.2 != 0.3` surprise cannot happen to an amount of money here.

### Why `figure_id`? The modelling move this guide turns on

Pause on that fourth argument to `Revenue`, because your SQL instinct is
already asking the right question: *isn't `(asset, period)` enough?*

```morph
-- tempting, and wrong for what is coming
predicate Revenue(asset: Subject, period: Subject, amount: Decimal)
```

That shape makes "the Q1 revenue" a *slot*: one value per asset and period,
which you would inevitably update in place. And the moment you update it, the
old figure is gone. Nothing in the system can ever again say "I relied on
*that* one."

Giving every reported figure its own id changes the picture. The slot becomes
a series of statements, each with an identity a later decision can point at.
A correction becomes a *new* figure beside the old one, never an overwrite.
Everything in the payoff section rests on this one move.

### How to read what follows

Everything below is built from a handful of forms. The most important one is
the *claim pattern*: inside a rule, writing `CurrentFigure(asset, period, f)`
does not create anything. It asks "is there an admitted claim like this?" -
and the lowercase names pick up the matching values. The rest decode in a line
each:

| Form | Read it as |
|---|---|
| `Foo(a, b)` | is there an admitted `Foo` claim with these arguments? |
| `_` | any value; I do not care which |
| `not Foo(...)` | no matching claim exists |
| `A implies B` | whenever A holds, B must hold too |
| `require X` | a gate: this action may proceed only if X is true right now |
| `admit X` | add this claim - if the whole change commits |
| `retract X` | remove this claim - if the whole change commits |
| `emit X` | stage an outbound notification, delivered after commit |

### One familiar rule

Now a rule. Add this:

```morph
invariant one_figure_in_force_per_period:
    CurrentFigure(asset, period, a) and CurrentFigure(asset, period, b) implies a = b
```

Read `implies` as "whenever the left is true, the right must be too." So: if two
claims both say a figure is current for the same asset and period, they must be
the same figure. In plain words - *at most one figure is in force at a time.*

If this feels like a `UNIQUE` constraint, good. It is doing the same job, and at
this point Morpholog has shown you nothing SQL cannot do. Hold that thought.

We will add one more rule that keeps the correction history from forking, but it
is the same flavour, so let us get to the interesting part.

### The ways state can change

Now the transformations. Each `admit`s claims, `retract`s claims, and `emit`s a
notification; a `require` is a gate checked at the moment of the action.

```morph
transformation report_revenue(asset, period, amount, figure_id):
    require not CurrentFigure(asset, period, _)
    admit Revenue(asset, period, amount, figure_id)
    admit CurrentFigure(asset, period, figure_id)
    emit RevenueReported(figure_id)

transformation run_covenant_test(test_id, asset, period, amount, figure_id):
    require Revenue(asset, period, amount, figure_id)
    require CurrentFigure(asset, period, figure_id)
    admit CovenantTest(test_id, asset, period, amount, figure_id)
    emit CovenantTestRecorded(test_id)
```

Take `report_revenue`'s gate first. `require not CurrentFigure(asset, period, _)`
reads as: "there must be no figure currently in force for this asset and
period - whatever figure that might be." So you can report revenue only where
no figure stands yet. That is deliberate: the *first* figure arrives through
`report_revenue`, and a replacement must arrive through correction - never by
quietly reporting over the top of what is there. Pass the gate, and the
transformation records the figure and points `CurrentFigure` at it.

`run_covenant_test` is where a gate earns its keep. Look at its two `require`
lines. The first says the figure must exist with that amount. The second says it
must be **the figure currently in force**. The bank may only run a test against
the live figure, and that is checked *at the moment it acts*. Remember this
line; it is the hinge of the whole story.

Here is the rest of the file - the second invariant and the correction
transformation, which will make sense in a moment. Add it, and your
`revenue.morph` is complete:

```morph
invariant correction_chain_never_forks:
    Supersedes(new_a, old) and Supersedes(new_b, old) implies new_a = new_b

transformation correct_revenue(asset, period, new_amount, new_figure_id, prior_figure_id):
    require Revenue(asset, period, _, prior_figure_id)
    require not Supersedes(_, prior_figure_id)
    admit Revenue(asset, period, new_amount, new_figure_id)
    admit Supersedes(new_figure_id, prior_figure_id)
    retract CurrentFigure(asset, period, prior_figure_id)
    admit CurrentFigure(asset, period, new_figure_id)
    emit RevenueCorrected(new_figure_id, prior_figure_id)
```

(If you are building the file up section by section, also copy the `intent`
declarations the `emit` lines refer to from the complete file at the end. An
`intent` is an outbound notification staged at commit and delivered
afterward - ignore them for now.)

Before running anything, check that the program is well-formed:

```bash
morpholog check revenue.morph
```

Silence and a zero exit means it parsed and validated - argument counts,
unbound variables, expression shapes, the lot. This is your compiler, and it
behaves like one: any mistake comes back with a caret pointing at the exact
line and statement in your source, not just a description. (Tooling can ask
for the same findings as data with `check --json`.) Success is quiet by
design, because scripts depend on the empty output. When you want the
reassurance, ask for it:

```bash
morpholog check -v revenue.morph
```
```
ok: revenue.morph
program: reported_revenue
  predicates: 4
  invariants: 2
  transformations: 3
  intents: 3
  derived claims: 0
```

## Make it happen

**First, report Q1 revenue of 1000 for the battery plant.** The figure is
signed off by an analyst - call her `verifier_anna` - and proposed under her
name. Morpholog can tell you the shape of a transformation's arguments - it is
just a JSON Schema:

```bash
morpholog schema revenue.morph report_revenue
```

and you supply the arguments as a matching JSON object (`amount` is a string,
because exact decimals do not survive being a JSON number):

```bash
morpholog propose revenue.morph report_revenue --actor verifier_anna \
  --args-named '{"asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```

`--actor` records *under whose authority* this change was proposed; it is written
to the audit row and kept. The receipt comes back as JSON. Here it is with the
first claim shown in full and the rest elided (`...`) to save space - every
`args` array has the same shape, one tagged value per argument:

```json
{
  "status": "committed",
  "transition_id": "019e937d-0dcf-7f00-b66d-c43a47bd84f8",
  "actor": { "type": "subject", "value": "verifier_anna" },
  "asserted_claims": [
    {
      "predicate": "Revenue",
      "args": [
        { "type": "subject", "value": "battery_07" },
        { "type": "subject", "value": "q1_2026" },
        { "type": "decimal", "value": "1000" },
        { "type": "subject", "value": "f1" }
      ]
    },
    { "predicate": "CurrentFigure", "args": [ ... ] }
  ],
  "retracted_claims": [],
  "emitted_intents": [
    { "name": "RevenueReported", "args": [ ... ] }
  ]
}
```

That receipt is not a log line you hope got written. It *is* the audit record -
the same commit that wrote the claims wrote this, atomically, or wrote nothing.

**Now the bank's credit committee (`bank_credit_cttee`) runs its covenant test
against that figure**, in good faith, on what the books say today:

```bash
morpholog propose revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_june","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```

It commits. A `CovenantTest` claim now stands, stamped `1000` and `f1`.

One more thing before moving on: copy the `transition_id` from the receipt and
stash it, because that moment is "as of June" and we will come back to it:

```bash
JUNE=019e937d-0e98-7790-897e-30500c744711   # yours will differ - use your receipt's id
```

Every commit returns a `transition_id` - its exact coordinate in the audit
log. It is an id rather than a date because a date is ambiguous: many things
can commit in one day. The id names one precise moment - the state immediately
after that particular commit. (When an instant is what you have, `--as-of`
also accepts an RFC 3339 timestamp and resolves it to the last commit at or
before it; the id remains the precise coordinate.)

(In real life the bank lives in its own systems, of course. What we are
modelling is the governed record on the asset's side: the bank's decision
*enters as a claim*, proposed under the bank's authority - that is what
`--actor` is recording. One Morpholog instance is one party's system of
record, not a ledger shared between organisations. And `--actor` is recorded
provenance, not authentication: verifying who is calling, before proposing in
their name, is your service's job. When authority itself must be *enforced* -
"only holders of this role may confirm" - that is a gate, and the
approval-controls worked example is built around exactly that.)

## The turn

Weeks later the meter data is reconciled and the real Q1 figure is **1200**, not
1000. You need to correct it.

Here is the fork in the road. It is worth slowing down, because this is
exactly where SQL habits lead you astray.

Your instinct is `UPDATE revenue SET amount = 1200 WHERE ...`. And the instant
you type that `UPDATE`, you have destroyed the answer to a question an auditor
*will* ask: *what figure did the bank's June covenant test actually rely on?*
The row now says 1200. The test looks like it was run against a number that did
not exist when it ran. You have not corrected history; you have rewritten it.

So you do not `UPDATE`. You correct the honest way: leave the original figure
exactly where it is, admit the new one beside it, move the "current" pointer, and
record that one supersedes the other. That is precisely what `correct_revenue`
does - and notice it never touches the `CovenantTest`.

## The wall you would have hit in SQL

Let's be fair: you *can* do all this in PostgreSQL. People do. But first
notice how it actually plays out, because nobody designs for corrections on
day one. The simple `revenue` table ships and runs quietly for a year. Then
the first correction request arrives - against live data, with decisions
already recorded on figures that are about to change. Now you retrofit, and
the retrofit looks like this:

- You stop storing the figure as a column you update. Each figure version
  gets its own row and id - and the rows already in production need ids too.
- Your `CovenantTest` can no longer reference "the Q1 figure". It has to
  foreign-key the *specific version* it relied on, or the correction silently
  reaches back and changes what the test was based on.
- You add a `current` flag, or a `valid_from` / `valid_to` pair, and a trigger
  to move it on correction without ever letting two versions be current at
  once.
- To answer "what did the books say as of June?" you add bitemporal columns
  and write effective-dated queries against them.
- And every one of these disciplines now binds *every* writer - the API, the
  admin panel, the month-end backfill script, the migration someone runs by
  hand. One bare `UPDATE` from any of them and the version history is quietly
  wrong. This is the guide's opening problem again, multiplied.

None of this is exotic - the warehouse world calls part of it slowly changing
dimensions, the SQL standard has temporal tables. The cost is not inventing the
techniques; it is that you are now the integrator of all of them at once,
forever, in every code path that writes, and each is a place to get it silently
wrong. Someone reruns last quarter's report a year later, the effective-dating
join is off by one boundary condition, and it confidently shows 1200 where it
should show 1000. Nothing errors. You find out in an audit.

Now watch Morpholog do the whole thing.

## The payoff

**Correct the figure:**

```bash
morpholog propose revenue.morph correct_revenue --actor verifier_anna \
  --args-named '{"asset":"battery_07","period":"q1_2026","new_amount":"1200","new_figure_id":"f2","prior_figure_id":"f1"}'
```

It commits: it admits the new `Revenue` (f2, 1200) and a `Supersedes(f2, f1)`,
retracts `CurrentFigure ... f1`, and admits `CurrentFigure ... f2`. The pointer
has moved. The old figure is untouched.

**Did the bank's June decision survive?** Ask Morpholog for the state exactly
as it stood at the moment you stashed in `$JUNE`. The claims come back as a
JSON array; to read along comfortably, here is a small shell helper that
prints one claim per line. You do not need to read its insides - only its
output - and we will reuse it for the rest of this guide:

```bash
flat() { jq -r '.[] | "\(.predicate)(\([.args[].value] | join(", ")))"'; }

morpholog inspect claims --as-of "$JUNE" | flat
```
```
Revenue(battery_07, q1_2026, 1000, f1)
CurrentFigure(battery_07, q1_2026, f1)
CovenantTest(covtest_june, battery_07, q1_2026, 1000, f1)
```

That is June, reconstructed: figure f1 at 1000 was in force, and the bank's test
rested on it. Now ask for the state **today** (the rows come back in admission
order, which happens to tell the story by itself):

```bash
morpholog inspect claims | flat
```
```
Revenue(battery_07, q1_2026, 1000, f1)                       <- the original, still here
CovenantTest(covtest_june, battery_07, q1_2026, 1000, f1)    <- the June decision, untouched
CurrentFigure(battery_07, q1_2026, f2)                       <- the pointer has moved
Revenue(battery_07, q1_2026, 1200, f2)                       <- the correction, beside it
Supersedes(f2, f1)                                           <- the lineage
```

Both answers are true. *What is the Q1 figure now?* 1200. *What did the bank
decide on in June, and against what?* 1000, figure f1 - still a faithful record
of what was decided that day. The correction moved future reliance to the new
figure without rewriting a single thing about the past. You did not design a
bitemporal schema. You did not write a trigger. The as-of query is one flag.

You may have noticed something, though: `correct_revenue` *retracted* a
claim - the old `CurrentFigure` pointer - and this guide promised that history
is never rewritten. Both are true, and the two outputs above are the proof.
Retraction removes a claim from *current* standing; it does not erase it. The
retraction is itself recorded in the audit log, which is exactly why the as-of
view can still show the pointer where it stood in June. Nothing is ever
deleted. Things stop being current.

**And the hinge.** Suppose someone now tries to run a *new* covenant test
against the old figure f1 - the one that has been superseded:

```bash
morpholog propose revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_august","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```
```json
{ "status": "rejected",
  "reason": "require failed: CurrentFigure(asset, period, figure_id) did not hold over pre-state" }
```

Refused. The exit code is non-zero; nothing was admitted - no claims
changed, no audit row. (The refusal itself lands in an operational
rejection log, so `inspect coverage` can later show which rules have
actually said no.) That second `require`
in `run_covenant_test` - "the figure must be the one in force" - is a **gate**:
it governs what you may do *next*, and f1 is no longer in force. But notice what
the gate did *not* do: it did not reach back and invalidate the June test that
was made when f1 *was* in force. New reliance on a stale figure is forbidden;
past reliance stays valid. Whether a decision was allowed is settled when it
is made, and stays settled. That is the difference between a gate and a
standing rule - and it is the part that is genuinely hard to keep honest by
hand.

When something is refused, you do not have to guess why. Ask:

```bash
morpholog explain revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_august","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```
```
Rejected: run_covenant_test(covtest_august, battery_07, q1_2026, 1000, f1) proposed by bank_credit_cttee

Gate not satisfied:
  CurrentFigure(asset, period, figure_id)

Directly missing claims:
  - CurrentFigure(battery_07, q1_2026, f1)
      candidate supplier transformations:
        - report_revenue
        - correct_revenue
```

That is not a string you grepped out of a stack trace. It is the exact missing
claim and the named transformations that could supply it - structured enough
that a program (or an AI agent proposing changes) can read the refusal, repair
its proposal, and try again. And when the caller is a service rather than a
person at a terminal, you do not need the separate command:
`morpholog propose --explain-on-reject` attaches this same account to the
rejection receipt itself, computed against the very state that refused.

## What just happened

Step back and notice what you did *not* write. No bitemporal schema. No
version-tracking trigger. No `validate_revenue()` that some code path could skip.
No reconciliation job to catch the cases the validation missed. Everything you
did write fits on one screen - and it bought you correction without overwrite,
point-in-time reconstruction, an audit trail that is the system of record
rather than a sidecar, and a refusal that explains itself.

Now the core ideas have names you have *felt*:

- **State is admitted claims, not facts.** You never overwrote the figure
  because you never had a figure to overwrite - only claims about it, each
  admitted at a moment, none ever rewritten. "Correction" was new claims beside
  old ones, which is why the past stayed answerable.
- **There are no entities.** There was no `Revenue` object to mutate. The
  destructive `UPDATE` was impossible to write, because there was nothing to
  update.
- **Invariants and gates do different jobs.** An invariant (`implies`) is a
  rule about what must always be true. A gate (`require`) governs a single
  action and never looks back. Putting "the figure must be current" in a gate,
  not an invariant, is the whole reason the June test survived the July
  correction.

## Reading state back

Your application still has screens to render - "current revenue, by asset and
period", say. Morpholog gives you both the raw read and the declared view.

The raw read you have already used; it also narrows to exactly the predicates
you ask for, which is how a service fetches governed state without dumping
everything:

```bash
morpholog inspect claims --predicate CurrentFigure
```

And if positional args feel fragile to consume from a service, add
`--named revenue.morph` and they come back keyed by the field names you
declared:

```bash
morpholog inspect claims --predicate CurrentFigure --named revenue.morph
```
```json
[{ "predicate": "CurrentFigure",
   "args": { "asset": "battery_07", "period": "q1_2026", "figure_id": "f2" } }]
```

But a screen does not want the pointer. It wants the joined answer: asset,
period, the figure in force, its amount. In SQL you would write a view - or,
in a bigger system, a read model someone has to keep in sync. In Morpholog you
declare it next to the rules. It is recomputed from the claims every time you
ask, so it cannot drift from the state it reports on. Add this to
`revenue.morph`:

```morph
predicate CurrentRevenue(asset: Subject, period: Subject, figure_id: Subject, amount: Decimal)

derived CurrentRevenue(asset, period, figure_id):
    over CurrentFigure(asset, period, figure_id)
    value amount = value Revenue(asset, period, _, figure_id)
```

Read it as: one row per in-force pointer, carrying the amount of the figure
it points at - the `_` marks the slot being extracted. (`morpholog check -v`
now reports `derived claims: 1`.) Ask for the view:

```bash
morpholog inspect derived revenue.morph CurrentRevenue | flat
```
```
CurrentRevenue(battery_07, q1_2026, f2, 1200)
```

And because the view is computed from claims, the time travel you already met
applies to reports too:

```bash
morpholog inspect derived revenue.morph CurrentRevenue --as-of "$JUNE" | flat
```
```
CurrentRevenue(battery_07, q1_2026, f1, 1000)
```

That is last quarter's report, reproduced - not from a snapshot someone
remembered to take, but recomputed from the audit log on demand.

If you are now wondering what that costs: less than you would think, and not
where you would think. Current state lives in an ordinary table, so everyday
reads - the screens, the views you just saw - never replay anything. Only the
*historical* question replays, the cost is linear in the length of the audit
log, and the replay is scoped to the predicates the view actually touches
(measured: about a second and a half through a hundred thousand commits).
And consider what the alternative costs today: "what did the books say in
June?" is usually a week of forensics, not a second and a half.

### Reading governed state as SQL

Your BI tool and your reporting stack speak SQL, not a CLI. So Morpholog hands
them SQL. `generate views` emits a script of typed views - one per predicate,
plus one per declared derived read - over a read-only schema:

```bash
morpholog generate views revenue.morph
```
```sql
-- View for base predicate `Revenue`.
CREATE OR REPLACE VIEW "morpholog_views"."revenue" AS ...
-- View for derived predicate `CurrentRevenue`.
CREATE OR REPLACE VIEW "morpholog_views"."current_revenue" AS ...
```

The base-predicate views read live claims directly. The derived views read a
cache the kernel fills, because a derived value (the exact decimal, the
nanosecond instant) is computed by the kernel and SQL must not become a second
evaluator that rounds it differently. You populate the cache out of band:

```bash
morpholog refresh derived revenue.morph
```
```
refreshed 1 derived claim(s) from 1 derived predicate(s)
  model: sha256:3056ed72...
```

Each refresh stamps the rules' hash, and the derived view shows rows only for a
cache built from the *same* rules - so a stale or mismatched refresh reads as
empty, never as a wrong number. The discipline holds on both sides: the kernel
is the only thing that evaluates, and SQL only projects what it produced.

## Where this fits in your stack

Morpholog is not your whole system. You drove it through its CLI just now,
with JSON arguments - and that is exactly how a real service embeds it. Your
Python or TypeScript backend calls the `morpholog` binary as a subprocess.
`morpholog schema` tells you the argument shape. `--args-named` takes the same
JSON the schema describes - it looks just like an API request body.
`morpholog propose` commits, `morpholog explain` tells you why something was
refused, and the outbox delivers the notifications each commit emits. No FFI,
no Rust toolchain in your app.

Which brings us back to those `emit` lines you have been ignoring. Remember
that the bank lives in its own systems - so when the figure was corrected,
something had to tell the lender. That is what `RevenueCorrected` is for. Each
commit queues its emitted intents in the outbox, inside the same atomic
commit, and a worker delivers them to the outside world afterward. So a
notification never goes out for a change that did not commit, and a change
that commits never loses its notification.

In Python, the whole integration is this:

```python
import json
import subprocess

def propose(transformation: str, actor: str, args: dict) -> dict:
    result = subprocess.run(
        ["morpholog", "propose", "revenue.morph", transformation,
         "--actor", actor, "--args-named", json.dumps(args)],
        capture_output=True, text=True,
    )
    if not result.stdout.strip():
        raise RuntimeError(result.stderr)   # operational failure, not a refusal
    return json.loads(result.stdout)        # committed or rejected, either way a receipt

receipt = propose("report_revenue", "verifier_anna", {
    "asset": "battery_07", "period": "q1_2026",
    "amount": "1000", "figure_id": "f1",
})
if receipt["status"] == "rejected":
    ...  # show receipt["reason"] (propose with --explain-on-reject and the
         # receipt carries the full missing-evidence account too)
```

A business refusal is data, not an exception. That is why the snippet does not
pass `check=True`: a refusal exits non-zero but still writes the receipt to
stdout, and `check=True` would raise before you ever read it. The
discrimination rule is the one load-bearing line: every *decided* result
arrives on stdout (stderr may carry advisory lines, like the rule's source
location on a refusal); **empty stdout** is the only operational failure. Your endpoint
turns the rejection into a 422 with the reason attached. A worked version of
exactly this pattern, driving a commodity-trade lifecycle end to end, lives in
[`../examples/etrm_embedder/`](../examples/etrm_embedder/), with the full
contract in [`embedder-integration.md`](embedder-integration.md).

That snippet is plumbing, and it reads like plumbing. So you do not have to
write it: the binary generates a typed client from your own programme.

```bash
morpholog generate python-client revenue.morph --out ./revclient
```
```
generated ./revclient/morpholog_client (3 transformations, 5 predicates, 3 intents)
```

What lands is a small, dependency-free package - a request model per
transformation, envelope parsing, refusals as values - stamped with the hash of
the rules it was built against, so the client and the binary cannot silently
speak different contracts. The subprocess call above is still what runs
underneath; the generated client is the ergonomic face over the same stable
JSON, regenerated whenever the rules change.

So the division of labour is: your UI, your analytics, your market data, your
dashboards stay in the tools you already use; Morpholog owns the one line where
"may this be admitted as a valid record?" needs an answer you can defend three
years later.

## Poke the model

The fastest way to trust an admission boundary is to try to get past it. A few
things to try with the program you already have:

1. **Run a covenant test against the corrected figure** - `f2`, amount `1200`.
   It commits: new decisions rest on the figure in force.
2. **Correct `f1` a second time** - say to `1300` as `f3`. Refused: `f1` has
   already been superseded, and the correction chain must not fork.
3. **Report revenue for `battery_07` / `q1_2026` again.** Refused: a current
   figure already exists, so a new figure for an already-reported period must
   go through correction, never a silent replace.

4. **The big one: try to break the invariant itself.** Every refusal so far
   came from a gate. Now add a deliberately careless transformation to
   `revenue.morph` - same admits as `report_revenue`, but no gate:

   ```morph
   transformation sloppy_report(asset, period, amount, figure_id):
       admit Revenue(asset, period, amount, figure_id)
       admit CurrentFigure(asset, period, figure_id)
       emit RevenueReported(figure_id)
   ```

   Run it against the already-reported period, and this time the *invariant*
   refuses it:

   ```json
   { "status": "rejected", "reason": "invariant `one_figure_in_force_per_period` violated" }
   ```

   (And on stderr, a courtesy line points at the violated rule's exact
   location in your source: `rule at revenue.morph:<line>:<col>`.)

   This is the lesson the other pokes only hint at. A gate makes a bad
   proposal hard to attempt; the invariant makes the bad state impossible to
   commit - even for a careless transformation someone adds next year. The
   rules do not depend on every transformation remembering its checks. You
   started this guide with a validator some code path forgot to call. Here is
   the forgetting, made harmless. (Delete `sloppy_report` afterwards - it has
   made its point.)

The gate refusals each trace to a `require` line you can point at in
`revenue.morph`; the sloppy one traces to the invariant itself - and
`morpholog explain` points at both kinds.

## Auditing the rules

So far you have read the *records* back. You can also read the *rules* back -
the question an auditor or a controller asks before trusting the system at all:
what does it forbid, what does it check, and is any of it dead text? Reading
the rules back answers that, straight off the same rules that decide admission.

`inspect guarantees` lists what the model makes impossible:

```bash
morpholog inspect guarantees revenue.morph
```
```
Guarantees of `reported_revenue` - states this model makes impossible:

  one_figure_in_force_per_period
    rule: (CurrentFigure(asset, period, a) and CurrentFigure(asset, period, b)) implies (a = b)
  ...
```

`inspect controls` turns it around: per transformation, what must already be
true before the action commits (its gates).

```bash
morpholog inspect controls revenue.morph
```
```
  report_revenue may commit only when:
    - not CurrentFigure(asset, period, _)
      consults: CurrentFigure

  correct_revenue may commit only when:
    - Revenue(asset, period, _, prior_figure_id)
      consults: Revenue
    - not Supersedes(_, prior_figure_id)
      consults: Supersedes
```

Where a gate pre-checks the very condition an invariant enforces, controls
draws the line between them - naming which front-line gate *front-loads* which
standing rule, and the failure each guards against. (This model's gates guard
different ground than its invariants, so no link is drawn here; the biometric
example shows one where the verification gate front-loads its standing
invariant.)

`inspect coverage` replays the whole audit log and reports which rules have
ever actually done work. After the report and the correction you ran:

```bash
morpholog inspect coverage revenue.morph
```
```
  one_figure_in_force_per_period - fired in 2 transition(s)
  correction_chain_never_forks - fired in 1 transition(s)

  report_revenue - 1 transition(s)
  correct_revenue - 1 transition(s)
  run_covenant_test - never used
```

`run_covenant_test` shows `never used` because you have not run a covenant
test yet (that is the first poke below). A transformation or rule that has
never done any work is dead text wearing a load-bearing name - and now it gets
named, instead of sitting in the source looking enforced. (Replay reports what
*has* happened, never what *could*: it cannot prove a rule will never fire,
only that it has not.)

## Say the rules on the declarations

The invariants you hand-wrote in this guide were the right way to learn them -
you now know exactly what each one means. But look at what they
*are*: "one current figure per asset and period" is a uniqueness rule, and
"the correction chain never forks" is a property of the supersession chain.
Rules of that shape are so common that Morpholog lets you declare them on the
predicates themselves:

```morph
predicate Revenue(asset: Subject, period: Subject, amount: Decimal, figure_id: Subject)
    append only
predicate CurrentFigure(asset: Subject, period: Subject, figure_id: Subject)
    current pointer by (asset, period)
    superseded via Supersedes
```

Those clauses say, in order: revenue figures are permanent records - nothing
may ever retract one (corrections supersede, as you built); `CurrentFigure`
is a *pointer* - at most one per asset and period, and retractable, because
pointers must move; and its history lives in `Supersedes`, whose chain may
never fork. With these clauses in place, the hand-written invariants
could be deleted: the runtime generates the same rules from the declarations
(run `morpholog inspect guarantees` and you will see them listed, each with a
`from:` line naming the clause it came from). The `append only` clause goes
one better than a runtime rule - a transformation that tries to `retract` a
`Revenue` row is refused when you `check` the file, before anything runs.

This guide's programme keeps the hand-written form so every rule you met has
a name you chose. The worked examples in the repository use the declaration
form throughout - and when your own programmes grow past a handful of rules,
so should you. (The same instinct - name the idea, not the plumbing - has a
second tool: `define` lets you name a condition once and use it from several
gates and invariants. The clinical-trial example reads as five named
conditions instead of one twenty-line gate.)

## Questions you are probably asking

The broader pitch questions - the dual-write worry, raw `psql` access, whether
a generic claims table is an EAV trap, GDPR erasure, and why not OPA, Datomic,
or Datalog - are answered in the
[README's common questions](../README.md#common-questions). What this guide
raises specifically:

**"Are `Subject` and `Decimal` really the only types?"**
No - this guide's example just never needed more. There are dates with date
comparison (the clinical-trial example gates enrolment on validity windows),
exact timestamps and durations with exact arithmetic (the laytime example
computes deadlines by shifting an instant and sums interval lengths against an
allowance), unit-tagged amounts (`Decimal[USD]`, `Decimal[t]` - the runtime
refuses to add or compare across units, so money never meets tonnes by
accident), booleans, enum-like domain symbols, and collections. Genuinely
missing today: calendar arithmetic on civil dates, timezone-aware local time,
and unit conversions - each planned to enter as admitted claims from an
authority you choose, never as a hidden runtime lookup table.

**"Does this scale beyond one small file?"**
The programmes are deliberately small so far, and `.morph` has no imports or
namespaces yet - they arrive when a real codebase forces them. What exists now
for keeping a rule set legible: named conditions (`define`) keep large gates
readable, claim disciplines put the structural rules on the declarations
(the previous section), `morpholog inspect guarantees` lists what a model
makes impossible, `morpholog check` flags suspicious-but-legal shapes as
hints, and `morpholog explain` turns any refusal into the exact failing rule.
Debugging is not grepping a thousand global rules; it is being handed the one
that fired, with a caret on its line.

## Where to go next

- The [worked examples](../examples/) - each models a real domain (settlement
  netting, double-entry bookkeeping, approval authority, sanctions screening,
  asset-backed lending, a commodity trade lifecycle) and each `.morph` file is
  written to teach its domain from scratch. The
  [trade lifecycle](../examples/10_trade_lifecycle/) and
  [verified revenue](../examples/02_verified_revenue/) examples are the fuller
  cousins of what you just built.
- [`runtime-semantics.md`](runtime-semantics.md) - what the kernel means, and the
  full surface-to-IR mapping if you want to know what every keyword lowers to.
- [`embedder-integration.md`](embedder-integration.md) - when you are ready to
  drive Morpholog from an application rather than a terminal: the pinned
  contract this guide's `propose()` sketch was secretly following, batch
  import for many transitions in one call, and
  `morpholog generate python-client` - one command that emits a complete,
  typed, dependency-free Python client for your own programme, so the
  subprocess plumbing is generated rather than written.
- The project [`README`](../README.md) - the wider pitch and the list of
  questions Morpholog is built to answer.
- [`scope-and-ambition.md`](scope-and-ambition.md) and
  [`roadmap.md`](roadmap.md) - what Morpholog is for, what it must never
  become, and what is coming next.

### The complete `revenue.morph`

This includes the read-side view from "Reading state back", so a fresh
`morpholog check -v` on it reports `derived claims: 1`.

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
    require not CurrentFigure(asset, period, _)
    admit Revenue(asset, period, amount, figure_id)
    admit CurrentFigure(asset, period, figure_id)
    emit RevenueReported(figure_id)

transformation correct_revenue(asset, period, new_amount, new_figure_id, prior_figure_id):
    require Revenue(asset, period, _, prior_figure_id)
    require not Supersedes(_, prior_figure_id)
    admit Revenue(asset, period, new_amount, new_figure_id)
    admit Supersedes(new_figure_id, prior_figure_id)
    retract CurrentFigure(asset, period, prior_figure_id)
    admit CurrentFigure(asset, period, new_figure_id)
    emit RevenueCorrected(new_figure_id, prior_figure_id)

transformation run_covenant_test(test_id, asset, period, amount, figure_id):
    require Revenue(asset, period, amount, figure_id)
    require CurrentFigure(asset, period, figure_id)
    admit CovenantTest(test_id, asset, period, amount, figure_id)
    emit CovenantTestRecorded(test_id)

derived CurrentRevenue(asset, period, figure_id):
    over CurrentFigure(asset, period, figure_id)
    value amount = value Revenue(asset, period, _, figure_id)
```
