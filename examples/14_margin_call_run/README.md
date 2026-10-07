# Margin call run

**A risk engine may calculate the whole margin run. Morpholog does not have
to trust it.** The run is accepted only if every account that should be
called is in it, no account that should not be called is, and every amount
is exact.

## Why this matters

When a trading account is leveraged, the firm holds a deposit - *margin* -
as a cushion against the position moving against it. Each day the account
is re-priced and its *equity* drifts. Two levels matter: the **required**
margin the account is meant to sit at, and a lower **maintenance** floor.
The moment equity drops below the floor, the firm must issue a **margin
call**: a demand to top the account back up to the required level, for
exactly the gap.

A risk engine sweeps the whole book each day and produces the day's batch
of calls. The dangerous failure is not a wrong call but a **missing** one.
An under-margined account left out of the run carries a losing position
with too little collateral, and if it defaults the firm absorbs the loss.
A check that only forbids bad calls would wave through a run that forgot
half the book.

## What Morpholog enforces

- **Completeness.** The run may proceed only if no account below its floor
  is absent from it.
- **Only real calls.** Every account in the run must actually be below its
  floor.
- **Exact amounts.** Every call is the required level minus equity, and no
  account is called twice in a run.

The engine submits the run as one proposal, and the whole run is accepted
or refused as one decision.

## What it refuses

The book opens with three accounts:

| Account        | Required | Maintenance floor | Equity | Status         |
| -------------- | -------- | ----------------- | ------ | -------------- |
| `acct_short_a` | 100,000  | 70,000            | 60,000 | below the floor |
| `acct_short_b` | 50,000   | 35,000            | 30,000 | below the floor |
| `acct_ok`      | 100,000  | 70,000            | 90,000 | comfortably above |

A run calling `[acct_short_a, acct_short_b]` is accepted: 40,000 for
`acct_short_a` (`100,000 - 60,000`), 20,000 for `acct_short_b`, and a demand
notice for each. Each of these is refused whole, with nothing recorded:

- **A forgotten account.** `[acct_short_a]` alone: an account below its
  floor is missing from the run.
- **A healthy account called.** Adding `acct_ok`: it is not short, so no
  demand can be made against it.

A calm day works too: with nobody below the floor, a run that calls no one
is accepted. Completeness refuses what is missing; it never demands a call
that is not owed.

## What you can show afterwards

Every run in the record was complete and exact against the book as it
stood when the engine proposed it. The record names who proposed each run,
and any past day's book and calls can be rebuilt as they stood.

## Where it fits

The risk engine keeps doing what it does: pricing the book and deciding
whom to call. It proposes the run through Morpholog instead of writing it.
The demand notices (`MarginCallIssued`) reach the systems that send them
only after the run is accepted. The same shape fits any job where an
external engine proposes a whole official batch: settlement runs, payment
runs, allocation engines.

## The program

### Claims

- `RequiredMargin(account, level)`, `MaintenanceMargin(account, floor)`,
  `AccountEquity(account, equity)` - the book as it stands when the run is
  proposed, each `unique by (account)`. Money carries its unit,
  `Decimal[USD]`, so the runtime will not let dollars be compared with
  anything that is not also dollars.
- `MarginRun(run, as_of)` - the run header.
- `MarginCall(run, account, amount)` - one demand, `unique by (run, account)`
  so an account cannot be called twice in a run.

### Invariants

- `call_amount_is_the_top_up` - wherever a call exists, its amount is
  exactly `required - equity`. A call for a made-up number cannot be
  admitted.
- The `unique by` disciplines above, lowered to enforced rules.

### Transformations

- `issue_margin_run(run_id, as_of_date, called_accounts)` - `called_accounts`
  is the whole list of accounts being called, submitted as one collection.
  A completeness gate refuses the run if any below-floor account is missing
  from the list; then, for each account in the list, a per-account gate
  refuses any account that is not actually short, the call amount is
  computed as the top-up, the call is recorded, and a demand notice is
  emitted.

## How to run it

```
morpholog check    examples/14_margin_call_run/margin_call_run.morph
morpholog inspect controls examples/14_margin_call_run/margin_call_run.morph
```

`inspect controls` renders the completeness gate in plain terms - "may
commit only when ... no account below its floor is absent from the batch" -
the control an auditor would want named.

The behaviour is pinned in `crates/morpholog-examples/tests/margin_call_run.rs`:
a complete run is admitted with exact amounts; a run that omits a short
account is refused; a run that calls a healthy account is refused; a calm
book admits an empty run.

## What this example deliberately does not cover

- **Where the book comes from.** `RequiredMargin`, `MaintenanceMargin`, and
  `AccountEquity` are read as given. In a real deployment they are kept
  current by their own governed processes (positions marked to market,
  collateral posted); modelling that daily refresh is a separate concern.
- **Book completeness is a separate control.** The completeness gate sees
  only accounts that *have* their margin claims: an account missing, say, its
  `MaintenanceMargin` is invisible to it. So this example governs
  completeness of the call run *given a complete book* - that every account
  with its figures is called - not that every account has figures in the
  first place. That second control belongs to whatever process maintains the
  book.
- **Proposer-chosen amounts.** Each call here is the derived top-up, so the
  batch carries only account handles. A run where the engine supplies a
  per-line figure of its own would need a structured collection element - a
  different, larger shape than this one forces.
- **Initial vs variation margin, haircuts, cross-margining.** Real margin
  systems are richer; the example keeps to the single control - every short
  account is called, exactly and completely - that the set-valued shape is
  here to teach.
