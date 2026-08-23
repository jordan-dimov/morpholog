# Morpholog

Morpholog is a language and runtime for the records a business must be able to defend: the books, the approvals, the trades, the filings. You write the rules those records must obey. From then on, no change that breaks a rule can be committed - by anyone, or anything - and every change that does commit carries proof of why it was allowed.

That flips the usual burden. When someone asks *"how do you know this number is right?"*, the answer is no longer an investigation. It is: **the system could not have admitted it otherwise** - and here is the audit trail that proves it.

## The questions this makes answerable

- *Why does this report differ from the one we filed last quarter?*
- *Who admitted this entry, and under what authority?*
- *If that authority was rescinded yesterday, was yesterday's decision still valid?*
- *What did the books say on the last day of Q1, under the rules in force then?*
- *Did this trade conform to our exposure limits when it was booked - not now, then?*
- *On what basis did the AI system identify this person, who verified it, and were they authorised that day?*

Each is a concrete question the worked examples below answer against a real PostgreSQL database, through a small CLI.

## How it works

There are two constructs, and everything else is built from them:

- An **invariant** is a rule that must always hold across the records.
- A **transformation** is the only way records change. It proposes additions, removals, and outbound notifications; the runtime checks every invariant against the proposed result; if any rule would break, nothing happens.

A fragment of a double-entry ledger:

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

Read it as plain statements: every entry's debits must equal its credits. Posting is the only way an entry gets into the books, and only while the period is open. An entry that is off by a penny is refused - atomically, with the rule named - and the database looks exactly as it did before the attempt.

There is no way around the check. No bypass flag, no skip-validation switch, no admin path. If the business genuinely needs an exception, the exception is itself a rule-governed, audited record - never a back door.

One word to know before reading further: Morpholog's own term for a record is a **claim** - a statement admitted under someone's authority at a specific moment, not a neutral fact. That framing is why corrections supersede rather than overwrite, and it is the word the examples and docs use.

## Where it fits in your stack

Morpholog is not a general-purpose language and does not replace your application. Screens, jobs, pipelines, and analytics stay in the tools you already use - Python, TypeScript, whatever you like. What moves into Morpholog is the governed core: the records that must be defensible, and the rules they must obey. Your code computes whatever it needs to, then *proposes* the result - through the typed Python client the binary generates from your own model, the CLI, or batch import - and Morpholog decides whether it may become a record. Reads come back the same way, at any past moment, or through generated typed SQL views. It owns only the small fraction of any system that, when it fails, makes the news.

## What every commit gives you

- **A full audit record** - the transformation, who proposed it, the arguments, what was added and removed, and the rules that were checked.
- **Correction without overwriting.** A correction admits the new figure and records what it supersedes; the original stays in the books. An auditor sees the original, the correction, and the moment one became the other.
- **Exact time travel.** Ask for any report as of any past moment and get precisely what the system knew then. Rules never read the clock or the network - anything the outside world determines, like a rate or a calendar, enters as a dated record - so the past cannot shift under you. No bitemporal columns, no shadow tables.
- **Standing, separate from the record.** Whether a figure may be *relied on* - and for what - is itself a record, granted and revoked by named authorities. A decision made while its basis was valid remains a valid record after that basis is revoked: its legitimacy was settled when it was made.
- **Exact arithmetic.** Decimals with no floating-point drift; instants and durations that compute exactly; amounts that know their unit, so dollars never meet tonnes by accident.
- **Side effects that respect the commit.** Outbound notifications stage with the commit and deliver after it, through a worker - never from inside the transaction, never if the commit rolled back.

## How this differs from what you already have

- **A database with constraints.** A `CHECK` constraint validates one row and fails with a constraint name. Morpholog checks every business rule against the whole proposed next state - including rules that span many records - refuses with an explanation in your own vocabulary, and keeps the full history of what was admitted, by whom, and why.
- **A policy engine** (OPA, Cedar). A policy engine answers "may this happen?" but owns no records: no commit, no audit trail, no history to replay. Morpholog's rules gate the actual state change and leave the proof behind.
- **An immutable or bitemporal database** (Datomic, XTDB). These remember everything but enforce nothing: nothing stops an invalid record entering the history, and a refusal is not a concept. Morpholog keeps the history *and* makes invalid records uncommittable.
- **A BDD test suite** (Cucumber). Given/when/then scenarios live beside the code and drift from it. In Morpholog the behavioural spec *is* the enforcement: the given/when/then runs on every real change, whoever proposed it, and cannot drift.
- **A workflow engine.** Workflows order the steps; they do not make an invalid outcome impossible. Morpholog does not care what order things happened in - only that no rule is broken by the result.

None of the underlying ideas is new - each has decades of theory behind it. What did not exist is all of them in one small runtime on plain PostgreSQL, enforcing at the one line that matters: the moment something becomes a record. [`docs/prior-art.md`](docs/prior-art.md) has the longer comparison.

## Anything can propose; only the rules admit

More and more of what writes to business records is software you did not write and cannot audit line by line - optimisers, ML models, autonomous agents. Morpholog is built for exactly that. Nothing writes directly: a person, a script, or a model only ever *proposes*, and the runtime admits or refuses. A refusal is not an error code - it is a structured, reproducible account of which rule failed and what evidence is missing, in your own rule names, never free text. A machine can read it, repair its proposal, and try again: **propose, refuse, repair**. You do not have to trust the proposer, because the rules - not the proposer - decide what becomes real.

## A sixty-second tour

One-time setup (PostgreSQL 18+):

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

The receipt carries the transition id, the actor, the admitted records, and the emitted notifications. Now the trial balance - and the trial balance *as it stood right after that first transition*:

```bash
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow --as-of <transition_id>
```

The second answer is exactly what an auditor would have seen at that moment, recomputed from the audit log. And an unbalanced entry is refused with the rule named - and its exact line in your source pointed at:

```json
{ "status": "rejected", "reason": "invariant `balanced_posted_entry` violated" }
```

## Worked examples

Each parses from `.morph` source and runs end to end against PostgreSQL. Nothing is mocked. Every per-example README has the business story.

The list below is by domain. If you arrived with a question rather than a domain - "can it admit a set of records in one act?" - start from the [capability index](examples/README.md), which maps what you want to do to the example that shows it.

- [**Settlement netting**](examples/01_settlement_netting/) - inputs that are valid alone but forbidden together are refused before commit.
- [**Verified revenue**](examples/02_verified_revenue/) - the flagship: a figure is verified, relied on, then corrected - and every decision made along the way stays defensible.
- [**Double-entry ledger**](examples/03_double_entry_ledger/) - the accounting equation as a rule; period close; restatement; the trial balance with as-of replay.
- [**Approval controls**](examples/04_approval_controls/) - authority granted, used, and revoked; revocation stops future approvals and preserves every past one.
- [**Insurance claim settlement**](examples/05_insurance_claim_settlement/) - cumulative payouts capped against the policy, exactly, at admission.
- [**Clinical trial enrolment**](examples/06_clinical_trial_enrolment/) - protocol, consent, and eligibility must be valid *on the randomisation date*; a later amendment does not undo an earlier enrolment.
- [**Chess**](examples/07_chess_transition_invariants/) - the toy: rules that relate the state before a change to the state after.
- [**KYC sanctions screening**](examples/08_kyc_sanctions_screening/) - onboarding blocked on stale screenings or an unadjudicated match.
- [**Carbon-credit provenance**](examples/09_carbon_credit_provenance/) - no green claim without admissible provenance; double counting forbidden in both directions.
- [**Trade lifecycle**](examples/10_trade_lifecycle/) - capture to settlement, with lifecycle phase as accumulated records, not a status field; a price correction leaves prior settlements standing.
- [**Borrowing base**](examples/11_borrowing_base/) - drawn amounts can never exceed the advance rate times eligible collateral.
- [**Laytime and demurrage**](examples/12_laytime_demurrage/) - voyage chartering's argument about minutes: exact instants, computed deadlines, cargo in tonnes, delay priced in dollars.
- [**Biometric identification oversight**](examples/13_biometric_identification_oversight/) - an EU AI Act statute enforced as admission rules: the AI's output has no standing until verified by two distinct, currently-authorised people.
- [**Margin call run**](examples/14_margin_call_run/) - a risk engine submits a whole batch as one decision, admitted only if *complete*: a missing margin call is refused, not just a wrong one.
- [**Metered billing**](examples/15_metered_billing/) - a bill correct to the penny: each line recomputed and rounded, the sealed total refusing the rival convention by name.
- [**Release governance**](examples/16_release_governance/) - this project's own release checklist as law: a tag at an ungated commit, or an announcement missing a platform's download, refuses to commit.
- [**Covenant reporting**](examples/17_covenant_reporting/) - a loan's reporting calendar as law: test dates rolling exactly three calendar months (clamped at month ends), and an overdue notice refused unless its day count is the record's own.
- [**Scoped charges**](examples/18_scoped_charges/) - the record picks a figure's source: metered charges take the meter's own reading, caller-sourced ones the proposal, and a line wearing the wrong source's figure cannot commit.
- [**Charging years**](examples/19_charging_years/) - a billing period may not straddle the 1 April anniversary: the charging year is a computed coordinate, the gate compares both ends' years, and the run records the year the rules computed along with the rate sheet it priced from - and a run that read the wrong year's sheet cannot commit.
- [**Operational information**](examples/20_operational_information/) - what two sources are worth only together: an untrusted optimiser files exact expected-loss certificates, the record recomputes every figure and refuses any beaten action, and a pair's synergy is read off the certified values.
- [**Worked embedder**](examples/etrm_embedder/) - the same trade lifecycle driven from Python through the generated client, including post-commit delivery.

## Try it yourself

No Rust toolchain needed: grab the prebuilt binary for linux (x86_64 or
arm64) or macOS (Apple Silicon) from the
[releases page](https://github.com/jordan-dimov/morpholog/releases) -
[`docs/install.md`](docs/install.md) is the fresh-machine walkthrough,
PostgreSQL included. Or from source:

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
cargo install --path crates/morpholog-cli
morpholog check examples/01_settlement_netting/netting.morph
```

`check` parses and validates, with every finding caret-located in the source. `propose` and `inspect` run against PostgreSQL once `DATABASE_URL` is set; [`CONTRIBUTING.md`](CONTRIBUTING.md) has the local-database setup.

New to Morpholog? [The developer introduction](docs/developer-intro.md) is the hands-on place to start - written for a developer who knows Python and SQL, it builds a small governed ledger end to end: a reported figure, a decision that relies on it, the honest correction, and the as-of replay that keeps both answers true.

## Status

Active development, built in Rust on PostgreSQL 18+, no unsafe code anywhere. The kernel, PostgreSQL adapter, CLI, outbox worker, and every worked example are working and tested end to end. A governed commit is ~9ms at worked-example scale; writes and as-of replay both scale linearly from there (~1.5s per commit with 100,000 in-scope records, under a frozen benchmark suite that keeps such claims honest). A compiled-to-SQL checking path - measured flat at ~2ms across that same sweep - is landing in stages: the compiler and the differential holding it to agreement with the kernel are merged; production integration follows.

Beyond the core: you can read a model's rules back before anything runs - what they make impossible (`inspect guarantees`), what each action requires (`inspect controls`), which rules have ever actually done work (`inspect coverage`) - and turn any rejection into a missing-evidence checklist (`explain`). A refusal names the values that caused it, not just the rule it broke - `invariant line_net_is_the_rounded_recompute violated` arrives with the line, the invoice, and the figures the rule was reading, so you can see the arithmetic without opening the database. The audit log is a tamper-evident Merkle tree: `audit verify` proves it intact rather than asserting it, and `audit export` produces a pack a third party verifies offline against a 32-byte anchor held outside the database - catching even a coordinated edit of the records and the log together. The integration surface is a pinned contract ([`docs/embedder-integration.md`](docs/embedder-integration.md)) with a **typed Python client generated by the binary itself**; an external open-source energy-trading system already drives a governed trade lifecycle through it.

Not in the box yet: a worker supervisor and HTTP deliverer for the outbox, higher-order authority, incremental maintenance for derived views. Each lands when a worked example forces its shape - the discipline that has kept the runtime small.

## Common questions

**Doesn't a separate system of record create a dual-write problem?** It removes the classic one. Morpholog holds the governed records; downstream stores are derived copies, kept current by consuming the notifications each commit emits (at-least-once, with idempotency keys). One write, explicit propagation, no two-phase commit. The schema is plain PostgreSQL and can share a database with your application's tables, behind its own schema and role.

**Can't someone bypass the rules with raw SQL?** With superuser access, yes - as with any database-backed system (a DBA can drop a `CHECK` constraint too). The mitigations: ordinary privilege separation, so only the runtime's role writes the governed schema; and tamper evidence, because the records and the audit log are two accounts of one history and `verify` catches edits that leave them disagreeing - or that rewrite both, if you have published a checkpoint anchor outside the database. The anchor is 32 bytes: email it to your auditor, hand it to a counterparty, stamp it with a timestamping service. The honest limit: an anchor protects history only up to the last one you distributed, and distributing it is operational discipline the runtime cannot enforce.

**Isn't a generic claims table a performance trap?** The table is storage, not the query engine. Rules are never evaluated as SQL self-joins: the runtime loads only the records a transformation touches and evaluates in memory, and the measured shape is linear (the Status numbers above). Heavy querying and reporting live in the typed SQL views Morpholog generates per predicate, or downstream - the system-of-record-plus-derived-reads split any audit substrate needs.

**What about GDPR's right to erasure, if nothing is deleted?** Keep personal data in an ordinary, erasable store keyed by an opaque subject id, and keep personal details out of the governed records themselves - a modelling discipline the opaque-identifier design encourages but cannot make automatic. Cryptographic redaction within history is a recognised future direction.

**How does a record's shape evolve once history exists?** By supersession, the same way records do: declare the new shape, carry forward with a governed transformation, and history stands as recorded. First-class migration tooling is future work.

**Do I have to shell out to a CLI on every request?** No. One-shot CLI calls are the simple path (~9ms per call, measured), and `morpholog session` is the resident one: a single process that loads your rules once, holds a warm connection, and answers proposals and reads over stdio a few times faster per call - with the same JSON envelopes either way. You don't hand-write the integration: `morpholog generate python-client` emits a complete, typed, dependency-free Python client from your own model - including the session wrapper - stamped with the hash of the rules it was built against. Rust embeds the library in-process. A socket server mode and more languages follow when a real integration forces them.

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, and what it refuses to become.
- [`docs/roadmap.md`](docs/roadmap.md) - what's next, deferred, and out of scope.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - what the kernel means, precisely.
- [`docs/embedder-integration.md`](docs/embedder-integration.md) - the pinned contract for integrating from any language.
- [`docs/design-history.md`](docs/design-history.md) - which worked example forced each design decision, and why.
- [`docs/prior-art.md`](docs/prior-art.md) - the theory this synthesises, and what was deliberately rejected.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
