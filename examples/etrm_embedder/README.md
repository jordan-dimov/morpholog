# A worked embedder: an ETRM driving Morpholog

The other examples here are `.morph` programmes - the rules an auditor reads. This one is the other side of the boundary: a non-Rust system putting one of those programmes to work. It is the smallest honest sketch of the reference energy-trading risk system integrating with Morpholog the way any external system would - a subprocess and JSON underneath, no FFI, no Rust toolchain - and it drives a real commodity trade through its whole governed life:

> grant the desk its authority -> capture the trade -> confirm it and set the official price -> correct that price -> settle against the corrected figure.

It runs against [`../10_trade_lifecycle/trade_lifecycle.morph`](../10_trade_lifecycle/) through the typed client the binary itself emits:

```bash
morpholog generate python-client examples/10_trade_lifecycle/trade_lifecycle.morph --out examples/etrm_embedder
```

The [`morpholog_client/`](morpholog_client/) package beside the script is that output, committed so the example runs as-is and so CI can prove the binary still generates it byte-for-byte (regenerate-and-diff). The lifecycle script itself is now only the business narrative: typed request models in, typed envelopes and read models out, every emitted intent delivered through its generated payload model.

## Why it exists

A worked example here earns its place by forcing the next improvement, not by looking polished. This one was written to lean on the contract's edges the way a real integration does.

**It uses what the binary already knows.** The request models carry each transformation's parameters and kinds (a `Decimal` is a `Decimal`, a date is a `date` - never a float, never a guessed string); the envelope models distinguish a lawful business rejection (the over-cap second settlement) from an operational failure by construction; `explain` answers *why* a settlement would be refused before the trade is confirmed, through the same typed surface. The whole lifecycle, including the post-commit delivery of every emitted intent, goes through the CLI alone.

## It keeps forcing the missing piece

Decoding emitted intent payloads by hand forced `morpholog schema --intent`. Reading governed state back forced `inspect claims --predicate`, and decoding those reads by declared field name forced `--named` (this script and the first real external embedder independently hand-rolled the same helper - two reinventions of one decode is the bar). Schema provisioning moved onto `morpholog init`, the embedded-schema path. The last layer standing was the hand-written client itself: the same codecs, envelope models, and subprocess adapter, written twice by two independent Python embedders. That convergence forced `generate python-client`, and the hand-rolled `Morpholog` class this example used to carry is deleted - the client is a projection of the programme now, like the schema and the envelopes, stamped with the model hash it was built against. The residual friction, printed at the end, is that selection stops at predicate granularity: an argument-level filter waits for an example with a book big enough to force it.

## Running it

```bash
# Use a DISPOSABLE database - the run path commits, and the script
# resets the schema for a reproducible run.
DATABASE_URL=postgres:///morpholog_bench python3 examples/etrm_embedder/etrm_lifecycle.py
```

Needs **Python 3.10+** (the floor the generated client declares and enforces at import) and three things on your `PATH`: the `morpholog` CLI (set `MORPHOLOG_BIN` to point elsewhere, e.g. `target/release/morpholog`), the `psql` client (the demo-only schema reset shells out to it), and a disposable PostgreSQL database in `DATABASE_URL`. Python standard library only - no packages to install. It prints each lifecycle step, the intent it delivered, and a closing list of the interface friction it hit.

After editing the `.morph`, regenerate the client with the command above; the `MODEL_HASH` stamp in `morpholog_client/__init__.py` names the rules the package was built against, so CI can assert the generated code, the `schema --all` manifest, and the live binary all agree.

## What it is not

Not the ETRM. It governs none of the things an ETRM does itself - market data, curves, position and P&L analytics - because those live outside Morpholog's boundary, in purpose-built stores. This drives only the lifecycle events that make those numbers auditable: capture, confirm, correct, settle. It is the seed of the real embedder, kept deliberately small.
