# Morpholog

A runtime that owns the legitimacy boundary of your business records. Every entry that enters governed state is checked against every active rule by the language itself - not by validators someone remembered to write, not by reconciliation scripts run nightly, not by tribal knowledge in the heads of senior staff. If the rules say no, nothing is written and nothing is sent. If they say yes, the record is part of an append-only audit log that any auditor can query, including as it stood after any committed transition.

## The questions Morpholog answers

A controller, an auditor, or a risk officer asks these questions of every serious business system. Most systems answer them poorly:

- *Why does this report differ from the one we filed three months ago?*
- *Who authorised this entry, and under what limit?*
- *If the authorisation we relied on was rescinded yesterday, was yesterday's decision still legitimate?*
- *What did the books say at quarter-end, under the close rules in force then?*
- *Did this trade conform to our exposure limits at the moment it was booked - not now, then?*

Conventional answers involve detective work: searching log tables, reconciling parallel systems, asking colleagues who happened to be on shift. Some questions never get a clean answer; the books just stop tying out, and accountants learn to live with that.

Morpholog makes those answers part of the substrate. The current runtime answers the worked-example versions of these questions today: it can show admitted claims, audit history, derived views such as trial balance, and those same views as they stood after any committed transition. The broader production forms - actor authority, exposure limits, end-to-end regulated workflows - are the direction of the project, not yet a packaged product surface. The substrate is the same in both cases: the audit log is the chronological record; derived views read only from admitted state (so they inherit the legitimacy of the source claims and cannot reflect records that an invariant rejected); and as-of evaluation reconstructs historical state from the audit log without any bitemporal flags polluting the schema.

## How it works

Two language constructs are first-class. Everything else is built from them:

- An **invariant** is a rule that must always hold over admitted state.
- A **transformation** is the only path by which state may change. It proposes a set of additions, removals, and outbound effects.

The runtime checks every active invariant against the proposed result. If any fails, the transformation is rejected atomically - nothing is written, nothing is sent. Records that survive are **claims**: typed assertions admitted under a specific authority, at a specific moment, by a specific transformation. The audit log is append-only; current claim-state changes by asserted and retracted claims, with corrections preserving history through supersession rather than overwriting prior audit records. The originals stay findable forever, even after restatement.

Surface syntax is illustrative (the parser is on the roadmap; programs today are constructed as Rust IR). The shape, drawn from the double-entry ledger example, fits on one screen:

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

The fundamental accounting equation is not a check the application code performs - it is a property of the runtime. The same applies to every other invariant: bilateral netting integrity, supersession lineage, admissibility-for-purpose, period-close gates.

## Running it today

The CLI works end-to-end against a real PostgreSQL database. Sample session:

```bash
# Run a governed transformation. The runtime checks every invariant
# before committing; rejection is atomic.
morpholog propose double_entry_ledger post_simple_entry \
    --args '[{"type":"subject","value":"entry_001"}, ... ]' \
    --database-url postgres:///my_db

# Inspect current admitted claims, audit log, or derived views.
morpholog inspect claims     --database-url postgres:///my_db
morpholog inspect audit      --database-url postgres:///my_db
morpholog inspect derived double_entry_ledger TrialBalanceRow \
    --database-url postgres:///my_db

# Ask the same questions as of a past transition. Auditors love this.
morpholog inspect claims --as-of <transition_id> ...
morpholog inspect derived double_entry_ledger TrialBalanceRow \
    --as-of <transition_id> ...
```

The as-of query is the one most directly recognizable to anyone who has had to defend the books: it reconstructs the exact state that existed at any past transition by replaying the audit log, with no bitemporal `valid_from`/`valid_to` columns required.

## Worked examples (each proven against PostgreSQL)

- [**Bilateral settlement netting**](examples/01_settlement_netting/) - shows that invariants catch arithmetic and exclusion errors against the candidate state, not just the pre-state. A transformation that would *create* an inconsistency by combining valid inputs is rejected before any commit.
- [**Revenue restatement**](examples/02_revenue_restatement/) - shows contested legitimacy: historical records survive correction; current-standing pointers move via retraction; supersession lineage is recorded as ordinary claims. Three months from now, the original number is still in the database and is still findable.
- [**Claim standing**](examples/03_claim_standing/) - shows admissibility-for-purpose. The same underlying claim can carry different standing for different decisions, granted by different authorities, lost without mutating the underlying claim itself. Exactly the shape regulated lending and statutory reporting need.
- [**Double-entry ledger with period close**](examples/04_double_entry_ledger/) - the fundamental accounting equation enforced as an invariant; period close as an admission gate; closed periods corrected by restatement that preserves the original record. Plus a `TrialBalanceRow` derived claim that turns the raw journal lines into a query-shaped report row, computed only from claims that have already passed the ledger's invariants.

Each example has integration tests that exercise both the in-memory kernel and the durable PostgreSQL commit path. The as-of evaluation that the CLI exposes runs against the same audit log the integration tests produce; nothing is mocked.

## Where Morpholog stays out

Morpholog is not the language you write a whole business system in. User interfaces, market data, dashboards, search, ML, optimisation, integrations - all of this lives outside and uses normal tools. Morpholog owns the part of the system where the question *"may this be admitted as legitimate?"* must have a definite answer. Measured in lines of code, that is always a small fraction of any real business system. Measured in failure modes prevented, it is most of what matters.

The framing of what Morpholog should grow into, and what it must never become, lives in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md).

---

## Project status

Active project, solo-built, shipping incremental milestones. The semantic kernel, the PostgreSQL persistence adapter, the CLI, and the worked examples are all functional and tested. Performance characteristics are bench-measured: writes scale linearly to 100K-entry ledgers (~1.6s per commit at 100K pre-existing entries after the indexed-`State` work); as-of replay is currently O(N^2) in claim count and is the next-forced optimisation. See [`crates/morpholog-bench/README.md`](crates/morpholog-bench/README.md) for the running performance story.

What's not yet in the box: a parser (programs are constructed as Rust IR; the CLI accepts built-in programs only); an outbox-delivery worker (rows are enqueued, deliberately not yet consumed); user-supplied program loading; materialised derived claims. Each is on the roadmap and will be added when a worked example forces the shape.

To run the tests yourself:

```bash
cargo test -p morpholog-core --all-targets
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --all-targets -- --test-threads=1
```

First-time setup (skip if the schema is already applied):

```bash
createdb morpholog_dev
psql morpholog_dev -f crates/morpholog-core/sql/schema.sql
```

Crates: `morpholog-core` (synchronous kernel, no I/O), `morpholog-postgres` (async adapter over `propose_against_pg`, plus read helpers for inspecting current and historical claims, audit, outbox, and derived enumerations), `morpholog-cli` (`morpholog inspect ...` / `morpholog propose <program> <transformation>` with `--as-of` for historical queries), `morpholog-bench` (scale-pressure benchmark for write, read, and as-of replay paths).

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, the language affordances on the roadmap, the three-level expansion ladder, and non-goals. Start here for the design framing.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - semantics that the `morpholog-core` kernel realises.
- [`docs/forced-by-examples.md`](docs/forced-by-examples.md) - retrospective doctrine doc recording, for each significant runtime/IR decision, which worked example forced it and why.
- [`docs/mvp-cut.md`](docs/mvp-cut.md) - decision record for the MVP cut line and the PRs that crossed it.
- Worked examples: [`examples/01_settlement_netting/`](examples/01_settlement_netting/), [`examples/02_revenue_restatement/`](examples/02_revenue_restatement/), [`examples/03_claim_standing/`](examples/03_claim_standing/), [`examples/04_double_entry_ledger/`](examples/04_double_entry_ledger/).

## Requirements

- Rust 1.95+ (install via [rustup](https://rustup.rs)).
- PostgreSQL 17+. Morpholog v0 targets PostgreSQL only and uses PostgreSQL-specific features (SSI for `SERIALIZABLE`, JSONB with CHECK constraints, JSONB path functions). Database portability is not a goal at this stage.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
