# Margin call run

When a trading account is leveraged, the firm holds a deposit - *margin* -
as a cushion against the position moving against it. Each day the account
is re-priced and its *equity* drifts. Two levels matter: the **required**
margin the account is meant to sit at, and a lower **maintenance** floor.
While equity stays above the floor, nothing happens. The moment it drops
below, the firm must issue a **margin call** - a demand to top the account
back up to the required level by the next morning - and the amount is
exactly the gap between where the account is and where it should be.

Every day a risk engine sweeps the whole book, works out which accounts
have fallen through the floor, and produces the day's batch of calls. This
example governs that batch.

The danger it guards against is not a wrong call - it is a **missing** one.
An under-margined account left out of the run is now carrying a losing
position with too little collateral; if it defaults, the firm absorbs the
loss. Whole firms have failed this way in volatile markets. So the control
the model needs is *completeness*, not merely correctness: a rule that only
forbade bad calls would happily wave through a run that forgot half the
book.

This is the example that forces **set-valued proposal**. Earlier examples
hand the runtime one record at a time. Here the risk engine - ordinary,
untrusted software - submits the **whole batch of called accounts as one
value**, and the run is admitted only if that batch is complete and exact.
It is the shape every "an external engine proposes a whole official
snapshot" job shares: settlement runs, payment runs, allocation engines.

## The scenario

The book opens with three accounts:

| Account        | Required | Maintenance floor | Equity | Status         |
| -------------- | -------- | ----------------- | ------ | -------------- |
| `acct_short_a` | 100,000  | 70,000            | 60,000 | below the floor |
| `acct_short_b` | 50,000   | 35,000            | 30,000 | below the floor |
| `acct_ok`      | 100,000  | 70,000            | 90,000 | comfortably above |

The risk engine proposes a run calling `[acct_short_a, acct_short_b]`. It is
admitted: each call is the exact top-up - 40,000 for `acct_short_a`
(`100,000 - 60,000`), 20,000 for `acct_short_b` - and a demand notice is
emitted for each.

Now the failures, each refused atomically, nothing recorded:

- **A forgotten account.** The engine proposes `[acct_short_a]` and leaves
  `acct_short_b` out. The whole run is refused - the completeness gate sees
  an account below its floor that is not in the batch.
- **A healthy account called.** The engine includes `acct_ok`. The run is
  refused - only an account actually short of margin may be called, so the
  run cannot manufacture a demand against a healthy one.

And a calm day works too: a book with nobody below the floor admits a run
that calls no one. Completeness refuses what is *missing*; it never demands
a call that is not owed.

## Completeness, not just correctness

Most of the gallery's controls *forbid* a bad state: a settlement larger
than the cap, an approval without authority. They are exclusions. The
margin run needs something stronger - that the proposed batch *contains*
everything the rule requires. A missing call is the dangerous failure, and
no exclusion rule catches an omission.

The model expresses this as a gate over the whole proposed batch:

> the run may proceed only if there is no account below its floor that is
> absent from the batch.

The batch is the collection the engine submitted. Reading "is this account
in the batch?" against the book the firm already holds is what makes the
omission visible - and refusable - in one decision.

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
- **Proposer-chosen amounts.** Each call here is the derived top-up, so the
  batch carries only account handles. A run where the engine supplies a
  per-line figure of its own would need a structured collection element - a
  different, larger shape than this one forces.
- **Initial vs variation margin, haircuts, cross-margining.** Real margin
  systems are richer; the example keeps to the single control - every short
  account is called, exactly and completely - that the set-valued shape is
  here to teach.
