# Morpholog

An experimental runtime for business systems where the rules that decide what may legitimately enter the records are enforced by the language itself - not by code somebody remembered to write.

## The failure it addresses

Anyone who has worked closely with a serious business system - a trading book, a general ledger, a regulated lending platform, a settlement engine - knows the family of bugs Morpholog targets. The records and the reports disagree. Yesterday's number is no longer today's number, and nobody can explain when it changed. The audit trail can show *what* happened, but not whether what happened was *legitimate under the rules in force at the time*. The rules themselves are scattered across validation layers, ORM hooks, stored procedures, end-of-day reconciliation scripts, and tribal knowledge in the heads of senior staff. Business software has spent decades trying to make that legitimacy guarantee from the outside; the result is the kind of long-tail failure every operator of a serious system eventually recognises.

Morpholog moves the legitimacy boundary *into* the language. State is no longer a soup of mutable rows. It is a set of **claims** - typed assertions admitted into governed state under a specific authority, at a specific moment, by a specific transformation, with full provenance. The rules that must always hold over admitted state are **invariants**. The only way claims change is through a **transformation**, which proposes a set of additions, removals, and outbound effects; the runtime checks every active invariant against the proposed result; if any fails, the transformation is rejected atomically and nothing is written or sent.

There are no entities, no classes, no ad-hoc validators, no reconciliation scripts. The discipline is exact:

> Whatever you want to make legitimate, name it as a predicate and admit it as a claim. Whatever rules must hold, write as an invariant. Everything else lives outside.

That is the entire surface area.

## What it looks like

Surface syntax is illustrative - the parser is deliberately deferred. The shape, drawn from the double-entry ledger example, is small enough to read in a sitting:

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

The invariant says what must be true of state - debits and credits balance for every posted entry. The transformation is the only path by which a journal entry can enter state, and it is admitted only if the period is open and the result satisfies every active invariant. If anything fails, no claim is written and no `JournalEntryPosted` notification is sent. The fundamental accounting equation is not a check the application code performs - it is a property of the runtime.

## Why this matters

Consider questions a controller, auditor, or risk officer might ask of any serious business system:

- *Why does this report differ from the one we filed three months ago?*
- *Who authorised this entry, and under what limit?*
- *If the authorisation we relied on was rescinded yesterday, was yesterday's decision still legitimate?*
- *What did the books say at quarter-end, under the close rules in force then?*

In conventional software these get answered by detective work - searching log tables, reconciling parallel systems, asking long-tenured colleagues, accepting that some questions never get a clean answer. In Morpholog the raw material is preserved by construction: claims, transformations, audit rows, and supersession lineage are all governed state, never overwritten by accident. The query and projection machinery that would turn that raw material into reproducible reports - derived claims, as-of evaluation - is named on the roadmap (see [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md)) and is what the next worked examples will push on.

The worked examples in this repository show the pattern in increasing depth:

- [**Bilateral settlement netting**](examples/01_settlement_netting/) - invariants that catch arithmetic and exclusion errors before any state changes.
- [**Revenue restatement**](examples/02_revenue_restatement/) - historical claims survive correction; current-standing pointers move via retraction; supersession lineage is recorded as ordinary claims.
- [**Claim standing**](examples/03_claim_standing/) - the same underlying claim can carry different admissibility for different decisions, granted by different authorities, lost without mutating the claim itself.
- [**Double-entry ledger with period close**](examples/04_double_entry_ledger/) - the fundamental accounting equation enforced as an invariant; period close as an admission gate; closed periods corrected by restatement that preserves the original record.

Each is proven both in-memory and durably against PostgreSQL.

## Where Morpholog ends

Morpholog is deliberately not the language you write a whole business system in. User interfaces, market data, reports, dashboards, search, machine learning, optimisation, integrations - all of this lives outside Morpholog and uses normal tools. What Morpholog owns is the *legitimacy surface*: the part of the system where the question "may this be admitted as legitimate?" must have a definite answer.

Measured in lines of code, that is always a small fraction of a real business system. Measured in failure modes prevented, it can be most of what matters. The deeper framing - what the project is for, what it should grow into, and what it must never become - lives in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md).

---

## Project status

Early but not a toy. A synchronous semantic kernel and a working PostgreSQL persistence adapter ship today. The worked examples are proven both in-memory and durably against PostgreSQL. The CLI can both inspect current state and run named transformations from a built-in program against a database (`morpholog inspect ...` and `morpholog propose ...`). There is no parser yet, no support for user-supplied programs (built-in examples only), and no outbox worker. These are deliberately deferred until the next semantic frontiers (derived claims and as-of evaluation) have been pushed harder.

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

Crates: `morpholog-core` (synchronous kernel, no I/O), `morpholog-postgres` (async adapter over `propose_against_pg`, plus read helpers for inspecting current claims/audit/outbox), `morpholog-cli` (`morpholog inspect ...` for read-side inspection and `morpholog propose <program> <transformation>` for running a named transformation from a built-in program; JSON output throughout).

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) - what Morpholog is for, the four language affordances on the roadmap, the three-level expansion ladder, and non-goals. Start here for the design framing.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) - semantics that the `morpholog-core` kernel realises.
- [`docs/forced-by-examples.md`](docs/forced-by-examples.md) - retrospective doctrine doc recording, for each significant runtime/IR decision, which worked example forced it and why.
- [`docs/mvp-cut.md`](docs/mvp-cut.md) - decision record for the MVP cut line. Concrete operational threshold ("a developer can run governed transformations against PostgreSQL without editing Morpholog's Rust source") and the three PRs that cross it.
- Worked examples: [`examples/01_settlement_netting/`](examples/01_settlement_netting/), [`examples/02_revenue_restatement/`](examples/02_revenue_restatement/), [`examples/03_claim_standing/`](examples/03_claim_standing/), [`examples/04_double_entry_ledger/`](examples/04_double_entry_ledger/).

## Requirements

- Rust 1.95+ (install via [rustup](https://rustup.rs)).
- PostgreSQL 17+. Morpholog v0 targets PostgreSQL only and uses PostgreSQL-specific features (SSI for `SERIALIZABLE`, JSONB with CHECK constraints, JSONB path functions). Database portability is not a goal at this stage.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
