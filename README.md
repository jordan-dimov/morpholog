# Morpholog

**Rules your records cannot break.**

Morpholog guards the records a business must be able to defend: the books, the approvals, the trades, the filings. You write the rules those records must obey. From then on, no change that breaks a rule can be saved - whether a person, a script or an AI agent tries - and every change that is saved carries proof of why it was allowed.

```text
your app, a script, an AI agent
        |
        |  proposes a change
        v
   +----------+     every rule holds  -->  saved, with an audit record
   |  rules   |
   +----------+     a rule would break -->  refused, with the reason;
                                            nothing changes
```

That flips the usual burden. When someone asks *"how do you know this number is right?"*, the answer is no longer an investigation. It is: **the system could not have saved it otherwise**, and here is the audit trail that proves it.

## Questions it can answer

- *Who entered this, and under what authority?*
- *If that authority was taken away yesterday, is yesterday's decision still valid?*
- *What did the books say on the last day of Q1, under the rules in force then?*
- *Did this trade respect our exposure limits when it was booked - not now, then?*
- *On what basis did the AI system identify this person, who checked it, and were they allowed to that day?*

Each one is answered by a worked example below, against a real PostgreSQL database, through a small command-line tool.

## Anything can propose; only the rules decide

More and more of what writes to business records is software you did not write and cannot check line by line: optimisers, machine-learning models, AI agents. Morpholog is built for that. Nothing writes directly. A person, a script or a model only ever *proposes* a change, and Morpholog saves it or refuses it.

A refusal is not an error code. It names the rule that failed and the values that failed it, in your own vocabulary. A program can read it, fix its proposal and try again: **propose, refuse, repair**. You do not have to trust the proposer, because the rules decide what gets saved.

## A rule, read aloud

Morpholog has two building blocks:

- An **invariant** is a rule that must always hold across the records.
- A **transformation** is the only way records change. It proposes what to add, what to remove and who to notify. If the result would break any invariant, nothing happens.

Everything else is built from these two.

Here is part of a double-entry ledger:

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

Read it line by line:

- Each `predicate` declares a kind of record: a journal entry, a line of an entry, a closed period.
- The `invariant` says: for every entry, the debits add up to the credits.
- The `transformation` posts an entry. It first `require`s that the period is still open, then `admit`s the entry and its two lines, then `emit`s a notification for other systems.

An entry that is off by a penny is refused, with the rule named, and the database looks exactly as it did before. There is no way around the check: no bypass flag, no admin path. If the business needs an exception, the exception is itself a record the rules govern and the audit trail keeps.

One word before going further. Morpholog calls a record a **claim**: something stated under someone's authority at a particular moment, not a neutral fact. That is why a correction is a new claim that replaces the old one, rather than an edit that erases it.

## What you get

- **A full audit record for every change**: what was proposed, by whom, with which values, what was added and removed, and which rules were checked.
- **Corrections that keep the original.** A correction records what it replaces. An auditor sees the original figure, the corrected one, and the moment one became the other.
- **Exact time travel.** Ask for any report as it stood at any past moment and get exactly what the system knew then. Rules never read the clock or the network; anything the outside world decides, like a rate or a holiday calendar, comes in as a dated record. So the past cannot shift under you.
- **Decisions that stay valid.** Whether a figure may be relied on is itself a record, granted and withdrawn by named people. An approval given while its basis was sound stays valid after that basis is withdrawn.
- **Exact arithmetic.** Decimals with no rounding drift, exact times and durations, and amounts that carry their unit, so dollars never get added to tonnes.
- **Several changes as one decision.** `morpholog transact` saves a group of proposals together, or none of them.
- **Notifications that respect the save.** Messages to other systems are sent only after a change is saved, by a separate worker, and never for a change that was refused.
- **Rules you can read back.** Before anything runs, ask what the rules forbid (`inspect guarantees`) and what each action requires (`inspect controls`). Later, ask which rules have ever actually done any work (`inspect coverage`), or turn a refusal into a checklist of missing evidence (`explain`).
- **A tamper-evident history.** `audit verify` proves the history has not been edited. `audit export` produces a file someone else can check offline, against a 32-byte fingerprint you gave them in advance, which catches even an edit to the records and the history together.

## How this differs from what you already have

- **Database constraints.** A `CHECK` constraint checks one row and fails with a constraint name. Morpholog checks rules that span many records, refuses in your own vocabulary, and keeps the history of what was saved, by whom and why.
- **Policy engines** (OPA, Cedar) answer "may this happen?" but hold no records: no commit, no audit trail, no history to replay. Morpholog's rules guard the change itself and leave the proof behind.
- **Immutable or bitemporal databases** (Datomic, XTDB) remember everything but enforce nothing: an invalid record enters history like any other. Morpholog keeps the history *and* refuses the invalid record.
- **BDD test suites** (Cucumber) describe behaviour beside the code, and drift from it. In Morpholog the rules *are* the enforcement, checked on every real change, so they cannot drift.
- **Workflow engines** decide the order of steps. They do not make a wrong outcome impossible. Morpholog does not care about order, only that the result breaks no rule.

None of the ideas underneath is new. What did not exist is all of them in one small runtime on plain PostgreSQL, enforced at the moment something becomes a record. [`docs/prior-art.md`](docs/prior-art.md) has the longer comparison.

## Where it fits in your stack

Morpholog is not a general-purpose language and does not replace your application. Screens, jobs, pipelines and analytics stay in the tools you already use. Only the governed core moves: the records that must be defensible, and their rules. Your code works out whatever it needs to, then proposes the result through a typed Python client generated from your own rules, the command line, or a batch import. Reads come back the same way, as of any past moment, or through generated SQL views. Morpholog owns the small part of a system that makes the news when it goes wrong.

## Try it

No Rust toolchain needed: download the binary for Linux (x86_64 or arm64) or macOS (Apple Silicon) from the [releases page](https://github.com/jordan-dimov/morpholog/releases). [`docs/install.md`](docs/install.md) walks through a fresh machine, PostgreSQL included. Or build from source:

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
cargo install --path crates/morpholog-cli
morpholog check examples/01_settlement_netting/netting.morph
```

`check` validates a rules file and points at the exact line of any problem. To go further, set up a database (PostgreSQL 18 or later):

```bash
createdb my_books
export DATABASE_URL=postgres:///my_books
morpholog init
```

Post a journal entry - debit $100 to cash, credit $100 to revenue:

```bash
morpholog propose examples/03_double_entry_ledger/ledger.morph post_simple_entry \
  --actor jordan \
  --args-named '{"entry_id":"entry_001","posting_date":"2026-04-15","period":"q1_2026",
                 "debit_account":"account_cash","credit_account":"account_revenue","amount":"100"}'
```

The receipt names the change (its transition id), who made it, and what was saved. Now read the trial balance, and the trial balance as it stood right after that change:

```bash
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow --as-of <transition_id>
```

The second answer is exactly what an auditor would have seen at that moment, rebuilt from the audit log. An unbalanced entry is refused with the rule named, and the line of your rules file it broke:

```json
{ "status": "rejected", "reason": "invariant `balanced_posted_entry` violated" }
```

New to Morpholog? [The developer introduction](docs/developer-intro.md) is the hands-on start. Written for someone who knows Python and SQL, it builds a small governed ledger end to end: a reported figure, a decision that relies on it, the honest correction, and the replay that keeps both answers true.

## Worked examples

Each one runs end to end against PostgreSQL; nothing is mocked, and each has a README with the business story. If you came with a question rather than an industry - "can it accept a whole batch as one decision?" - the [capability index](examples/README.md) maps what you want to do to the example that shows it.

**Money and markets**

- [Double-entry ledger](examples/03_double_entry_ledger/) - debits equal credits; closing a period; restating; a trial balance at any past moment.
- [Settlement netting](examples/01_settlement_netting/) - settlements that are fine alone but forbidden together are refused.
- [Insurance claim settlement](examples/05_insurance_claim_settlement/) - total payouts can never exceed the policy limit.
- [Trade lifecycle](examples/10_trade_lifecycle/) - a commodity trade from capture to settlement; a price correction leaves earlier settlements standing.
- [Borrowing base](examples/11_borrowing_base/) - a loan can never be drawn beyond what its collateral supports.
- [Margin call run](examples/14_margin_call_run/) - a risk engine's whole batch is accepted only if it is complete; a missing call is refused, not just a wrong one.
- [Metered billing](examples/15_metered_billing/) - a bill correct to the penny, every line recomputed and rounded the agreed way.
- [Scoped charges](examples/18_scoped_charges/) - each charge takes its figure from the right source, and a line with the wrong source's figure cannot be saved.
- [Charging years](examples/19_charging_years/) - a billing period may not cross the 1 April anniversary, and each run records the price list it used.
- [Covenant reporting](examples/17_covenant_reporting/) - a loan's reporting calendar, three calendar months at a time, with overdue notices that must count the days correctly.
- [Laytime and demurrage](examples/12_laytime_demurrage/) - shipping's argument about minutes: exact times, computed deadlines, tonnes of cargo and dollars of delay.

**Evidence, authority and regulation**

- [Verified revenue](examples/02_verified_revenue/) - the flagship: a figure is verified, relied on, then corrected, and every decision along the way stays defensible.
- [Approval controls](examples/04_approval_controls/) - authority granted, used and withdrawn; withdrawing it stops future approvals and keeps past ones valid.
- [Clinical trial enrolment](examples/06_clinical_trial_enrolment/) - protocol, consent and eligibility must all be valid on the day a patient is enrolled.
- [KYC sanctions screening](examples/08_kyc_sanctions_screening/) - onboarding blocked by an out-of-date screening or an unresolved match.
- [Carbon-credit provenance](examples/09_carbon_credit_provenance/) - no green claim without evidence behind it, and no credit counted twice.
- [Biometric identification oversight](examples/13_biometric_identification_oversight/) - the EU AI Act as rules: an AI's match counts for nothing until two different, currently authorised people verify it.

**Operations**

- [Release governance](examples/16_release_governance/) - this project's own release checklist: a release tagged before its checks passed cannot be recorded.
- [Operational information](examples/20_operational_information/) - an untrusted optimiser's figures are recomputed and checked before any of them is accepted.
- [Worked embedder](examples/etrm_embedder/) - the trade lifecycle driven from Python through the generated client.

**A toy**

- [Chess](examples/07_chess_transition_invariants/) - rules that compare the board before a move with the board after it.

## Status

Active development, in Rust on PostgreSQL 18+, with no unsafe code. The kernel, database adapter, command-line tool, notification worker and every worked example work and are tested end to end.

Capture Energy, a licensed electricity supplier in Great Britain, was a design partner and the first commercial application of Morpholog (2026).

- **Speed.** A saved change takes about 9ms at worked-example scale. Where a programme's rules compile to SQL and their indexes are in place, a change checks only the records it touches and stays under 10ms from 1,000 to 100,000 records. Other rules are checked in memory and grow with the data (about 1.5s per change at 100,000). A frozen benchmark suite keeps these numbers honest.
- **Integration.** The contract is pinned ([`docs/embedder-integration.md`](docs/embedder-integration.md)), and the binary generates a typed Python client from your rules. An open-source energy-trading system already runs a governed trade lifecycle through it.
- **Not yet built:** a supervisor and HTTP delivery for the notification worker, authority rules that cover whole families of actions, and incremental refresh of derived views. Each arrives when a worked example needs it - the discipline that has kept the runtime small.

## Common questions

**Doesn't a separate system of record mean writing everything twice?** No. Morpholog holds the governed records; other stores are copies, kept up to date from the notifications each change sends (delivered at least once, with keys that make repeats harmless). One write, then explicit copying, with no two-phase commit. The tables are plain PostgreSQL and can sit in the same database as your application's, in their own schema.

**Can't someone bypass the rules with raw SQL?** With superuser access, yes, as with any database (a DBA can drop a `CHECK` constraint too). Two things limit it. Ordinary permissions let only Morpholog's role write its tables. And the records and the audit log are two accounts of one history, so `audit verify` catches an edit that makes them disagree, or one that rewrites both if you have shared a checkpoint fingerprint outside the database. That fingerprint is 32 bytes: email it to your auditor, or have a public timestamp authority sign it (`audit checkpoint --witness rfc3161:<url>`) so the check can also show *when* the history looked like this. The honest limit: it protects history only up to the last fingerprint you shared, and sharing it is a habit the software cannot enforce.

**Isn't one generic table of records slow?** The table is storage, not the query engine. A rule that compiles to SQL is checked inside the same transaction, only over the records a change touches; other rules are checked in memory over the records the change reads. Either way the in-memory version is the specification, and a test holds the SQL version to it. Heavy querying and reporting use the typed SQL views Morpholog generates, or a downstream copy.

**What about GDPR's right to erasure, if nothing is deleted?** Keep personal data in an ordinary store you can erase, keyed by an opaque id, and keep personal details out of the governed records. The design encourages this but cannot make it automatic. Erasing inside the history cryptographically is a known future direction.

**How does the shape of a record change once history exists?** The same way records change: declare the new shape, carry the data forward with a governed transformation, and history stays as it was recorded. Tooling for this is future work.

**Do I have to call a command-line tool for every request?** No. One-off calls are the simple path (about 9ms each). `morpholog session` keeps one process running with your rules loaded and a warm connection, and answers several times faster per call, with the same JSON either way. `morpholog generate python-client` writes a typed, dependency-free Python client from your rules, session support included, stamped with the fingerprint of the rules it was built from. Rust programs use the library directly. A network server and more languages will come when a real integration needs them.

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, and what it refuses to become.
- [`docs/roadmap.md`](docs/roadmap.md) - what's next, what's deferred, and what's out of scope.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - exactly what the rules mean.
- [`docs/embedder-integration.md`](docs/embedder-integration.md) - the pinned contract for integrating from any language.
- [`docs/design-history.md`](docs/design-history.md) - which worked example forced each design decision, and why.
- [`docs/prior-art.md`](docs/prior-art.md) - the theory underneath, and what was deliberately left out.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
