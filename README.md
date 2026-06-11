# Morpholog

Morpholog is a language and runtime for the records a business must be able to defend - the ones where legitimacy has to be provable, not just stored.

Most software assumes its records are legitimate. Morpholog enforces it. You write the rules your records must obey as **invariants**. The only way state changes is a **transformation**, and it commits only if every invariant still holds. Invalid business state is not caught later or flagged for review - it is impossible to commit. And every committed change leaves an audit trail that proves why it was allowed.

Underneath, Morpholog treats records as **admitted claims**, not freestanding facts: statements admitted under specific authority, at a specific moment. The same figure can carry different *standing* for different decisions - whether it may be relied on, and for what - granted and revoked by different authorities. A correction supersedes; it never erases: the original stays in the books, the corrected figure becomes current. **A decision made under valid standing remains a valid record even when that standing is later revoked** - its legitimacy was settled when it was made. And the audit log carries enough provenance to reconstruct any past moment, with no bitemporal columns and no shadow tables.

None of these ideas is new. Rules as admission law, authority you can grant and revoke, history that supersedes instead of erasing, audit that proves instead of asserts - each has decades of theory behind it. What did not exist is all of them as **one small runtime on plain PostgreSQL**, enforcing legitimacy at the moment of commit. That synthesis is Morpholog.

The point is to make *"how do you know?"* answerable by *"because the system could not have admitted it otherwise."*

That gate matters more, not less, as decisions stop being made only by people. Whatever decides what to do next - a person, an optimiser, an AI model - only ever *proposes* a change; the runtime admits it or refuses. The thing making the decision does not have to be trusted, because legitimacy is enforced outside it, never asked of it. And when the runtime refuses, it says why: a structured, reproducible account of the gate or rule that failed and the evidence that is missing, built from your own predicate names, never free text. A solver or a model can propose, read the refusal, repair its candidate, and try again - and whatever finally commits did so by satisfying the same rules a human change would have to. **You can put untrusted intelligence to work on governed records, safely.**

## The questions you can answer

- *Why does this report differ from the one we filed last quarter?*
- *Who admitted this entry, and under what authority?*
- *If that authority was rescinded yesterday, was yesterday's decision still valid?*
- *What did the books say on the last day of Q1, under the close rules in force then?*
- *Did this trade conform to our exposure limits when it was booked? Not now - then?*
- *On what basis did the AI system identify this person, who verified it, and were they authorised that day?*

The worked examples below answer concrete instances of these against a real PostgreSQL database, through a small CLI.

## What you get for writing your rules this way

- **The same record can carry different standing for different decisions** - granted and revoked by different authorities, without modifying the underlying record. The shape regulated lending and statutory reporting need.
- **Correction without overwriting.** Originals stay admitted; a new transformation asserts the correction and records the lineage. An auditor three quarters later sees the original, the corrected figure, and the moment one became the other.
- **A complete audit of every change** - the transformation, the actor, the arguments, the asserted and retracted records, the emitted notifications, and the rules that governed admission.
- **Atomic commit or full rollback.** A change either lands every part of itself - records, audit row, outbound notifications - or none of them. PostgreSQL's `SERIALIZABLE` isolation does the work.
- **Reports computed by the same rules they're governed by.** A trial balance, an exposure summary - declared alongside the rules, computed by the same kernel. A report cannot show a record an invariant would have refused.
- **Notifications that respect the commit boundary.** Outbound side effects stage at commit and deliver afterward through a worker - never inside the transaction, never if the commit rolled back.
- **Exact numbers, exact time, honest units.** Arbitrary-precision decimals with no floating-point drift; instants and durations that shift, difference, and sum exactly; amounts that know their unit (`Decimal[USD]`, `Decimal[t]`), so money never meets tonnes by accident.

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

## Behaviour you can't drift from

Behaviour-driven development asks you to write the behaviour first - *given* this state, *when* someone does that, *then* this must hold - and to keep the system honest against it. The catch is that the scenarios and the code are separate artifacts, so they drift.

In Morpholog the behaviour *is* the system. An invariant and a transformation's gates are the *given / when / then*, enforced on every change, whoever or whatever proposed it. **Morpholog delivers what BDD promises - an executable behavioural specification that cannot drift from the system - without the ceremony.** `morpholog explain` turns a rejected proposal into the failing-scenario report; `morpholog inspect guarantees` lists what the rules make impossible before anything runs.

## A sixty-second tour

One-time setup (PostgreSQL 17+):

```bash
createdb my_books
export DATABASE_URL=postgres:///my_books
morpholog init
```

`init` provisions Morpholog's schema from a copy embedded in the binary - a
deployment carries exactly the schema its build expects, nothing to vendor.

Post a journal entry - debit $100 to cash, credit $100 to revenue:

```bash
morpholog propose examples/03_double_entry_ledger/ledger.morph post_simple_entry \
  --actor jordan \
  --args-named '{"entry_id":"entry_001","posting_date":"2026-04-15","period":"q1_2026",
                 "debit_account":"account_cash","credit_account":"account_revenue","amount":"100"}'
```

The receipt carries the transition id, the actor, the asserted claims, and the emitted intents. `--actor` records under whose authority the transition was proposed; the audit row carries it for the next three years.

Now the trial balance, then the trial balance *as it stood right after the first transition*:

```bash
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow --as-of <transition_id>
```

The report is exactly what an auditor would have seen at that moment - recomputed from the audit log, no bitemporal columns anywhere in your schema. (`--as-of` also accepts an RFC 3339 timestamp.)

And an unbalanced entry is refused atomically, with the rule named - and its exact line in your source pointed at on stderr:

```json
{ "status": "rejected", "reason": "invariant `balanced_posted_entry` violated" }
```

## Worked examples

Each runs both in memory (against the kernel) and durably (against PostgreSQL). All parse end-to-end as `.morph` source. Nothing is mocked.

- [**Bilateral settlement netting**](examples/01_settlement_netting/) - invariants check the *candidate state*, not the pre-state: inputs that are individually valid but forbidden together are rejected before any commit.
- [**Verified revenue**](examples/02_verified_revenue/) - the flagship. A figure is verified, relied on, then corrected: the original stays in the books, standings on it are retracted automatically, and **decisions made under it remain valid records of what was decided that day**. Defending a contested number over time, in one example.
- [**Double-entry ledger with trial balance**](examples/03_double_entry_ledger/) - the accounting equation as an invariant, period close as a gate, restatement that preserves the original, and the trial balance as a governed read-side projection with as-of replay.
- [**Approval controls**](examples/04_approval_controls/) - unconditional and quantitative authority; revocation prevents future approvals while preserving every historical one.
- [**Insurance claim settlement**](examples/05_insurance_claim_settlement/) - cumulative settlements against a policy aggregate limit, evaluated exactly at admission.
- [**Clinical trial enrolment**](examples/06_clinical_trial_enrolment/) - randomisation requires protocol, consent, delegation and eligibility all valid *on the randomisation date*. **Validity is admission-time, not eternal**: a later amendment does not invalidate an earlier enrolment.
- [**Chess transition invariants**](examples/07_chess_transition_invariants/) - the one non-business example: rules that relate the state before a change to the state after (move count strictly increases). The same `pre(...)` mechanism enforces a conservation law in the insurance example.
- [**KYC sanctions and PEP screening**](examples/08_kyc_sanctions_screening/) - onboarding gated by current, clean screenings; an unresolved match blocks until adjudicated; every step emits a **declared intent**, so a misspelled compliance event is a validation error, not a silent drop.
- [**Carbon-credit provenance**](examples/09_carbon_credit_provenance/) - the explanation engine's flagship: **no green claim without admissible provenance** - verified measurement, accredited verifier, double-counting forbidden in both directions, single custody, terminal retirement.
- [**Trade lifecycle**](examples/10_trade_lifecycle/) - a commodity trade from capture to settlement, with the **phase modelled as accumulated claims, not a status field**: a price correction is a restatement that leaves prior settlements standing. The first external-embedder target.
- [**Borrowing base**](examples/11_borrowing_base/) - asset-backed lending: drawn amounts can never exceed the advance rate times eligible collateral, cross-multiplied so the decision is exact.
- [**Laytime and demurrage**](examples/12_laytime_demurrage/) - voyage chartering's argument about minutes, and the example that forced Morpholog's time values and units: exact instants, computed deadlines, interval counting, cargo in tonnes, delay priced in dollars - and time on demurrage as a **derived excess, deliberately not a violation**, because running over the allowance is the normal priced outcome.
- [**Biometric identification oversight (EU AI Act Article 12)**](examples/13_biometric_identification_oversight/) - a statute as the forcing catalyst: the AI system's output enters as a claim with **no standing**; standing comes only from verification under live, revocable human authority; a decision without **two distinct verifications cannot commit**. It forced no new language - four earlier patterns meeting a statute - which is the headline. Its README maps each statute clause to the rule enforcing it, regenerable as `morpholog inspect controls`.

Morpholog is not the whole stack. UIs, dashboards, dataloaders, ML pipelines stay in normal tools. What Morpholog owns is the line where *"may this be admitted as a valid record?"* needs a definite answer - the small fraction of any real business system that, when it fails, makes the news.

## Try it yourself

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
cargo install --path crates/morpholog-cli
morpholog check examples/01_settlement_netting/netting.morph
```

`check` parses and validates - one command answers "is this program well-formed?", and any finding arrives with a caret pointing at the exact line. `propose` and `inspect` take a `.morph` file path and run against PostgreSQL once `DATABASE_URL` is set; see [`CONTRIBUTING.md`](CONTRIBUTING.md) for local-database setup.

New to Morpholog? [The developer introduction](docs/developer-intro.md) is the gentle, hands-on place to start - written for a developer who knows Python and SQL, it builds a small governed ledger end to end: a reported figure, a decision that relies on it, the honest correction, and the as-of replay that keeps both answers true.

## Status

Active development. Kernel, PostgreSQL adapter, CLI, polling outbox worker, and the worked examples are all working and tested; built in Rust on PostgreSQL 17+, `#[forbid(unsafe_code)]` throughout. A governed commit is ~9ms end to end at worked-example scale, and writes scale linearly from there (~1.6s per commit with 100,000 in-scope claims); as-of replay is also linear. The `.morph` parser arc is complete, with every diagnostic - parse, validation, lint - caret-located in source.

The legibility tooling has begun: `inspect guarantees` names what a model makes impossible, `inspect controls` renders what must be true before each action beside what can never be true, `explain` turns a rejection into a missing-evidence checklist with the transformations that could supply each gap, `inspect coverage` replays history and reports which rules have ever actually done work (a rule that has never matched anything is dead text wearing an invariant's name - now it gets named), and `verify` replays the audit log against the claims table and names any divergence.

The integration surface is a pinned contract ([`docs/embedder-integration.md`](docs/embedder-integration.md)): JSON-Schema contracts for every transformation and intent, named-argument codecs in both directions, batch import, schema provisioning from the binary, a canonical hash that identifies the rules in force, machine-readable diagnostics and outcome envelopes - and a **typed Python client generated by the binary itself** (`morpholog generate python-client`), so an embedder speaks exactly the contract its binary speaks. An external open-source energy-trading system already drives a governed trade lifecycle through it end to end.

What is *not* in the box yet: a worker supervisor with circuit breakers and an HTTP-aware deliverer; a tamper-evident (Merkle history tree) audit log with exportable evidence packs; higher-order authority; materialised derived claims. Each lands when a worked example forces the shape - the discipline that has kept the kernel small.

## Common questions

**Doesn't a separate system of record create a dual-write problem?** It removes the classic transactional one. Morpholog holds the governed records; downstream stores are derived copies, kept current by consuming the intents each commit emits (at-least-once, with idempotency keys). One write, explicit propagation, no two-phase commit. And the schema is plain PostgreSQL: it can share a database with your application's tables, behind its own schema and role.

**Can't someone bypass the rules with raw SQL?** With superuser access, yes - as with any database-backed system of record (a DBA can drop a `CHECK` constraint too). The mitigations are ordinary privilege separation - only the runtime's role writes the `morpholog` schema - plus one the model adds: the claims table and the audit log are two records of one history, and `morpholog verify` detects edits that leave them disagreeing. A *coordinated* edit of both is what the planned tamper-evident audit log addresses.

**Isn't a generic claims table an EAV performance trap?** The claims table is storage, not the query engine. Rules are never evaluated as SQL self-joins: the runtime loads only the claims whose predicates a transformation touches, and the kernel evaluates in memory. The measured shape is linear (the Status numbers above), and the intended workload is the admission line of governed records, not general OLTP - that boundary is the design.

**What about GDPR's right to erasure, if nothing is ever deleted?** Opaque subjects reduce the problem rather than solve it. The pattern: keep personal data in an ordinary, erasable store keyed by subject id, so erasure deletes the mapping - and keep personal details out of claim contents, which is a modelling discipline, not a free lunch. Redaction within history itself (per-subject crypto-shredding) is a recognised future direction.

**Why not OPA, Datomic, or Datalog?** Neighbouring problems. OPA decides policy but owns no state, no commit, no audit, no replay. Datomic and XTDB keep immutable history, but rules are not admission law and refusals are not explained. Datalog derives; it does not gate transactions. Morpholog is the combination - admission law, atomic commit, audit-as-store, replay, explanation - in one small kernel on plain PostgreSQL. [`docs/prior-art.md`](docs/prior-art.md) has the longer comparison.

**How does a predicate evolve once history exists?** By supersession, the same way records do: declare the new shape, carry forward with a governed transformation, and history stands as recorded. First-class migration tooling is future work.

**Do I have to shell out to a CLI on every request?** The CLI is the non-Rust integration surface (~9ms per call, measured), and you don't write the integration by hand: `morpholog generate python-client` emits a complete, typed, dependency-free Python client from your own programme - request models, envelope parsing, the whole pinned contract - stamped with the hash of the rules it was built against. Rust embeds the library in-process. A long-running server mode and more languages follow when an embedder forces them, wrapping the same contract unchanged.

The [developer introduction](docs/developer-intro.md) closes with a longer-form version of these questions.

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, the affordances on the design horizon, and non-goals.
- [`docs/roadmap.md`](docs/roadmap.md) - what's imminent, deferred, and out of scope.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - what the kernel means.
- [`docs/embedder-integration.md`](docs/embedder-integration.md) - the pinned contract for integrating from any language.
- [`docs/design-history.md`](docs/design-history.md) - for each significant runtime/IR decision, which worked example forced it.
- [`docs/prior-art.md`](docs/prior-art.md) - the decades of theory this synthesises, and what we deliberately rejected.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
