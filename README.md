# Morpholog

An experimental runtime for business systems where the rules that decide whether state may be admitted as legitimate are enforced by the language itself, not by code somebody remembered to write.

## What it is

Business software has spent decades trying to make this guarantee from the outside — validation layers, ORM hooks, stored procedures, scheduled reconciliation scripts, periodic audits, end-of-day batch jobs. The result is the kind of failure every operator of a serious business system eventually recognises: the records say one thing, the reports say another, and nobody can explain when the drift started.

Morpholog moves the legitimacy boundary into the language. The runtime is built on three primitives:

- **Claims** — admitted assertions about subjects, kept with full provenance. State is a set of claims, not a snapshot of mutable rows.
- **Invariants** — rules over admitted state that must always hold.
- **Transformations** — the only path by which state may change. A transformation proposes a set of additions, removals, and outbound intents; the runtime evaluates every active invariant against the result; if any fails, the transformation is rejected atomically — no claim is written, no instruction is sent.

There are no entities, no classes, no ad-hoc validators, no reconciliation scripts. The discipline is exact:

> Whatever you want to make legitimate, name it as a predicate and admit it as a claim. Whatever rules must hold, write as an invariant. Everything else lives outside.

That is the entire surface area.

## What that means in practice

Consider any system that needs to answer questions like these:

- *What did we believe at the close of business last Tuesday, under the rules in force then?*
- *Why does this report differ from the one we filed three months ago?*
- *Who authorised this entry, under what limit, and when?*
- *If the authorisation we relied on was rescinded yesterday, was yesterday's decision still legitimate?*

In conventional software these are answered by detective work — searching log tables, reconciling parallel systems, asking long-tenured colleagues, accepting that some questions never get a clean answer. In Morpholog the raw material for those answers — claims, transformations, audit rows, and supersession lineage — is preserved by construction; the query and projection machinery that turns it into reproducible reports is part of what is being built (see [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md)).

Three worked examples in this repository show the pattern in increasing depth:

- [**Bilateral settlement netting**](examples/01_settlement_netting/) — invariants that catch arithmetic and exclusion errors before any state changes.
- [**Revenue restatement**](examples/02_revenue_restatement/) — historical claims survive correction; current-standing pointers move via retraction; supersession lineage is recorded as ordinary claims.
- [**Claim standing**](examples/03_claim_standing/) — the same underlying claim can carry different admissibility for different decisions, granted by different authorities, lost without mutating the claim itself.

Each is proven both in-memory and durably against PostgreSQL.

## A small illustrative example

Surface syntax is not final. The shape:

```
invariant net_amount_equals_lines:
    NetSettlement(net, _, _, amount) implies
        amount == sum { x | SettlementLine(line, net, x) }

transformation create_net_settlement(party_a, party_b, lines):
    require forall { line | line in lines }:
        ApprovedSettlementLine(line) and not Netted(line)
    let net = new Subject()
    let amount = sum { x | line in lines and LineAmount(line, x) }
    assert NetSettlement(net, party_a, party_b, amount)
    for line in lines:
        assert SettlementLine(line, net, LineAmount(line))
        assert Netted(line)
    emit NetSettlementCreated(net)
```

The invariant says what must be true of state. The transformation is the only path by which state may change. If the transformation would produce a state where the invariant does not hold, the runtime refuses to commit it.

## Where Morpholog ends

Morpholog is deliberately not the language you write a whole business system in. User interfaces, market data, reports, dashboards, search, machine learning, optimisation — all of this lives outside Morpholog and uses normal tools. What Morpholog owns is the *legitimacy surface*: the part of the system where the question "may this be admitted as legitimate?" must have a definite answer.

Measured in lines of code, that is always a minority of a real system. Measured in failure modes prevented, it can be most of what matters. The deeper framing — what the project is for, what it should grow into, and what it must never become — lives in [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md).

---

## Project status

Early but not a toy. A synchronous semantic kernel and a working PostgreSQL persistence adapter ship today. Three worked examples are proven both in-memory and durably against PostgreSQL. There is no parser, no usable CLI beyond `--version`, and no outbox worker — these are deliberately deferred until the next semantic frontiers (derived claims and as-of evaluation) have been pushed harder.

```bash
cargo test -p morpholog-core --all-targets                              # 33 tests, in-memory
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --all-targets -- --test-threads=1   # 12 tests, durable
```

First-time setup (skip if the schema is already applied):

```bash
createdb morpholog_dev
psql morpholog_dev -f crates/morpholog-core/sql/schema.sql
```

Crates: `morpholog-core` (synchronous kernel, no I/O), `morpholog-postgres` (async adapter over `propose_against_pg`), `morpholog-cli` (version-printer skeleton; subcommands wait on surface syntax).

## Deeper reading

- [`docs/scope-and-ambition.md`](docs/scope-and-ambition.md) — what Morpholog is for, the four language affordances on the roadmap, the three-level expansion ladder, and non-goals. Start here for the design framing.
- [`docs/runtime-semantics.md`](docs/runtime-semantics.md) — semantics that the `morpholog-core` kernel realises.
- [`docs/postgres-persistence-v0.md`](docs/postgres-persistence-v0.md) — historical design pin for the PostgreSQL adapter.
- Worked examples: [`examples/01_settlement_netting/`](examples/01_settlement_netting/), [`examples/02_revenue_restatement/`](examples/02_revenue_restatement/), [`examples/03_claim_standing/`](examples/03_claim_standing/).

## Requirements

- Rust 1.95+ (install via [rustup](https://rustup.rs)).
- PostgreSQL 17+. Morpholog v0 targets PostgreSQL only and uses PostgreSQL-specific features (SSI for `SERIALIZABLE`, JSONB with CHECK constraints, JSONB path functions). Database portability is not a goal at this stage.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
