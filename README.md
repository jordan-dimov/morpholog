# Morpholog

There comes a moment in every serious business when someone - an auditor, a regulator, a board director who has not been having a great quarter - asks you to *prove* a number. Why is it what it is. Who decided it should be. What it looked like before the restatement. Whether the rule it had to satisfy actually held at the moment it was admitted.

In most systems, those answers come from detective work. You pull logs, reconcile parallel files, ask the people who happened to be on call.

Morpholog answers them differently. It treats records as **admitted claims** - statements admitted into the system under specific authority at a specific moment, never as freestanding truth. The same number can carry different *standing* for different decisions, granted and revoked by different authorities. The verifier can correct the figure; the original stays in the books and the corrected figure becomes current. **Decisions admitted under valid standing remain valid records even when that standing is later revoked** - the legitimacy of a *past* decision was established when it was made. Every change is checked against the rules atomically; nothing sticks unless every rule holds; the audit log carries enough provenance to reconstruct any past moment - no bitemporal columns, no shadow tables.

The point is to make *"how do you know?"* answerable by *"because the system could not have admitted it otherwise."*

## The questions you can answer

- *Why does this report differ from the one we filed last quarter?*
- *Who admitted this entry, and under what authority?*
- *If that authority was rescinded yesterday, was yesterday's decision still valid?*
- *What did the books say on the last day of Q1, under the close rules in force then?*
- *Did this trade conform to our exposure limits when it was booked? Not now - then?*

The worked examples below answer concrete instances of these against a real PostgreSQL database, through a small CLI.

## What you get for writing your rules this way

- **The same record can carry different standing for different decisions** - granted and revoked by different authorities, without modifying the underlying record. The shape regulated lending and statutory reporting need.
- **Correction without overwriting.** Originals stay admitted; a new transformation asserts the correction and records the lineage. An auditor three quarters later sees the original, the corrected figure, and the moment one became the other.
- **A complete audit of every change.** Each commit writes a row carrying the transformation, the actor, the arguments, the asserted and retracted records, the emitted notifications, and the rules that governed admission.
- **Atomic commit or full rollback.** A change either lands every part of itself - records, audit row, outbound notifications - or none of them. PostgreSQL's `SERIALIZABLE` isolation does the work.
- **Reports computed by the same rules they're governed by.** A trial balance, an exposure summary, an account balance - declared alongside the rules, computed by the same kernel. A report cannot show a record an invariant would have refused.
- **Notifications that respect the commit boundary.** Outbound side effects stage at commit and deliver afterward through a worker. They never run inside the transaction; they never run if the commit rolled back.
- **Arbitrary-precision decimals.** No floating-point drift. Adds, subtracts, sums - all exact.

## How it works

Two first-class constructs. Everything else is built from these.

An **invariant** says what must always be true of admitted records. A **transformation** is the only path that gets to change them. It proposes additions, removals, and outbound notifications; the runtime checks every active invariant against the proposed result; if anything fails, nothing happens.

A fragment of the double-entry ledger, in `.morph` source:

```morph
program double_entry_ledger

predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)
predicate JournalLine(entry_id: Subject, account: Subject, debit: Decimal, credit: Decimal)
predicate PeriodClosed(period: Subject)

invariant balanced_posted_entry:
    JournalEntry(entry, _, _) implies sum(d | JournalLine(entry, _, d, _)) = sum(c | JournalLine(entry, _, _, c))

transformation post_simple_entry(entry_id, posting_date, period, debit_account, credit_account, amount):
    require not PeriodClosed(period)
    admit JournalEntry(entry_id, posting_date, period)
    admit JournalLine(entry_id, debit_account, amount, 0)
    admit JournalLine(entry_id, credit_account, 0, amount)
    emit JournalEntryPosted(entry_id)
```

The invariant says debits and credits must balance for every posted entry. The transformation is the only way a journal entry can enter the books, and only when the period is open and the result balances. If the math is off by a penny, the commit is rejected atomically and the database looks exactly as it did before the attempt.

## A sixty-second tour

One-time setup (PostgreSQL 17+):

```bash
createdb my_books
psql my_books -f crates/morpholog-core/sql/schema.sql
export DATABASE_URL=postgres:///my_books
```

Post a journal entry - debit $100 to cash, credit $100 to revenue:

```bash
morpholog propose double_entry_ledger post_simple_entry \
  --actor jordan \
  --args '[
    {"type":"subject","value":"entry_001"},
    {"type":"subject","value":"2026-04-15"},
    {"type":"subject","value":"q1_2026"},
    {"type":"subject","value":"account_cash"},
    {"type":"subject","value":"account_revenue"},
    {"type":"decimal","value":"100"}
  ]'
```

The receipt carries the transition id, the actor, the asserted claims, and the emitted intents. `--actor` records under whose authority the transition was proposed; the audit row carries it for the next three years.

Now the trial balance, then the trial balance *as it stood right after the first transition*:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow
morpholog inspect derived double_entry_ledger TrialBalanceRow --as-of <transition_id>
```

The report is exactly what an auditor would have seen at that moment - recomputed from the audit log, no bitemporal columns anywhere in your schema.

An unbalanced entry would come back as `{"status":"rejected","reason":"balanced_posted_entry invariant did not hold"}`.

## Worked examples

Each runs both in memory (against the kernel) and durably (against PostgreSQL). All parse end-to-end as `.morph` source. Nothing is mocked.

- [**Bilateral settlement netting**](examples/01_settlement_netting/) - invariants check the *candidate state*, not the pre-state. Transformations that combine individually-valid inputs into a forbidden whole are rejected before any commit.
- [**Verified revenue**](examples/02_verified_revenue/) - the flagship. A figure is independently verified; different authorities grant *standing* for it to be relied on for their own decisions. The verifier can correct it later; the original stays in the books; **active standings on the prior figure are retracted automatically**; **historical decisions made under the original remain valid records of what was decided that day.** Defending a contested number over time, in one example.
- [**Double-entry ledger with trial balance**](examples/03_double_entry_ledger/) - the accounting equation as an invariant; period close as an admission gate; restatement that preserves the original; the trial balance as a **governed read-side projection**. As-of replay yields the historical trial balance without a bitemporal column in the schema.
- [**Approval controls**](examples/04_approval_controls/) - unconditional and quantitative authority. The approving actor flows from transition context; the asserted record carries them. Revocation prevents future approvals while preserving every historical one.
- [**Insurance claim settlement**](examples/05_insurance_claim_settlement/) - cumulative settlements against a policy aggregate limit, evaluated at admission as `paid_so_far + proposed_settlement <= aggregate_limit`. Same audit-grade evidence regime as the other examples.
- [**Clinical trial enrolment**](examples/06_clinical_trial_enrolment/) - a participant is randomised only if protocol, consent, investigator delegation and eligibility evidence are all valid on the randomisation date. **Validity is admission-time, not eternal**: a later protocol amendment does not invalidate an earlier randomisation. The first non-finance worked example, grounded in Good Clinical Practice.

Morpholog is not the whole stack. UIs, dashboards, dataloaders, ML pipelines stay in normal tools. What Morpholog owns is the line where *"may this be admitted as a valid record?"* needs a definite answer - the small fraction of any real business system that, when it fails, makes the news.

## Try it yourself

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
cargo install --path crates/morpholog-cli
morpholog check examples/01_settlement_netting/netting.morph
```

`check` parses the file and runs `Program::validate()` against the IR - one command answers "is this program well-formed?" `parse` prints the parsed `Program` as JSON when you want to see the structure. `propose` and `inspect` run against PostgreSQL once `DATABASE_URL` is set; see [`CONTRIBUTING.md`](CONTRIBUTING.md) for local-database setup.

## Status

Active development. Kernel, PostgreSQL adapter, CLI, polling outbox worker, and worked examples are all working and tested. Every committed transition records its actor. Writes scale linearly (~1.6s per commit at 100,000-entry scale); as-of replay also linear (~1.5s for 100,000 transitions). Predicate-scoped loading on both read and write paths means a transformation only loads claims it actually consults.

The `.morph` parser arc is complete: every worked example parses end-to-end as `.morph` source. The formatter and parser are coupled by a round-trip property test. Diagnostics surface through `ariadne` with source spans.

Built in Rust on PostgreSQL 17+. The kernel is `#[forbid(unsafe_code)]`; the PG adapter leans on SERIALIZABLE isolation and JSONB so an entire commit lands atomically or not at all.

What is *not* in the box yet: a worker supervisor with circuit breakers and an HTTP-aware deliverer; predicate-pattern matching for higher-order authority; user-supplied program loading from `.morph` at runtime (today's CLI runs built-in programmes); materialised derived claims; legibility tooling that answers *"what does this system prevent?"* with mechanically-derived exclusion matrices. Each lands when a worked example forces the shape.

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, the affordances on the design horizon, and non-goals.
- [`docs/roadmap.md`](docs/roadmap.md) - what's imminent, deferred, and out of scope.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - what the kernel means.
- [`docs/design-history.md`](docs/design-history.md) - for each significant runtime/IR decision, which worked example forced it.
- [`docs/outbox-sketch.md`](docs/outbox-sketch.md) - the "Morpholog plus an Outside Coordinator" doctrine for the outbox worker.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
