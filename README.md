# Morpholog

Morpholog is a language and runtime for business systems where a record's legitimacy must be provable, not just stored.

You write the rules your records must obey as invariants. The only way state changes is a transformation, and it commits only if every invariant still holds. The runtime makes invalid business state impossible to commit - and keeps an audit trail that proves why each committed change was allowed.

Underneath, Morpholog treats records as **admitted claims**, not freestanding facts: statements admitted under specific authority, at a specific moment. The same figure can carry different *standing* for different decisions - whether it may be relied on, and for what - granted and revoked by different authorities. A verifier can correct a figure without erasing it: the original stays in the books, the corrected figure becomes current. **A decision made under valid standing remains a valid record even when that standing is later revoked** - its legitimacy was settled when it was made. And the audit log carries enough provenance to reconstruct any past moment: no bitemporal columns, no shadow tables.

The point is to make *"how do you know?"* answerable by *"because the system could not have admitted it otherwise."*

That gate runs forward as well as back. Whatever decides what to do next - a person, an optimiser, a heuristic, an AI model - only ever *proposes* a change; the runtime is what admits it or refuses. So the thing making the decision does not have to be trusted. You can put untrusted intelligence to work precisely because legitimacy is enforced outside it, never asked of it.

And when it refuses, it says why. A rejected proposal comes back as a structured, reproducible account - the gate or invariant that failed and, where the cause is a missing claim, that claim and the candidate transformations that could supply it - built from your own predicate and transformation names, never free text. That turns the gate into something an automated searcher can *work against*, not just bounce off. A solver or a model proposes, reads the refusal, repairs its own candidate, and tries again - and whatever finally commits did so by satisfying the same rules a human change would have to.

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

## Behaviour you can't drift from

Behaviour-driven development asks you to write the behaviour first - *given* this state, *when* someone does that, *then* this must hold - and to keep the system honest against it. The catch is that the scenarios and the code are separate artifacts, so they drift, and the glue between them quietly goes stale.

In Morpholog the behaviour *is* the system. An invariant and a transformation's gates are the *given / when / then*: given the admitted claims, when an actor proposes a change, then it commits only if every rule still holds. There is no second implementation to drift from - the specification is the runtime's admission law, enforced on every change, whoever or whatever proposed it. **Morpholog delivers what BDD promises - an executable behavioural specification that cannot drift from the system - without the ceremony.** `morpholog explain` turns a rejected proposal into the failing-scenario report; `morpholog inspect guarantees` lists what the rules make impossible before anything runs.

## A sixty-second tour

One-time setup (PostgreSQL 17+):

```bash
createdb my_books
psql my_books -f crates/morpholog-core/sql/schema.sql
export DATABASE_URL=postgres:///my_books
```

Post a journal entry - debit $100 to cash, credit $100 to revenue:

```bash
morpholog run examples/03_double_entry_ledger/ledger.morph post_simple_entry \
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

The report is exactly what an auditor would have seen at that moment - recomputed from the audit log, no bitemporal columns anywhere in your schema. (`--as-of` also accepts an RFC 3339 timestamp, resolved to the last commit at or before it.)

And an unbalanced entry is refused atomically, with the rule named:

```json
{ "status": "rejected", "reason": "invariant `balanced_posted_entry` violated" }
```

## Worked examples

Each runs both in memory (against the kernel) and durably (against PostgreSQL). All parse end-to-end as `.morph` source. Nothing is mocked.

- [**Bilateral settlement netting**](examples/01_settlement_netting/) - invariants check the *candidate state*, not the pre-state. Transformations that combine individually-valid inputs into a forbidden whole are rejected before any commit.
- [**Verified revenue**](examples/02_verified_revenue/) - the flagship. A figure is independently verified; different authorities grant *standing* for it to be relied on for their own decisions. The verifier can correct it later; the original stays in the books; **active standings on the prior figure are retracted automatically**; **historical decisions made under the original remain valid records of what was decided that day.** Defending a contested number over time, in one example.
- [**Double-entry ledger with trial balance**](examples/03_double_entry_ledger/) - the accounting equation as an invariant; period close as an admission gate; restatement that preserves the original; the trial balance as a **governed read-side projection**. As-of replay yields the historical trial balance without a bitemporal column in the schema.
- [**Approval controls**](examples/04_approval_controls/) - unconditional and quantitative authority. The approving actor flows from transition context; the asserted record carries them. Revocation prevents future approvals while preserving every historical one.
- [**Insurance claim settlement**](examples/05_insurance_claim_settlement/) - cumulative settlements against a policy aggregate limit, evaluated at admission as `paid_so_far + proposed_settlement <= aggregate_limit`. Same audit-grade evidence regime as the other examples.
- [**Clinical trial enrolment**](examples/06_clinical_trial_enrolment/) - a participant is randomised only if protocol, consent, investigator delegation and eligibility evidence are all valid on the randomisation date. **Validity is admission-time, not eternal**: a later protocol amendment does not invalidate an earlier randomisation. Grounded in Good Clinical Practice.
- [**Chess transition invariants**](examples/07_chess_transition_invariants/) - the one non-business example: an isolated demonstration of *transition invariants*, rules that relate the state before a change to the state after (move count strictly increases; piece count never rises). The same `pre(...)` mechanism enforces a real per-policy conservation law in the insurance example.
- [**KYC sanctions and PEP screening**](examples/08_kyc_sanctions_screening/) - onboarding is gated by current, clean screenings against both the sanctions and PEP lists; an unresolved match on *any* of a customer's screenings blocks admission until it is adjudicated. Each step emits a **declared intent** to a distinct downstream consumer - misspelling `MatchRaised` is a validation error, not a silently-dropped compliance event.
- [**Carbon-credit provenance**](examples/09_carbon_credit_provenance/) - the explanation engine's flagship: a green claim becomes official only on an admissible, *current* provenance chain - verified measurement, attestation, an accredited verifier - with double-counting forbidden in both directions, single custody, and terminal retirement. Retire-by-deadline obligations are swept by an outside coordinator that hands the date in. **No green claim without admissible provenance.**
- [**Trade lifecycle**](examples/10_trade_lifecycle/) - a commodity trade through capture, confirmation, official-price correction, and settlement, with the **phase modelled as accumulated admitted claims, not a status field**. The confirmation event is split from the official price figure, so a later price correction is a restatement that moves the in-force pointer and leaves a settlement made under the prior figure standing. A trade can never be settled for more than the quantity captured. Morpholog's first **external-embedder** target.
- [**Borrowing base**](examples/11_borrowing_base/) - asset-backed lending: the total drawn against a facility can never exceed its **advance rate times the eligible collateral**, and a read-side view reports each facility's drawn-to-collateral utilisation. The example that completes the decimal arithmetic - the advance-limit invariant **multiplies** (cross-multiplied so the decision is exact), the utilisation projection **divides**.
- [**Laytime and demurrage**](examples/12_laytime_demurrage/) - voyage chartering's argument about minutes, and the example that forced Morpholog's time values. Notice of Readiness at an exact instant; laytime commencement **computed** by shifting that instant by the agreed turn time; the Statement of Facts as counting intervals whose recorded lengths can never disagree with their ends; and time on demurrage as a **derived excess, floored at zero** - because running over the allowance is the normal priced outcome, deliberately *not* an invariant violation. All instants are zone-less UTC by design: port-local days arrive as admitted claims in this example's next stage, never as a hidden runtime timezone database.

Morpholog is not the whole stack. UIs, dashboards, dataloaders, ML pipelines stay in normal tools. What Morpholog owns is the line where *"may this be admitted as a valid record?"* needs a definite answer - the small fraction of any real business system that, when it fails, makes the news.

## Try it yourself

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog
cargo install --path crates/morpholog-cli
morpholog check examples/01_settlement_netting/netting.morph
```

`check` parses the file and runs `Program::validate()` against the IR - one command answers "is this program well-formed?" `parse` prints the parsed `Program` as JSON when you want to see the structure. `run` and `inspect` take a `.morph` file path and run against PostgreSQL once `DATABASE_URL` is set; see [`CONTRIBUTING.md`](CONTRIBUTING.md) for local-database setup.

New to Morpholog? [The developer introduction](docs/developer-intro.md) is the gentle, hands-on place to start - written for a developer who knows Python and SQL, it builds a small governed ledger end to end: a reported figure, a decision that relies on it, the honest correction, and the as-of replay that keeps both answers true.

## Status

Active development. Kernel, PostgreSQL adapter, CLI, polling outbox worker, and worked examples are all working and tested. Every committed transition records its actor. Writes scale linearly (~1.6s per commit at 100,000-entry scale); as-of replay also linear (~1.5s for 100,000 transitions). Predicate-scoped loading on both read and write paths means a transformation only loads claims it actually consults.

The `.morph` parser arc is complete: every worked example parses end-to-end as `.morph` source. The formatter and parser are coupled by a round-trip property test. Diagnostics surface through `ariadne` with source spans.

Built in Rust on PostgreSQL 17+. The kernel is `#[forbid(unsafe_code)]`; the PG adapter leans on SERIALIZABLE isolation and JSONB so an entire commit lands atomically or not at all.

The legibility tooling has begun: `morpholog inspect guarantees` names what a model makes impossible, and `morpholog explain` turns a rejection into a missing-evidence checklist with the transformations that could supply each gap. What is *not* in the box yet: a worker supervisor with circuit breakers and an HTTP-aware deliverer; predicate-pattern matching for higher-order authority; materialised derived claims; the rest of the legibility set (transformation graphs, mechanically-derived exclusion matrices). Each lands when a worked example forces the shape.

## Common questions

**Doesn't a separate system of record create a dual-write problem?** It removes the classic transactional one. Morpholog holds the governed records; downstream stores - analytics tables, caches, search indexes - are derived copies, kept current by consuming the intents each commit emits (at-least-once, with idempotency keys). One write, explicit propagation, no two-phase commit; the projection pipeline is still yours to operate, but it is one system propagating, not two writers pretending to be co-primary. And the schema is plain PostgreSQL: it can share a database with your application's tables, behind its own schema and role.

**Can't someone bypass the rules with raw SQL?** With superuser access, yes - as with any database-backed system of record (a DBA can drop a `CHECK` constraint too). The mitigations are ordinary privilege separation - only the runtime's role writes the `morpholog` schema - plus one the model adds: the claims table and the audit log are two records of one history. `morpholog verify` detects edits that leave the two disagreeing - replaying one against the other, from a single database snapshot, and naming the difference. A *coordinated* edit of both records is what the next hardening step, a hash-chained audit log, addresses.

**Isn't a generic claims table an EAV performance trap?** The claims table is storage, not the query engine. Rules are never evaluated as SQL self-joins: the runtime loads only the claims whose predicates a transformation touches, and the kernel evaluates in memory. The measured shape is linear (the Status numbers above), and the intended workload is the admission line of governed records, not general OLTP - that boundary is the design.

**What about GDPR's right to erasure, if nothing is ever deleted?** Opaque subjects reduce the problem rather than solve it. The pattern: keep personal data in an ordinary, erasable store keyed by subject id, so erasure deletes the mapping - and keep personal details out of claim contents, which is a modelling discipline, not a free lunch. Redaction within history itself (per-subject crypto-shredding) is a recognised future direction.

**Why not OPA, Datomic, or Datalog?** Neighbouring problems. OPA decides policy but owns no state, no commit, no audit, no replay. Datomic and XTDB keep immutable history, but rules are not admission law and refusals are not explained. Datalog derives; it does not gate transactions. Morpholog is the combination - admission law, atomic commit, audit-as-store, replay, explanation - in one small kernel on plain PostgreSQL. [`docs/prior-art.md`](docs/prior-art.md) has the longer comparison.

**How does a predicate evolve once history exists?** By supersession, the same way records do: declare the new shape, carry forward with a governed transformation, and history stands as recorded. First-class migration tooling is future work.

**Do I have to shell out to a CLI on every request?** Today the CLI is the non-Rust integration surface (~9ms per call, measured); Rust embeds the library in-process. The JSON contract is the interface - a long-running server mode and native language clients are contemplated next steps, and they would wrap the same contract unchanged.

The [developer introduction](docs/developer-intro.md) closes with a longer-form version of these questions.

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, the affordances on the design horizon, and non-goals.
- [`docs/roadmap.md`](docs/roadmap.md) - what's imminent, deferred, and out of scope.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - what the kernel means.
- [`docs/design-history.md`](docs/design-history.md) - for each significant runtime/IR decision, which worked example forced it.
- [`docs/outbox-sketch.md`](docs/outbox-sketch.md) - the "Morpholog plus an Outside Coordinator" doctrine for the outbox worker.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
