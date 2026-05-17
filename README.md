# Morpholog

There comes a moment in every serious business when someone - an auditor, a regulator, a board director who has not been having a great quarter - asks you to *prove* a number. Why is it what it is. Who decided it should be. What it looked like before the restatement. Whether the rule it had to satisfy actually held at the moment it was admitted.

In most systems, those answers come from detective work. You pull logs, reconcile parallel files, ask the people who happened to be on call. Sometimes the answer is good enough. Sometimes the books just stop tying out, and everyone learns to live with that.

Morpholog is a programming language and runtime that takes those questions out of detective territory. You write down the rules your records have to satisfy. The language only lets you express rules the runtime can guarantee, and the runtime checks every one on every change. Anything that breaks a rule never gets in. Everything that does get in carries enough provenance that you can reconstruct exactly what the books said at any past point in the audit log - no bitemporal columns, no shadow tables, no overnight reconciliation scripts.

The point is to make *"how do you know?"* answerable by *"because the system could not have admitted it otherwise."*

## The questions you can answer

- *Why does this report differ from the one we filed last quarter?*
- *Who admitted this entry, and under what authority?*
- *If that authority was rescinded yesterday, was yesterday's decision still valid?*
- *What did the books say on the last day of Q1, under the close rules in force then?*
- *Did this trade conform to our exposure limits when it was booked? Not now - then?*

Morpholog answers a meaningful subset of these today, against a real PostgreSQL database, through a small CLI. The trial-balance worked example handles question 1 and question 4 directly. The claim-standing worked example handles 2 and 3. Question 5 needs an exposure-limit program that doesn't exist yet, but the runtime would handle it the same way - that is the point of writing the runtime first.

## How it works

There are two first-class constructs. Everything else is built from these.

An **invariant** says what must always be true of admitted records. A **transformation** is the only path that gets to change them. It proposes additions, removals, and outbound notifications; the runtime checks every active invariant against the proposed result; if anything fails, nothing happens. No record written. No notification sent. The state is exactly what it was before you asked.

Here is what that looks like, in the double-entry ledger example (surface syntax is illustrative - the parser is on the roadmap; programs today are constructed as Rust IR):

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

Post a journal entry - debit $100 to cash, credit $100 to revenue:

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

Post another entry - say, $200 between the same two accounts - and look again. Cash is at 300; revenue at -300. Fine. Now ask for the trial balance *as it stood right after the first transition*:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow \
    --as-of 019231ab-...-...-...-...-...
```

Cash is back at 100. Revenue is back at -100. The report is exactly what an auditor would have seen if they had run it at that exact moment - frozen in time, recomputed from the audit log, with no bitemporal columns or shadow tables anywhere in your schema.

If you had tried to post an unbalanced entry - say, debit 100 against credits totalling 95 - the receipt would have come back as `{"status":"rejected","reason":"balanced_posted_entry invariant did not hold"}`, and your database would look exactly the way it did before you tried. That is the whole point.

## What you get for writing your rules this way

The as-of tour is the most visceral feature, but it is not the only one and probably not the most important. The list a controller or a risk officer might actually care about:

- **Atomic commit or full rollback.** A transformation either lands every change it proposed - claims, audit row, outbound notifications - or none of them. There is no half-written ledger, no orphaned audit row, no notification fired against a state that never actually committed. PostgreSQL's `SERIALIZABLE` isolation does the work; the runtime guarantees it.
- **An append-only audit of every change.** Every committed transformation writes a row carrying the transformation name, the arguments, the asserted and retracted claims, the emitted notifications, the invariants that governed admission, and a UUIDv7-timestamped transition id. The record of how a number got to be what it is, three months from now, is the row that wrote it.
- **Read-side projections governed by the same rules.** A trial balance, an exposure summary, an account balance - these are derived claims, computed from admitted state on demand. They cannot reflect a record an invariant rejected. The report cannot drift from the source.
- **Correction without overwriting.** When a record needs to change, the original stays admitted; a new transformation asserts the correction and a `Supersedes` claim records the lineage. Auditors three quarters later can still see the original number, the corrected number, and the moment one became the other.
- **Notification staging that respects the commit boundary.** Outbound side effects (a wire dispatch instruction, a webhook fire, a Kafka publish) stage as outbox rows at commit and deliver afterward through a worker. Side effects never run inside the database transaction; they never run if the commit rolled back.
- **Arbitrary-precision decimal arithmetic throughout.** No floating-point drift in financial quantities. Adds, subtracts, sums - all exact.
- **Admissibility-for-purpose without mutation.** The same underlying claim can carry different standing for different decisions, granted and revoked by different authorities, without ever modifying the underlying record. Regulated lending and statutory reporting need exactly this shape; the third worked example demonstrates it end-to-end.

## Worked examples

Each runs both in memory (against the kernel) and durably (against PostgreSQL). The integration tests exercise the same audit log the CLI does; nothing is mocked.

- [**Bilateral settlement netting**](examples/01_settlement_netting/) - proves invariants check the *candidate state*, not just the pre-state. A transformation that would create an inconsistency by combining individually-valid inputs is rejected before any commit.
- [**Revenue restatement**](examples/02_revenue_restatement/) - proves contested legitimacy. Historical records survive correction; current-standing pointers move via retraction; supersession lineage is recorded as ordinary claims. Three months from now, the original number is still in the database and still findable.
- [**Claim standing**](examples/03_claim_standing/) - proves admissibility-for-purpose. The same underlying claim can carry different standing for different decisions, granted by different authorities, lost without mutating the underlying claim itself. The shape regulated lending and statutory reporting need.
- [**Double-entry ledger with period close**](examples/04_double_entry_ledger/) - the accounting equation enforced as an invariant; period close as an admission gate; closed periods corrected by restatement that preserves the original record. Hosts the `TrialBalanceRow` derived claim used in the tour above.

Morpholog isn't the whole stack - UIs, dashboards, dataloaders, ML pipelines all stay in the normal tools. What it owns is the line where "may this be admitted as a valid record?" needs a definite answer. That line is a small fraction of any real business system, and the fraction that, when it fails, makes the news. The framing of what Morpholog should grow into, and what it must never become, lives in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md).

---

## Project status

Active. Kernel, PostgreSQL adapter, CLI, and worked examples all work and are tested. Writes scale linearly: about 1.6 seconds per commit against a 100,000-entry ledger. As-of replay also scales linearly: about 1.5 seconds to reconstruct state from a 100,000-transition audit log (was quadratic until recently; a `ReplaySet` working set replaced the linear-scan dedupe loop). See [`crates/morpholog-bench/README.md`](crates/morpholog-bench/README.md) for the running performance story.

Not in the box yet: a parser (programs are constructed as Rust IR; the CLI accepts built-in programs only); an outbox worker (rows are enqueued; nobody consumes them yet); user-supplied program loading; materialised derived claims. Each lands when a worked example forces the shape.

Built in Rust on modern PostgreSQL (17+). The kernel is `#[forbid(unsafe_code)]`; the PG adapter leans on SERIALIZABLE isolation (SSI) and JSONB so an entire commit - `claims`, `audit`, and `outbox` rows - lands atomically in one transaction or not at all.

To run the tests:

```bash
cargo test -p morpholog-core --all-targets
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --all-targets -- --test-threads=1
```

Existing databases need the migrations under `crates/morpholog-core/sql/migrations/` applied in numeric order. Fresh installations get the head schema from `crates/morpholog-core/sql/schema.sql`.

The workspace splits into `morpholog-core` (synchronous kernel, no I/O), `morpholog-postgres` (async adapter and read helpers), `morpholog-cli` (the `morpholog` binary), and `morpholog-bench` (scale-pressure benchmark).

## Where this is heading

The runtime today is operationally complete enough to defend a number; the next arc is making it operationally complete enough to defend an *organisation*. The roadmap, framed in business shapes rather than feature names:

- **An outbox worker plus compensating transformations.** Outbound notifications (wire dispatch, webhook fire, Kafka publish) need a consumer. When delivery fails non-retryably - the SEPA network rejects a wire because of an out-of-band AML lock, say - the runtime should reconcile through a *compensating transformation* that goes back through every invariant gate and writes its own audit row. The ledger does not pretend the original commit never happened; it records the external contradiction and the correction as further governed facts. This is the *Morpholog plus an Outside Coordinator* architecture: the runtime stays a strict local gatekeeper; the worker owns the asynchronous conversation with the world. Sketched in [`docs/outbox-sketch.md`](docs/outbox-sketch.md); first substrate PR landing now.
- **Actor authority and approval limits.** "Who admitted this entry, and under what authority?" is one of the questions the runtime promises to answer; closing it fully requires `ApprovalAuthorityFor(actor, predicate_pattern, limit)`-shaped claims and an invariant that gates admission on the actor's standing. A worked example is the right forcing function.
- **Effective time as a first-class temporal axis.** As-of evaluation already gives knowledge time (what did we believe at moment T). Effective time (the day a contract becomes binding; the period a posting reflects) is expressible as ordinary claims; combining the two gives full bitemporal addressability without ever introducing `valid_from`/`valid_to` columns to any schema.
- **A surface syntax and parser.** Programs are constructed as Rust IR today; the CLI accepts built-in programs only. A parser commits to file layout, module system, error spans, literal syntax, and a dozen other things that should be ratified by a real outside user, not pre-decided. The parser arrives when an outside collaborator is genuinely blocked by its absence - not before.
- **Materialised derived claims.** Trial balance and similar reads are recomputed on demand today. For long audit logs and frequent queries, materialised snapshots will become forced. The benchmark is the regression test that will reveal when.

The discipline that has carried the runtime so far - *smallest possible increment that produces a working artefact, forced by a worked example* - applies to each. None of these are speculative roadmap entries; each has a concrete forcing scenario named in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) or [`docs/forced-by-examples.md`](docs/forced-by-examples.md), and each lands when an example actually demands it.

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, the language affordances on the roadmap, the three-level expansion ladder, and non-goals. Start here for the design framing.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - semantics the `morpholog-core` kernel realises.
- [`docs/forced-by-examples.md`](docs/forced-by-examples.md) - retrospective doctrine doc recording, for each significant runtime/IR decision, which worked example forced it and why.
- [`docs/outbox-sketch.md`](docs/outbox-sketch.md) - design sketch for the outbox worker plus compensating-transformation pattern (the "Morpholog plus an Outside Coordinator" architecture). Accompanied by a hand-rolled spike test; not yet implemented.
- Worked examples: [`examples/01_settlement_netting/`](examples/01_settlement_netting/), [`examples/02_revenue_restatement/`](examples/02_revenue_restatement/), [`examples/03_claim_standing/`](examples/03_claim_standing/), [`examples/04_double_entry_ledger/`](examples/04_double_entry_ledger/).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
