# A worked embedder: an ETRM driving Morpholog

The other examples here are `.morph` programmes - the rules an auditor reads. This one is the other side of the boundary: a non-Rust system putting one of those programmes to work. It is the smallest honest sketch of the reference energy-trading risk system integrating with Morpholog the way any external system would - a subprocess and JSON, no FFI, no generated client, no Rust toolchain - and it drives a real commodity trade through its whole governed life:

> grant the desk its authority -> capture the trade -> confirm it and set the official price -> correct that price -> settle against the corrected figure.

It runs against [`../10_trade_lifecycle/trade_lifecycle.morph`](../10_trade_lifecycle/) using only the public contract in [`docs/embedder-integration.md`](../../docs/embedder-integration.md): `morpholog schema` to learn the input shape, `morpholog run` to commit, `morpholog explain` to ask why a transition would be refused, `morpholog inspect claims --predicate` to read governed state back, and `morpholog outbox claim` / `complete` to deliver the intents each commit emits.

## Why it exists

A worked example here earns its place by forcing the next improvement, not by looking polished. This one was written to lean on the contract's edges the way a real integration does.

**It uses what the kernel already offers.** The embedder never hard-codes a transformation's parameters: it asks `morpholog schema` and builds the request from the answer. It distinguishes a lawful business rejection (`run` exits 1 with a `rejected` outcome - the over-cap second settlement) from an operational failure, and it asks `morpholog explain` *why* a settlement would be refused before the trade is confirmed, getting back the exact missing gate. The whole lifecycle, including the post-commit delivery of every emitted intent, goes through the CLI alone.

**It forced the piece that was missing.** To *deliver* an emitted intent - to turn `TradeSettlementRequested` into a downstream payment request - a deliverer has to read the intent's payload. That payload arrives as positional, tagged values with no field names. An earlier draft of this example had to hard-code "argument 0 is the settlement id, argument 1 is the trade, argument 2 is the quantity" for every intent it consumed - exactly the contract drift a schema exists to prevent. So this example forced `morpholog schema --intent <Type>`, the payload dual of the transformation-argument schema, and the PR that adds the example adds that subcommand with it. The hand-coding is gone; the embedder decodes intents by name from their declared contract.

It also forced the read side. To settle against a price it did not itself mint - the position of any resumed process or separate service - an embedder has to *read current governed state back*, and an earlier draft could only record that as friction. It forced `morpholog inspect claims --predicate`, the targeted claim query, and the settle step now discovers the in-force figure through it (decoding the positional claim args by declared field name via `inspect predicates`, never by hard-coded position). The residual friction, printed at the end, is that selection stops at predicate granularity: picking *this trade's* pointer out of the result is client-side, and an argument-level filter waits for an example with a book big enough to force it.

## Running it

```bash
# Use a DISPOSABLE database - the run path commits, and the script
# resets the schema for a reproducible run.
DATABASE_URL=postgres:///morpholog_bench python3 examples/etrm_embedder/etrm_lifecycle.py
```

Needs **Python 3.13+** and three things on your `PATH`: the `morpholog` CLI (set `MORPHOLOG_BIN` to point elsewhere, e.g. `target/release/morpholog`), the `psql` client (the demo-only schema reset shells out to it), and a disposable PostgreSQL database in `DATABASE_URL`. Python standard library only - no packages to install. It prints each lifecycle step, the intent it delivered, and a closing list of the interface friction it hit.

## What it is not

Not the ETRM. It governs none of the things an ETRM does itself - market data, curves, position and P&L analytics - because those live outside Morpholog's boundary, in purpose-built stores. This drives only the lifecycle events that make those numbers auditable: capture, confirm, correct, settle. It is the seed of the real embedder, kept deliberately small.
