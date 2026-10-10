# Morpholog

**Rules your records cannot break.**

People, applications, optimisers and AI agents propose the changes a business must be able to defend: booking a trade, approving a payment, accepting a model's result, settling an account.

Anything may propose an action. Morpholog accepts it only if the organisation's rules, authority and evidence requirements are satisfied. An accepted action leaves a record of what was decided, by whom, under which rules and why. A refused one changes nothing, and the refusal names the rule that failed and the values that failed it, so a person or a program can repair the proposal and try again.

**You do not have to trust the system proposing the action.** The control sits outside it. A risk engine, an optimiser or an AI agent can calculate whatever it likes; it cannot make a change the rules forbid. Morpholog has no bypass flag and no privileged "approve anyway" path.

So when someone asks *"how do you know this record met the controls?"*, the answer is not an investigation. It is: **the system could not have saved it otherwise**, and here is the record that proves it.

## Where Morpholog fits

```text
   person, application, optimiser, AI agent
                     |
                     |  proposes an action
                     v
             +----------------+
             |   Morpholog    |   rules, authority, evidence
             +----------------+
                |            |
       accepted |            |  refused
                v            v
         the record,      the rule that failed and
       and notifications  the values that failed it;
                |         nothing changes
                v
      ETRM, ERP, ledger, workflow
```

Your ETRM, ERP, workflow engine and AI platform stay where they are. Morpholog owns the decision boundary for the actions you need to be able to defend.

| Already in your stack | Its job | Morpholog's job beside it |
|---|---|---|
| AI or agent platform | Reason, plan, call tools. | Decide whether what it proposes may be accepted. |
| Policy engine (OPA, Cedar) | Evaluate policy against the context it is given. | Enforce rules over the records as part of the change itself, and keep the history that results. |
| Workflow engine | Order the steps. | Decide whether each step that changes the record is allowed. |
| ETRM or ERP | Run the business. | Leave it running the business; send it each accepted action. |
| PostgreSQL | Store and query data. | Use it as the durable store: Morpholog's tables can live in their own schema in your existing database. |

## What you can answer afterwards

| The question | The answer |
|---|---|
| Can an AI or a service do something it should not? | It can propose it. Nothing it proposes is saved unless the rules allow it. |
| Was this person allowed to act when they acted? | Authority is checked at the moment of the action and recorded with it. Withdrawn tomorrow, it stops tomorrow's actions; today's stay valid. |
| What happens when a figure is corrected later? | The correction changes what may happen next. It does not rewrite what was decided on the old figure. |
| Can we prove all this later? | Any past moment can be rebuilt exactly. Every change records the rules that accepted it. And someone outside your organisation can check that the history was not edited. |

## A rule, read aloud

Morpholog has two building blocks:

- A **business rule** about the records, which Morpholog calls an **invariant**. Every change is checked against it.
- A **governed action**, which Morpholog calls a **transformation**. It is the only way records change: it proposes what to add, what to remove and who to notify. If anything it touches would break a rule, nothing happens.

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

An entry that is off by a penny is refused, with the rule named, and the database looks exactly as it did before. If the business needs an exception, the exception is itself a record the rules govern and the audit trail keeps.

One word before going further. Morpholog calls a record a **claim**: something stated under someone's authority at a particular moment, not a neutral fact. That is why a correction is a new claim that replaces the old one, rather than an edit that erases it.

## What Morpholog gives you

- **A record of every change**: what was proposed, by whom, with which values, what was added and removed, and which rules were checked.
- **Corrections that keep the original.** A correction records what it replaces, so the original figure, the corrected one and the moment one became the other all stay visible. Whether a figure may be relied on is itself a record, granted and withdrawn by named people.
- **A past that cannot shift.** Rules never read the clock or the network; anything the outside world decides, like a rate or a holiday calendar, comes in as a dated record. So any report can be rebuilt exactly as it stood at any past moment.
- **Exact arithmetic.** Decimals with no rounding drift, exact times and durations, and amounts that carry their unit, so dollars never get added to tonnes.
- **Several changes as one decision.** `morpholog transact` saves a group of proposals together, or none of them.
- **Ask before you act.** `explain` runs the same rules against the current records and, when it would refuse, names the rule and the figures it compared, saving nothing: a dry run for an operator's tool, an automated agent or a test.
- **Notifications that respect the save.** Messages to other systems are sent only after a change is saved, by a separate worker, and never for a change that was refused.
- **Rules you can read back.** Ask what the rules forbid (`inspect guarantees`) and what each action requires (`inspect controls`); later, which rules have ever actually done any work (`inspect coverage`), and what evidence a refusal is missing (`explain --json`).
- **A tamper-evident history.** `audit verify` proves the history has not been edited. `audit export` writes a file someone else can check offline, against a 32-byte fingerprint you gave them in advance.

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

Before committing an entry, you can ask whether it would be accepted. `explain` runs the rules against the current records, reports the verdict, and changes nothing; `propose` makes the change. Ask first, then post a journal entry - debit $100 to cash, credit $100 to revenue:

```bash
morpholog explain examples/03_double_entry_ledger/ledger.morph post_simple_entry \
  --actor jordan \
  --args-named '{"entry_id":"entry_001","posting_date":"2026-04-15","period":"q1_2026",
                 "debit_account":"account_cash","credit_account":"account_revenue","amount":"100"}'
```
```text
Admissible: post_simple_entry(entry_001, 2026-04-15, q1_2026, account_cash, account_revenue, 100) proposed by jordan
```
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

The second answer is exactly what an auditor would have seen at that moment, rebuilt from the audit log. An unbalanced entry, a debit of $100 split into credits of $60 and $30, is refused with the rule named, the entry it blames, the two figures that failed to agree, and the line of your rules file it broke:

```bash
morpholog propose examples/03_double_entry_ledger/ledger.morph post_split_entry \
  --actor jordan \
  --args-named '{"entry_id":"entry_002","posting_date":"2026-04-16","period":"q1_2026",
                 "debit_account":"account_cash","debit_amount":"100",
                 "credit_a_account":"account_revenue","credit_a_amount":"60",
                 "credit_b_account":"account_fees","credit_b_amount":"30"}'
```
```json
{
  "status": "rejected",
  "reason": "invariant `balanced_posted_entry` violated",
  "rule": "balanced_posted_entry",
  "witness": [{ "var": "entry", "value": { "type": "subject", "value": "entry_002" } }],
  "compared": { "op": "=", "left": { "type": "decimal", "value": "100" }, "right": { "type": "decimal", "value": "90" } }
}
```

New to Morpholog? [The developer introduction](docs/developer-intro.md) is the hands-on start. Written for someone who knows Python and SQL, it builds a small governed ledger end to end: a reported figure, a decision that relies on it, the honest correction, and the replay that keeps both answers true.

## Start with these examples

Each one runs end to end against PostgreSQL; nothing is mocked.

- [Trade lifecycle](examples/10_trade_lifecycle/) - a commodity trade from capture to settlement. Only a desk with authority for the commodity can confirm it. Each settlement must name the official price in force. A price correction changes the price the next settlement must name, and leaves the earlier settlements standing.
- [Adding Morpholog to an existing trading system](examples/etrm_embedder/) - a Python program standing in for a trading system drives the trade lifecycle through the client Morpholog generates from its rules. The trading system keeps doing its job; Morpholog decides the steps that must be defensible.
- [Margin call run](examples/14_margin_call_run/) - a risk engine proposes the whole day's run. It is accepted only if every account that should be called is in it, no other account is, and every amount is exact. A margin call the engine forgot is refused, not just a wrong one.
- [Verified revenue](examples/02_verified_revenue/) - a figure is approved for a particular use, relied on, then corrected. Every decision made along the way stays defensible.
- [Biometric identification oversight](examples/13_biometric_identification_oversight/) - the EU AI Act's two-person verification, enforced: an AI's match counts for nothing until two distinct verifiers, each authorised when they verified, confirm it.

If you came with a question rather than an industry - "can it accept a whole batch as one decision?" - the [example index](examples/README.md) maps what you want to do to the example that shows it.

## More examples

**Money and markets**

- [Double-entry ledger](examples/03_double_entry_ledger/) - debits equal credits; closing a period; restating; a trial balance at any past moment.
- [Settlement netting](examples/01_settlement_netting/) - settlements that are fine alone but forbidden together are refused.
- [Insurance claim settlement](examples/05_insurance_claim_settlement/) - total payouts can never exceed the policy limit.
- [Borrowing base](examples/11_borrowing_base/) - a loan can never be drawn beyond what its collateral supports.
- [Metered billing](examples/15_metered_billing/) - a bill correct to the penny, every line recomputed and rounded the agreed way.
- [Covenant reporting](examples/17_covenant_reporting/) - a loan's reporting calendar, three calendar months at a time, with overdue notices that must count the days correctly.
- [Laytime and demurrage](examples/12_laytime_demurrage/) - shipping's argument about minutes: exact times, computed deadlines, tonnes of cargo and dollars of delay.

**Evidence, authority and regulation**

- [Approval controls](examples/04_approval_controls/) - authority checked at the moment someone approves, and withdrawn later; approvals given while it held stay valid.
- [Clinical trial enrolment](examples/06_clinical_trial_enrolment/) - protocol, consent and eligibility must all be valid on the day a patient is enrolled.
- [KYC sanctions screening](examples/08_kyc_sanctions_screening/) - onboarding blocked by an out-of-date screening or an unresolved match.
- [Carbon-credit provenance](examples/09_carbon_credit_provenance/) - no green claim without evidence behind it, and no credit counted twice.

**Operations**

- [Operational information](examples/20_operational_information/) - an untrusted optimiser's figures are recomputed and checked before any of them is accepted.
- [Release governance](examples/16_release_governance/) - this project's own release checklist: a release tagged before its checks passed cannot be recorded.
- [Closed-loop execution](examples/21_closed_loop_execution/) - an untrusted agent's orders reach the venue only through a custodied executor, and the venue's own report exposes any order nobody authorised.

**Language features**

- [Scoped charges](examples/18_scoped_charges/) - each charge takes its figure from the right source, and a line with the wrong source's figure cannot be saved.
- [Charging years](examples/19_charging_years/) - a billing period may not cross the 1 April anniversary, and each run records the price list it used.
- [Chess](examples/07_chess_transition_invariants/) - a toy: rules that compare the board before a move with the board after it.

## Status

Active development, in Rust on PostgreSQL 18+, with no unsafe code. The kernel, database adapter, command-line tool, notification worker and every worked example work and are tested end to end.

- **Speed.** A saved change takes about 9ms at worked-example scale. Where a programme's rules compile to SQL and their indexes are in place, a change checks only the records it touches and stays under 10ms from 1,000 to 100,000 records. Other rules are checked in memory and grow with the data (about 1.5s per change at 100,000). A frozen benchmark suite keeps these numbers honest.
- **Integration.** The contract is pinned ([`docs/embedder-integration.md`](docs/embedder-integration.md)), and the binary generates a typed Python client from your rules. An open-source energy-trading system already runs a governed trade lifecycle through it.
- **Not yet built:** a supervisor and HTTP delivery for the notification worker, authority rules that cover whole families of actions, and incremental refresh of derived views. Each arrives when a worked example needs it - the discipline that has kept the runtime small.

## Common questions

**Doesn't this mean writing everything twice?** No. Each governed action is written once, to Morpholog. The systems that need it - the ETRM, the ERP, a reporting store - are kept up to date from the notifications accepted actions emit (retried on transient failure, with keys that make a repeated delivery harmless, and a permanent failure recorded rather than retried forever). One write, then explicit copying, with no two-phase commit. The tables are plain PostgreSQL and can sit in the same database as your application's, in their own schema.

**Can't someone bypass the rules with raw SQL?** With superuser access, yes, as with any database (a DBA can drop a `CHECK` constraint too). Two things limit it. Ordinary permissions let only Morpholog's role write its tables. And the records and the audit log are two accounts of one history, so `audit verify` catches an edit that makes them disagree, or one that rewrites both if you have shared a checkpoint fingerprint outside the database. That fingerprint is 32 bytes: email it to your auditor, or have a public timestamp authority sign it (`audit checkpoint --witness rfc3161:<url>`) so the check can also show *when* the history looked like this. The honest limit: it protects history only up to the last fingerprint you shared, and sharing it is a habit the software cannot enforce.

**Isn't one generic table of records slow?** The table is storage, not the query engine. A rule that compiles to SQL is checked inside the same transaction, only over the records a change touches; other rules are checked in memory over the records the change reads. Either way the in-memory version is the specification, and a test holds the SQL version to it. Heavy querying and reporting use the typed SQL views Morpholog generates, or a downstream copy.

**What about GDPR's right to erasure, if nothing is deleted?** Keep personal data in an ordinary store you can erase, keyed by an opaque id, and keep personal details out of the governed records. The design encourages this but cannot make it automatic. Erasing inside the history cryptographically is a known future direction.

**How does the shape of a record change once history exists?** The same way records change: declare the new shape, carry the data forward with a governed transformation, and history stays as it was recorded. Tooling that automates this is future work; `morpholog migrate` already upgrades Morpholog's own tables between releases.

**Do I have to call a command-line tool for every request?** No. One-off calls are the simple path (about 9ms each). `morpholog session` keeps one process running with your rules loaded and a warm connection, and answers several times faster per call, with the same JSON either way. `morpholog generate python-client` writes a typed, dependency-free Python client from your rules, session support included, stamped with the fingerprint of the rules it was built from. Rust programs use the library directly. A network server and more languages will come when a real integration needs them.

## Deeper reading

[The documentation guide](docs/README.md) maps the rest by what you want to know: what Morpholog is for, how to build on it, exactly what the rules mean, and why each piece exists. It ends with a glossary of Morpholog's words for things you already have.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
