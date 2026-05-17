# Morpholog

A programming language and runtime for the parts of your business system where you can't afford to be wrong about whether a record was allowed.

The two halves are inseparable. The language only has constructs the runtime can guarantee. The runtime only enforces what the language can express. Together they cover the thing every serious business system needs and almost none of them have: a place where "may this record be admitted as legitimate?" has a definite answer, written once, checked on every change.

You know the kind of records this is for. General ledger entries. Trades booked into a position keeper. Loan disbursements. Anything where an auditor will eventually ask "why is this number what it is, and how do you know?" — and a fuzzy answer will not do.

## The questions you can answer

If you've been close to the books for any length of time, you know this list:

- *Why does this report differ from the one we filed last quarter?*
- *Who admitted this entry, and under what authority?*
- *If that authority was rescinded yesterday, was yesterday's decision still valid?*
- *What did the books say on the last day of Q1, under the close rules in force then?*
- *Did this trade conform to our exposure limits when it was booked? Not now — then?*

Conventional systems answer these with detective work. You search log tables, reconcile parallel systems, ask the person who happened to be on call. Sometimes the answer is good enough. Sometimes the books just stop tying out, and accountants quietly learn to live with that.

Morpholog answers a meaningful subset of these today, against a real PostgreSQL database, through a small CLI. The trial-balance worked example handles question 1 and question 4 directly. The claim-standing worked example handles 2 and 3. Question 5 needs an exposure-limit program that doesn't exist yet, but the runtime would handle it the same way — that is the point of writing the runtime first.

## How it works

There are two first-class constructs. Everything else is built from these.

An **invariant** says what must always be true of admitted records. A **transformation** is the only path that gets to change them. It proposes additions, removals, and outbound notifications; the runtime checks every active invariant against the proposed result; if anything fails, nothing happens. No record written. No notification sent. The state is exactly what it was before you asked.

Here is what that looks like, in the double-entry ledger example (surface syntax is illustrative — the parser is on the roadmap; programs today are constructed as Rust IR):

```
invariant balanced_posted_entry:
    JournalEntry(entry, _, _) implies
        sum { d | JournalLine(entry, _, d, _) }
        == sum { c | JournalLine(entry, _, _, c) }

transformation post_simple_entry(
    entry_id, posting_date, period,
    debit_account, credit_account, amount
):
    require not PeriodClosed(period)
    assert JournalEntry(entry_id, posting_date, period)
    assert JournalLine(entry_id, debit_account, amount, 0)
    assert JournalLine(entry_id, credit_account, 0, amount)
    emit JournalEntryPosted(entry_id)
```

The invariant says debits and credits must balance for every posted entry. Always. The transformation is the only way a journal entry can enter the books, and only when the period is open and the result balances. If the math is off by a penny, the entire transaction is rejected atomically. No half-written ledger, no missing notifications, no "we'll reconcile this in the morning."

## A sixty-second tour

You need PostgreSQL (17+; Morpholog uses PG-specific features). One-time setup:

```bash
createdb my_books
psql my_books -f crates/morpholog-core/sql/schema.sql
export DATABASE_URL=postgres:///my_books
```

Post a journal entry — debit $100 to cash, credit $100 to revenue:

```bash
morpholog propose double_entry_ledger post_simple_entry --args '[
  {"type":"subject","value":"entry_001"},
  {"type":"subject","value":"2026-04-15"},
  {"type":"subject","value":"q1_2026"},
  {"type":"subject","value":"account_cash"},
  {"type":"subject","value":"account_revenue"},
  {"type":"decimal","value":"100"}
]'
```

The runtime checks every invariant, commits the transaction, and prints a receipt:

```json
{
  "status": "committed",
  "transition_id": "019231ab-...-...-...-...-...",
  "asserted_claims": [ /* the journal entry and its two lines */ ],
  "retracted_claims": [],
  "emitted_intents": [ {"name":"JournalEntryPosted","args":[...]} ]
}
```

Write down that `transition_id`. You will want it shortly. Now look at the trial balance:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow
```

```json
[
  {"predicate":"TrialBalanceRow","args":[
    {"type":"subject","value":"account_cash"},
    {"type":"decimal","value":"100"}]},
  {"predicate":"TrialBalanceRow","args":[
    {"type":"subject","value":"account_revenue"},
    {"type":"decimal","value":"-100"}]}
]
```

Post another entry — say, $200 between the same two accounts — and look again. Cash is at 300; revenue at -300. Fine. Now ask for the trial balance *as it stood right after the first transition*:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow \
    --as-of 019231ab-...-...-...-...-...
```

Cash is back at 100. Revenue is back at -100. The report is exactly what an auditor would have seen if they had run it at that exact moment — frozen in time, recomputed from the audit log, with no bitemporal columns or shadow tables anywhere in your schema.

If you had tried to post an unbalanced entry — say, debit 100 against credits totalling 95 — the receipt would have come back as `{"status":"rejected","reason":"balanced_posted_entry invariant did not hold"}`, and your database would look exactly the way it did before you tried. That is the whole point.

## Worked examples

Each runs both in memory (against the kernel) and durably (against PostgreSQL). The integration tests exercise the same audit log the CLI does; nothing is mocked.

- [**Bilateral settlement netting**](examples/01_settlement_netting/) — proves invariants check the *candidate state*, not just the pre-state. A transformation that would create an inconsistency by combining individually-valid inputs is rejected before any commit.
- [**Revenue restatement**](examples/02_revenue_restatement/) — proves contested legitimacy. Historical records survive correction; current-standing pointers move via retraction; supersession lineage is recorded as ordinary claims. Three months from now, the original number is still in the database and still findable.
- [**Claim standing**](examples/03_claim_standing/) — proves admissibility-for-purpose. The same underlying claim can carry different standing for different decisions, granted by different authorities, lost without mutating the underlying claim itself. The shape regulated lending and statutory reporting need.
- [**Double-entry ledger with period close**](examples/04_double_entry_ledger/) — the accounting equation enforced as an invariant; period close as an admission gate; closed periods corrected by restatement that preserves the original record. Hosts the `TrialBalanceRow` derived claim used in the tour above.

Morpholog isn't the whole stack — UIs, dashboards, dataloaders, ML pipelines all stay in the normal tools. What it owns is the line where "may this be admitted as a valid record?" needs a definite answer. That line is a small fraction of any real business system, and the fraction that, when it fails, makes the news. The framing of what Morpholog should grow into, and what it must never become, lives in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md).

---

## Project status

Active. Kernel, PostgreSQL adapter, CLI, and worked examples all work and are tested. Writes scale linearly: about 1.6 seconds per commit against a 100,000-entry ledger. As-of replay is currently quadratic in claim count for asserts-only workloads; the bench surfaced this in the last shipped PR, and the next optimisation will fix it. See [`crates/morpholog-bench/README.md`](crates/morpholog-bench/README.md) for the running performance story.

Not in the box yet: a parser (programs are constructed as Rust IR; the CLI accepts built-in programs only); an outbox worker (rows are enqueued; nobody consumes them yet); user-supplied program loading; materialised derived claims. Each lands when a worked example forces the shape.

To run the tests:

```bash
cargo test -p morpholog-core --all-targets
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --all-targets -- --test-threads=1
```

The workspace splits into `morpholog-core` (synchronous kernel, no I/O), `morpholog-postgres` (async adapter and read helpers), `morpholog-cli` (the `morpholog` binary), and `morpholog-bench` (scale-pressure benchmark).

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) — what Morpholog is for, the language affordances on the roadmap, the three-level expansion ladder, and non-goals. Start here for the design framing.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) — semantics the `morpholog-core` kernel realises.
- [`docs/forced-by-examples.md`](docs/forced-by-examples.md) — retrospective doctrine doc recording, for each significant runtime/IR decision, which worked example forced it and why.
- [`docs/mvp-cut.md`](docs/mvp-cut.md) — decision record for the MVP cut line and the PRs that crossed it.
- Worked examples: [`examples/01_settlement_netting/`](examples/01_settlement_netting/), [`examples/02_revenue_restatement/`](examples/02_revenue_restatement/), [`examples/03_claim_standing/`](examples/03_claim_standing/), [`examples/04_double_entry_ledger/`](examples/04_double_entry_ledger/).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
