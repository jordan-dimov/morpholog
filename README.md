# Morpholog

An experimental programming language for business systems where correctness is enforced by the language itself, not by code you remembered to write.

You don't write entities, validators, or reconciliation scripts. You write:

- **Invariants** — rules that must always hold (e.g. "a netted settlement equals the sum of its approved lines").
- **Transformations** — the only way state is allowed to change.

A transformation is rejected at commit time if it would produce state that violates any active invariant. PostgreSQL is the runtime: Morpholog programs become schemas, constraints, and transactions.

It targets finance, trading, and other regulated domains where the question "what rule was in force when this happened, and how do you know the data obeyed it?" has to have an answer.

Illustrative — syntax is not final:

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

## Status

Very early, but no longer an in-memory toy. Morpholog has a synchronous semantic kernel and a working PostgreSQL persistence adapter. There is still no parser, no usable CLI, and no outbox worker.

Think of Morpholog right now as a **governed-state runtime**, not a programming language. The product is *commit legitimacy*: a transformation is the only way governed state changes, and it commits only if the resulting state is admissible.

## What works today

```bash
cargo test -p morpholog-core --all-targets                        # 26 tests, in-memory
DATABASE_URL=postgres:///morpholog_dev \
  cargo test -p morpholog-postgres --all-targets -- --test-threads=1   # 10 tests, durable
```

- **`morpholog-core`** — synchronous in-memory semantic kernel. IR types (Invariant, Transformation, Stmt, Expr, Term, Value, Claim, Intent), the evaluator, and `propose()` which builds a candidate state, runs every active invariant against it, and returns `Accepted` or `Rejected`.
- **`morpholog-postgres`** — thin async I/O adapter. `propose_against_pg()` opens one PostgreSQL transaction at `SERIALIZABLE` isolation, loads claims into an in-memory `State`, calls the existing sync kernel, and either rolls back atomically (Rejected) or commits claims + audit + outbox in one transaction (Committed). Outbox intents are enqueued for post-commit delivery.
- **Canonical schema** at [`crates/morpholog-core/sql/schema.sql`](crates/morpholog-core/sql/schema.sql) — `claims`, `audit`, `outbox`.

Two worked examples drive everything:

- **Settlement netting** ([`examples/01_settlement_netting/`](examples/01_settlement_netting/)) — proves the runtime can enforce arithmetic and exclusion (no double-netting). Has both in-memory tests and PostgreSQL integration tests.
- **Revenue restatement** ([`examples/02_revenue_restatement/`](examples/02_revenue_restatement/)) — proves the runtime can model **contested legitimacy**: historical claims remain admitted, current-standing pointer claims move via retractions, supersession lineage is recorded, all without claim metadata.

## What this baseline proves

- **Claims are admitted assertions, not objective facts.** State is a set of typed `(predicate, args)` claims. Multiple authorities can legitimately make different claims about the same underlying event; the system preserves all of them.
- **Transformations propose; invariants decide.** A transformation stages assertions, retractions, and outbox intents against a snapshot of pre-state. The runtime builds a candidate state and evaluates every active invariant against it. The transformation commits only if all invariants hold.
- **Durable commit boundary.** The PostgreSQL adapter atomically writes claim mutations, one audit row, and one outbox row per emitted intent. SQLSTATE `40001` is classified as a distinct retryable error. Rejected transformations write nothing.
- **"Claims about claims"** is sufficient for currentness, lineage, and standing. Authority lives in predicate naming; currentness in pointer claims; lineage in `Supersedes` claims. Metadata on claims is deferred until a real example forces it.

## What is not yet in place

- Surface syntax. `.morph` source files exist as a North-Star target but are never parsed.
- CLI beyond `--version`. No subcommands.
- Outbox worker. Intents are enqueued; nobody consumes them yet.
- Invariant lifecycle (versioned epochs). The IR carries `version: 1` everywhere.
- Read-side framework. Queries against committed state are unstudied.
- Migrations framework. The schema is applied by hand (or by CI).
- Model checker for the decidable core. Documented as a later artefact.

## Next

A third worked example focused on **claim standing** — when an admitted claim is *admissible for a given purpose*, and how that standing is acquired, transferred, and lost without mutating the claim itself. Settlement netting proved transactional correctness; revenue restatement proved that history survives correction; the next example should push on the standing-as-claims-about-claims pattern hard enough to either confirm it generalises or surface what's missing.

Parser, CLI, migrations framework, outbox worker, and read-side projections remain deliberately deferred — the next semantic frontier comes before more plumbing.

## Requirements

- Rust 1.95+ (install via [rustup](https://rustup.rs))
- PostgreSQL 17+. Morpholog v0 targets PostgreSQL only and deliberately uses PostgreSQL-specific features (SSI for SERIALIZABLE, JSONB with CHECK constraints, JSONB path functions) without portability apologies. Database portability is not a goal at this stage.

## Design tenets

- Surface language has only invariants and transformations. No entities, classes, or services.
- State is a set of *admitted claims* over opaque subject identifiers — not objective facts. A claim is a statement admitted into governed state under a specific authority, epoch, and transformation.
- Reads inside a transformation see pre-transformation state. Writes are staged and become real only at commit.
- Decimal arithmetic for business values. No floats.
- External side effects happen post-commit, at-least-once, with deterministic idempotency keys.

## License

MIT OR Apache-2.0.
