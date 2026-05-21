# Morpholog

There comes a moment in every serious business when someone - an auditor, a regulator, a board director who has not been having a great quarter - asks you to *prove* a number. Why is it what it is. Who decided it should be. What it looked like before the restatement. Whether the rule it had to satisfy actually held at the moment it was admitted.

In most systems, those answers come from detective work. You pull logs, reconcile parallel files, ask the people who happened to be on call.

Morpholog answers them differently. It treats records as **admitted claims**, not objective facts. The same number can carry different *standing* for different decisions, granted and revoked by different authorities. The verifier can correct the figure; the original stays in the books and the corrected figure becomes current. **Decisions admitted under valid standing remain valid records even when that standing is later revoked** - the legitimacy of a *past* decision was established when it was made. Behind that: every change is checked against the rules atomically; nothing sticks unless every rule holds; the audit log carries enough provenance to reconstruct any past moment - no bitemporal columns, no shadow tables.

The point is to make *"how do you know?"* answerable by *"because the system could not have admitted it otherwise."*

## The questions you can answer

- *Why does this report differ from the one we filed last quarter?*
- *Who admitted this entry, and under what authority?*
- *If that authority was rescinded yesterday, was yesterday's decision still valid?*
- *What did the books say on the last day of Q1, under the close rules in force then?*
- *Did this trade conform to our exposure limits when it was booked? Not now - then?*

Morpholog answers a meaningful subset of these today, against a real PostgreSQL database, through a small CLI. The double-entry-ledger example handles questions 1 and 4 through its trial balance and as-of replay. The verified-revenue example handles 2 and 3. Question 5 awaits an exposure-limit programme - the runtime would handle it the same way.

## What you get for writing your rules this way

- **The same record can carry different standing for different decisions.** Granted and revoked by different authorities, without modifying the underlying record. The shape regulated lending and statutory reporting need - and the genuinely distinctive thing in this list.
- **Correction without overwriting.** Originals stay admitted; a new transformation asserts the correction and records the lineage. An auditor three quarters later sees the original number, the corrected number, and the moment one became the other.
- **A complete audit of every change.** Each committed change writes a row carrying the transformation, the actor, the arguments, the asserted and retracted records, the emitted notifications, and the rules that governed admission.
- **Atomic commit or full rollback.** A change either lands every part of itself - record edits, audit row, outbound notifications - or none of them. PostgreSQL's `SERIALIZABLE` isolation does the work; the runtime guarantees it.
- **Reports computed by the same rules they're governed by.** A trial balance, an exposure summary, an account balance - declared alongside the rules, computed by the same kernel. A report cannot show a record an invariant would have refused.
- **Notifications that respect the commit boundary.** Outbound side effects stage at commit and deliver afterward through a worker. They never run inside the transaction; they never run if the commit rolled back.
- **Arbitrary-precision decimals.** No floating-point drift. Adds, subtracts, sums - all exact.

## How it works

Two first-class constructs. Everything else is built from these.

An **invariant** says what must always be true of admitted records. A **transformation** is the only path that gets to change them. It proposes additions, removals, and outbound notifications; the runtime checks every active invariant against the proposed result; if anything fails, nothing happens.

In the double-entry ledger example (illustrative surface syntax - the parser is on the roadmap; programs are Rust IR today):

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

The invariant says debits and credits must balance for every posted entry. The transformation is the only way a journal entry can enter the books, and only when the period is open and the result balances. If the math is off by a penny, the transaction is rejected atomically.

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

The receipt:

```json
{
  "status": "committed",
  "transition_id": "019231ab-...-...-...-...-...",
  "actor": {"type":"subject","value":"jordan"},
  "asserted_claims": [ /* the journal entry and its two lines */ ],
  "retracted_claims": [],
  "emitted_intents": [ {"name":"JournalEntryPosted","args":[...]} ]
}
```

`--actor` records under whose authority the transition was proposed; it lands in the audit row. Three months from now the answer to "who admitted this?" is that row.

Now the trial balance:

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

Post another entry, then ask for the trial balance *as it stood right after the first transition*:

```bash
morpholog inspect derived double_entry_ledger TrialBalanceRow \
    --as-of 019231ab-...-...-...-...-...
```

The report is exactly what an auditor would have seen at that moment - recomputed from the audit log, no bitemporal columns anywhere in your schema.

An unbalanced entry would come back as `{"status":"rejected","reason":"balanced_posted_entry invariant did not hold"}`, and the database would look exactly the way it did before the attempt.

## Worked examples

Each runs both in memory (against the kernel) and durably (against PostgreSQL). Nothing is mocked.

- [**Bilateral settlement netting**](examples/01_settlement_netting/) - invariants check the *candidate state*, not just the pre-state. A transformation that would create an inconsistency by combining individually-valid inputs is rejected before any commit.
- [**Verified revenue**](examples/02_verified_revenue/) - the flagship. A revenue figure is independently verified, and different authorities grant *standing* for it to be relied on for their own decisions. The verifier can correct the figure later; the original stays in the books; **active standings on the prior figure are retracted by the runtime** so future reliance must attach to the corrected one; **historical decisions made under the original figure remain valid records of what was decided that day.** Defending a contested number over time, in one example.
- [**Double-entry ledger with trial balance**](examples/03_double_entry_ledger/) - the accounting equation as an invariant; period close as an admission gate; restatement that preserves the original; the trial balance as a **governed read-side projection**. As-of replay yields the historical trial balance without a bitemporal column in the schema.
- [**Approval controls**](examples/04_approval_controls/) - unconditional authority for sign-offs that aren't about amounts, and quantitative authority for amount-sensitive approvals. The approving actor flows from transition context; the asserted record carries them. Revocation prevents future approvals while preserving every historical one.
- [**Insurance claim settlement**](examples/05_insurance_claim_settlement/) - an insurer authorises claim payments against a policy. Each settlement consumes from a cumulative aggregate limit; once exhausted, no further settlements admit. **The cap is evaluated at admission as `paid_so_far + proposed_settlement <= aggregate_limit`**, with the same audit-grade evidence regime as the other examples - who decided, under what authority, against which policy, and what the aggregate position was at that moment.
- [**Clinical trial enrolment**](examples/06_clinical_trial_enrolment/) - a participant may be randomised only if the protocol version, consent form, investigator delegation and eligibility evidence are all valid on the randomisation date. **Validity is an admission-time gate, not an eternal invariant**: a later protocol amendment does not invalidate the earlier randomisation, but a participant enrolled after the old window closed must satisfy the new one. The first non-finance worked example, grounded in Good Clinical Practice evidence requirements; forces civil-date comparison and inclusive `[from, to]` window semantics into the kernel.

Morpholog isn't the whole stack - UIs, dashboards, dataloaders, ML pipelines stay in normal tools. What it owns is the line where "may this be admitted as a valid record?" needs a definite answer. That line is a small fraction of any real business system, and the fraction that, when it fails, makes the news. The framing of what Morpholog should grow into - and what it must never become - lives in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md).

---

## Project status

Active. Kernel, PostgreSQL adapter, CLI, polling outbox worker, and worked examples all work and are tested. Every committed transition records its actor. Writes scale linearly (~1.6s per commit against a 100,000-entry ledger); as-of replay also linear (~1.5s for 100,000 transitions). See [`crates/morpholog-bench/README.md`](crates/morpholog-bench/README.md) for the performance story.

Not in the box yet: a parser; a worker supervisor with circuit breakers and an HTTP-aware deliverer; predicate-pattern matching / higher-order authority (quantitative authority works today via `Expr::Le`); user-supplied program loading; materialised derived claims. Each lands when a worked example forces the shape.

Built in Rust on PostgreSQL 17+. The kernel is `#[forbid(unsafe_code)]`; the PG adapter leans on SERIALIZABLE isolation and JSONB so an entire commit lands atomically or not at all.

To run the tests:

```bash
cargo test -p morpholog-core --all-targets
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --all-targets -- --test-threads=1
```

Existing databases need the migrations under `crates/morpholog-core/sql/migrations/` applied in numeric order. Fresh installations get the head schema from `crates/morpholog-core/sql/schema.sql`.

The workspace: `morpholog-core` (sync kernel, no I/O), `morpholog-examples` (worked-example IR + registry), `morpholog-postgres` (async adapter and read helpers), `morpholog-outbox` (polling worker), `morpholog-cli` (the `morpholog` binary), `morpholog-bench` (scale-pressure benchmark).

## Where this is heading

The runtime today is operationally complete enough to defend a number; the next arc is making it operationally complete enough to defend an *organisation*. Framed in business shapes, not feature names:

- **A worker supervisor with circuit breakers and an HTTP-aware deliverer.** The polling worker exists and ships a `StdoutDeliverer`; what's missing is a supervisor running multiple workers under restart-with-intensity, per-target circuit breakers, and an `HttpDeliverer`.
- **Predicate-pattern matching and higher-order authority.** Quantitative authority works today (see [Approval Controls](examples/04_approval_controls/)). The next shape is *one* authority claim governing a *family* of transformations, instead of one claim per kind. Forces predicate names as first-class IR values.
- **Effective time as a first-class temporal axis.** As-of already gives *knowledge* time. Effective time - the day a contract becomes binding, the period a posting reflects - is expressible as ordinary claims; combining the two gives full bitemporal addressability without any `valid_from`/`valid_to` columns.
- **A surface syntax and parser.** Programs are Rust IR today. A parser commits to a dozen choices (file layout, module system, error spans, literal syntax) that should be ratified by a real outside user, not pre-decided.
- **Materialised derived claims.** Reports are recomputed on demand. For long audit logs and frequent queries, materialised snapshots will become forced.

None of these are speculative; each has a concrete forcing scenario in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) or [`docs/design-history.md`](docs/design-history.md). Each lands when an example actually demands it.

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, the affordances on the roadmap, the three-level expansion ladder, and non-goals.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - semantics the `morpholog-core` kernel realises.
- [`docs/design-history.md`](docs/design-history.md) - for each significant runtime/IR decision, which worked example forced it and why.
- [`docs/outbox-sketch.md`](docs/outbox-sketch.md) - the "Morpholog plus an Outside Coordinator" doctrine for the outbox worker.
- Worked examples: [`examples/01_settlement_netting/`](examples/01_settlement_netting/), [`examples/02_verified_revenue/`](examples/02_verified_revenue/), [`examples/03_double_entry_ledger/`](examples/03_double_entry_ledger/), [`examples/04_approval_controls/`](examples/04_approval_controls/), [`examples/05_insurance_claim_settlement/`](examples/05_insurance_claim_settlement/), [`examples/06_clinical_trial_enrolment/`](examples/06_clinical_trial_enrolment/).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
