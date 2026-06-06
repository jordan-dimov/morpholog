# Morpholog for developers: a gentle introduction

You know Python. You have built things on top of an API framework and a SQL
database. You have, at some point, written a function called something like
`validate_invoice()` - and you have lived with the quiet dread that somewhere,
on some code path, someone forgot to call it.

This guide is about that dread, and a different way to make it go away. It
assumes nothing about Morpholog and asks for about half an hour. By the end you
will have run a small program that does something you would find genuinely
annoying to build correctly in plain Python and SQL - not because the language
is clever, but because it is built around the part of the problem that is
actually hard.

Morpholog is not a new general-purpose language competing for your day job.
Your UI, your dataloaders, your analytics, your HTTP handlers all stay exactly
where they are. Morpholog is a focused tool for one job: the place in a system
where the question *"may this be admitted as a valid record?"* needs a definite,
provable answer. Think of it the way you think of SQL - a small, foreign-looking
thing you reach for one job, used alongside the language you already write.

## The core ideas, in terms you already have

Most of Morpholog's core ideas you already know under different names.

**A SQL `CHECK` constraint is an invariant the database enforces no matter which
code path writes the row.** That is its whole appeal: it does not matter whether
the insert came from your API, a migration, a cron job, or someone typing into
`psql` at midnight - the rule holds, because it lives in the substrate, not in
application code that has to remember to run. Morpholog calls that an
**invariant**, and removes the ceiling SQL puts on it: a Morpholog invariant can
span many rows and many tables, do exact decimal arithmetic, and quantify ("for
all", "there exists", "the sum of") - the rules a `CHECK` constraint cannot
express, the ones that today live in application code and drift.

**A database transaction with exactly one writer is a transformation.** In
Morpholog the *only* way state ever changes is a **transformation**: it proposes
some claims to add and some to remove, the runtime checks every invariant
against the proposed result, and it commits only if nothing breaks. There is no
other door. No `UPDATE` from a forgotten script, no direct write that skips the
checks. If a transformation's result would violate an invariant, the whole thing
is refused and the database is byte-for-byte what it was before.

The remaining idea is the one that has no familiar name, so hold it lightly for
now:
Morpholog stores **claims**, not *entities*. There is no table-of-objects, no
ORM class, no `Invoice` row you `UPDATE` in place. Your application keeps its
domain nouns - invoices, trades, assets - and Morpholog stores the admitted
claims about them: small statements it has agreed to admit because they obey
the rules, the ones your system is prepared to stand behind. A claim is much
like a row; the difference is that you never edit one. You admit new claims and
retract old ones, and the history of what was admitted is the system of record.
That sounds austere. By the end of this guide it will be the thing you did not
know you wanted.

## Setup

Morpholog runs on PostgreSQL 17+ and ships as a single CLI. One-time setup:

```bash
# install the CLI from the repo
cargo install --path crates/morpholog-cli

# a throwaway database to play in
createdb morpholog_intro
psql morpholog_intro -f crates/morpholog-core/sql/schema.sql
export DATABASE_URL=postgres:///morpholog_intro
```

The setup is deliberately explicit - install the CLI, create a disposable
database, load the schema, point the CLI at it - so you can see there is no
magic underneath. The schema you just loaded is Morpholog's own - a claims
table, an audit table, an outbox. You will never design a table in this guide.

## Your first program

We are going to model one small, real situation: a **reported revenue figure
that a bank relies on, and that later turns out to need correcting.** It is the
kind of thing every finance system has, and the kind of thing that, done
casually, quietly corrupts your audit trail.

A Morpholog program is one `.morph` file, and we will build it in sections. If
you would rather have a runnable file from the start, paste
[the complete `revenue.morph`](#the-complete-revenuemorph) from the end of this
guide into your working directory now and read along; the snippets below are
the same file, taken a piece at a time.

### The vocabulary

First, the nouns. A `predicate` declares a *kind of claim* the system can hold,
and the shape of its arguments. These are the only nouns this world knows - the
closest analogue is "the tables you would have created", except each one names a
statement, not a thing.

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

`Subject` is Morpholog's opaque identifier type. It is just an opaque string to
you here (`battery_07`, `q1_2026`, `f1`); the runtime can also mint fresh unique
ones, but supplying your own is fine.

A `Decimal` is arbitrary-precision. There is no `f64` anywhere near your money.

### Why `figure_id`? The modelling move this guide turns on

Pause on that fourth argument to `Revenue`, because your SQL instinct is
already asking the right question: *isn't `(asset, period)` enough?*

```morph
-- tempting, and wrong for what is coming
predicate Revenue(asset: Subject, period: Subject, amount: Decimal)
```

That shape makes "the Q1 revenue" a *slot* - one value per asset and period,
which you would inevitably update in place. And the moment you update it, you
have lost the identity of the figure that used to be there - so nothing else in
the system can ever again say "I relied on *that* one."

Giving every reported figure its own id turns the slot into a series of
admitted statements, each with an identity that a later decision can point at.
A correction becomes a *new* figure that sits beside the old one, never an
overwrite. Everything in the payoff section rests on this one move.

### How to read what follows

Everything below is built from a handful of forms. Decode them once and the
rest reads as a business story:

| Form | Read it as |
|---|---|
| `Foo(a, b)` | a claim pattern - "a Foo claim with these arguments" |
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

`report_revenue` records a figure and points `CurrentFigure` at it (refusing if
one already exists - a fresh figure for an already-reported period goes through
correction, never a silent replace).

`run_covenant_test` is where a gate earns its keep. Look at its two `require`
lines. The first says the figure must exist with that amount. The second says it
must be **the figure currently in force**. The bank may only run a test against
the live figure, and that is checked *at the moment it acts*. Remember this
line; it is the hinge of the whole story.

Here is the rest of the file - the second invariant and the correction
transformation, which we will earn in a moment. Add it, and your `revenue.morph`
is complete:

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

Silence and a zero exit means it parsed and validated - predicate arities,
binding flow, expression shapes, the lot. This is your compiler. Success is
quiet by design (scripts depend on the empty output); when you want the
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

**Report Q1 revenue of 1000 for a battery-storage asset.** Morpholog can tell
you the shape of a transformation's arguments - it is just a JSON Schema:

```bash
morpholog schema revenue.morph report_revenue
```

and you supply the arguments as a matching JSON object (`amount` is a string,
because exact decimals do not survive being a JSON number):

```bash
morpholog run revenue.morph report_revenue --actor verifier_anna \
  --args-named '{"asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```

`--actor` records *under whose authority* this change was proposed; it is written
to the audit row and kept. The receipt comes back as JSON - this is the real
output, lightly trimmed:

```json
{
  "status": "committed",
  "transition_id": "019e937d-0dcf-7f00-b66d-c43a47bd84f8",
  "actor": { "type": "subject", "value": "verifier_anna" },
  "asserted_claims": [
    { "predicate": "Revenue", "args": [ ...battery_07, q1_2026, 1000, f1 ] },
    { "predicate": "CurrentFigure", "args": [ ...battery_07, q1_2026, f1 ] }
  ],
  "retracted_claims": [],
  "emitted_intents": [
    { "name": "RevenueReported", "args": [ ...f1 ] }
  ]
}
```

That receipt is not a log line you hope got written. It *is* the audit record -
the same commit that wrote the claims wrote this, atomically, or wrote nothing.

**Now the bank runs a covenant test against that figure**, in good faith, on
what the books say today:

```bash
morpholog run revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_june","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```

It commits. A `CovenantTest` claim now stands, stamped `1000` and `f1`. Note the
`transition_id` it returns - that moment is "as of June", and we will come back
to it.

## The turn

Weeks later the meter data is reconciled and the real Q1 figure is **1200**, not
1000. You need to correct it.

Here is the fork in the road, and it is worth slowing down for, because it is
the exact moment your instinct as a SQL developer will betray you.

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

You *can* do all this in PostgreSQL. That is the point worth being fair about.
But look at what it costs you, because this is the part nobody budgets for:

- You stop storing the figure as a column you update. You give each figure
  version its own row and id.
- Your `CovenantTest` can no longer reference "the Q1 figure" - it has to
  foreign-key the *specific version* it relied on, or the correction silently
  reaches back and changes what the test was based on.
- You add a `current` flag, or a `valid_from` / `valid_to` pair, and a trigger
  to move it on correction without breaking the one-in-force rule.
- And to answer "what did the books say as of June?" you add bitemporal columns
  and write effective-dated queries against them.

Every one of those is a place to get it subtly wrong, and the classic bug is the
worst kind: it is silent. Someone reruns last quarter's report a year later, the
effective-dating join is off by one boundary condition, and the report
confidently shows 1200 where it should show 1000. Nothing errors. You find out
in an audit.

Now watch Morpholog do the whole thing.

## The payoff

**Correct the figure:**

```bash
morpholog run revenue.morph correct_revenue --actor verifier_anna \
  --args-named '{"asset":"battery_07","period":"q1_2026","new_amount":"1200","new_figure_id":"f2","prior_figure_id":"f1"}'
```

It commits: it admits the new `Revenue` (f2, 1200) and a `Supersedes(f2, f1)`,
retracts `CurrentFigure ... f1`, and admits `CurrentFigure ... f2`. The pointer
has moved. The old figure is untouched.

**Did the bank's June decision survive?** Ask Morpholog for the state exactly as
it stood at that "as of June" transition id you noted. The CLI emits a JSON
array of claim objects; for reading along, flatten it with a one-line `jq`
helper we will reuse for the rest of this guide:

```bash
flat() { jq -r '.[] | "\(.predicate)(\([.args[].value] | join(", ")))"'; }

morpholog inspect claims --as-of 019e937d-0e98-7790-897e-30500c744711 | flat
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

**And the hinge.** Suppose someone now tries to run a *new* covenant test
against the old figure f1 - the one that has been superseded:

```bash
morpholog run revenue.morph run_covenant_test --actor bank_credit_cttee \
  --args-named '{"test_id":"covtest_august","asset":"battery_07","period":"q1_2026","amount":"1000","figure_id":"f1"}'
```
```json
{ "status": "rejected",
  "reason": "require failed: CurrentFigure(asset, period, figure_id) did not hold over pre-state" }
```

Refused. The exit code is non-zero; nothing was written. That second `require`
in `run_covenant_test` - "the figure must be the one in force" - is a **gate**:
it governs what you may do *next*, and f1 is no longer in force. But notice what
the gate did *not* do: it did not reach back and invalidate the June test that
was made when f1 *was* in force. New reliance on a stale figure is forbidden;
past reliance stays valid. That asymmetry - "whether a decision was allowed is
settled when it is made, and stays settled" - is the difference between a gate
and a standing rule, and it is the thing that is genuinely hard to keep honest
by hand.

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
its proposal, and try again.

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
- **There are no entities.** There was no `Revenue` object to mutate. That
  absence is exactly what made `UPDATE`'s sin impossible to commit - there was
  nothing to update.
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

But a screen does not want the pointer - it wants the joined answer: the
asset, the period, the figure in force, and its amount. In SQL this is where
you would write a view, and in a bigger system a read model someone has to
keep in sync. In Morpholog you declare it next to the rules, and it is
recomputed from the admitted claims every time it is asked for - a report
that structurally cannot drift from the state it reports on. Add this to
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
morpholog inspect derived revenue.morph CurrentRevenue --as-of 019e937d-0e98-7790-897e-30500c744711 | flat
```
```
CurrentRevenue(battery_07, q1_2026, f1, 1000)
```

That is last quarter's report, reproduced - not from a snapshot someone
remembered to take, but recomputed from the audit log on demand.

## Where this fits in your stack

Morpholog is not your whole system. You drove it through its CLI just now, with
JSON arguments - and that is exactly how a real service embeds it. Your Python or
TypeScript backend calls the `morpholog` binary as a subprocess: ask `morpholog
schema` for the argument shape, build the request body (`--args-named` takes the
same JSON your schema describes - it looks just like an API request body),
`morpholog run` to commit, `morpholog explain` to find out why a refusal
happened, and the outbox to deliver the notifications each commit emits. No FFI,
no generated client, no Rust toolchain in your app.

In Python, the whole integration is this:

```python
import json
import subprocess

def propose(transformation: str, actor: str, args: dict) -> dict:
    result = subprocess.run(
        ["morpholog", "run", "revenue.morph", transformation,
         "--actor", actor, "--args-named", json.dumps(args)],
        capture_output=True, text=True,
    )
    if result.stderr:
        raise RuntimeError(result.stderr)   # operational failure, not a refusal
    return json.loads(result.stdout)        # committed or rejected, either way a receipt

receipt = propose("report_revenue", "verifier_anna", {
    "asset": "battery_07", "period": "q1_2026",
    "amount": "1000", "figure_id": "f1",
})
if receipt["status"] == "rejected":
    ...  # show receipt["reason"], or ask `morpholog explain` what is missing
```

A business refusal is data, not an exception - which is why the snippet does
not pass `check=True`: a refusal exits non-zero but still writes the structured
receipt to stdout, and `check=True` would raise before you ever read it. Your
endpoint turns the rejection into a 422 with the reason attached. A worked version of exactly this pattern, driving
a commodity-trade lifecycle end to end, lives in
[`../examples/etrm_embedder/`](../examples/etrm_embedder/), with the full
contract in [`embedder-integration.md`](embedder-integration.md).

So the division of labour is: your UI, your analytics, your market data, your
dashboards - all of it stays in the tools you already use. Morpholog owns the one
line where "may this be admitted as a valid record?" needs an answer you can
defend three years later. That is a small fraction of any system. It is also the
fraction that, when it goes wrong, makes the news.

## Poke the model

The fastest way to trust an admission boundary is to try to get past it. A few
things to try with the program you already have:

1. **Run a covenant test against the corrected figure** - `f2`, amount `1200`.
   It commits: new reliance belongs on the figure in force.
2. **Correct `f1` a second time** - say to `1300` as `f3`. Refused: `f1` has
   already been superseded, and the correction chain must not fork.
3. **Report revenue for `battery_07` / `q1_2026` again.** Refused: a current
   figure already exists, so a new figure for an already-reported period must
   go through correction, never a silent replace.

Each refusal traces to a single `require` line you can point at in
`revenue.morph` - and `morpholog explain` will point back.

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
- The project [`README`](../README.md) - the wider pitch and the list of
  questions Morpholog is built to answer.

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
