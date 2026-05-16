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

Very early. A small in-memory semantic kernel exists; there is no parser, no PostgreSQL integration, and no usable CLI yet. If you came here looking for something to deploy, come back in a few months.

## What works today

```bash
cargo test -p morpholog-core      # 17 tests pass
cargo run -q                      # prints: morpholog 0.0.1
```

The library can:

- represent invariants and transformations as Rust IR data;
- evaluate an invariant against an in-memory state of admitted claims;
- propose a transformation against a pre-state, stage claim assertions/retractions and outbox intents, build the candidate state, run all active invariants against it, and return either `Accepted` or `Rejected { reason }`.

Two worked examples exercise the semantic loop end-to-end as Rust tests: **settlement netting** (a clean kernel proof) and **revenue restatement** (preserves historical claims while moving current-standing pointer claims under temporal correction, with no metadata on any claim).

## What this baseline proves

- **Claims are admitted assertions, not objective facts.** State is a set of typed `(predicate, args)` claims. Multiple authorities can legitimately make different claims about the same underlying event; the system preserves all of them.
- **Transformations propose; invariants decide.** A transformation stages assertions, retractions, and outbox intents against a snapshot of pre-state. The runtime builds a candidate state and evaluates every active invariant against it. The transformation commits only if all invariants hold.
- **Settlement netting** enforces existence, equality-via-aggregation, and exclusion invariants. Tests prove that a valid netting commits, that a `require` failure rolls back before any staging, and that an invariant violation on the *candidate state* rolls back atomically.
- **Revenue restatement** preserves historical claims while moving current-standing pointer claims, with no metadata. A four-step chain test exercises admit → recognise → correct verification (which retracts the dependent current pointer) → restate, and verifies the final state.
- **"Claims about claims"** is sufficient for currentness, lineage, and standing. Authority lives in predicate naming; currentness in pointer claims; lineage in `Supersedes` claims. Metadata on claims is deferred until a real example forces it.

## What this baseline does not yet prove

- Surface syntax. `.morph` source files exist as a North-Star target but are never parsed.
- CLI beyond `--version`. No subcommands.
- PostgreSQL execution. A v0 schema is applied to a local dev DB but no Rust code talks to it.
- Audit and outbox row writing. The IR stages these as values; they never persist.
- Invariant lifecycle (versioned epochs). The IR carries `version: 1` everywhere.
- Read-side framework. Queries against committed state are unstudied.
- Model checker for the decidable core. Documented as a later artefact.

## Next

PostgreSQL persistence for the existing semantic loop: a `propose_against_pg()` that opens a transaction, runs the same kernel against the live `claims` table, writes one `audit` row on commit, and enqueues `outbox` rows. See [`docs/postgres-persistence-v0.md`](docs/postgres-persistence-v0.md) for the design pin. Parser and surface syntax come later.

## Requirements

- Rust 1.95+ (install via [rustup](https://rustup.rs))
- PostgreSQL 17+ (needed once the runtime is wired to storage)

## Design tenets

- Surface language has only invariants and transformations. No entities, classes, or services.
- State is a set of *admitted claims* over opaque subject identifiers — not objective facts. A claim is a statement admitted into governed state under a specific authority, epoch, and transformation.
- Reads inside a transformation see pre-transformation state. Writes are staged and become real only at commit.
- Decimal arithmetic for business values. No floats.
- External side effects happen post-commit, at-least-once, with deterministic idempotency keys.

## License

MIT OR Apache-2.0.
