# Adding Morpholog to an existing trading system

**A reference integration: the trading system keeps doing trading-system
work, and Morpholog decides the lifecycle steps that must be defensible.**

```text
   trading system (Python)
          |
          |  capture, confirm, correct, settle
          v
      Morpholog  -- refused: the rule and the values, nothing changes
          |
          |  accepted: the record, and the notifications it emits
          v
   trading system, payments, downstream systems
```

## Why this matters

Replacing a trading system to get governed records is not an option for
most firms. This example shows the alternative: a program standing in for
an energy-trading system drives one commodity trade through its whole life
with the governed steps going through Morpholog, the way any external
system would - a subprocess and JSON underneath, no Rust:

> grant the desk its authority -> capture the trade -> confirm it and set the official price -> correct that price -> settle against the corrected figure.

## What Morpholog enforces

Everything in [trade lifecycle](../10_trade_lifecycle/): authority per
commodity, settlement on the official price in force, a settlement cap
that moves with the terms, and one settlement per id. The trading system
cannot write a step that breaks those rules; it can only propose one.

## What it refuses

The run includes a second settlement that would take the trade past its
terms. The client reports it as a business rejection, distinct by
construction from an operational failure, and `explain` says beforehand why
a settlement would be refused while the trade is still unconfirmed.

## What you can show afterwards

Every accepted step is in Morpholog's record with who proposed it and the
rules that accepted it. The script reads back which official price was in
force as of the confirmation and which is in force now, and the terms as a
timeline.

## Where it fits

The trading system keeps its market data, curves, positions and P&L.
It calls Morpholog through a typed Python client that Morpholog generates
from the trade lifecycle's own rules:

```bash
morpholog generate python-client examples/10_trade_lifecycle/trade_lifecycle.morph --out examples/etrm_embedder
```

The [`morpholog_client/`](morpholog_client/) package beside the script is that output, committed so the example runs as-is and so CI can prove the binary still generates it byte-for-byte (regenerate-and-diff). The lifecycle script itself is now only the business narrative: typed request models in, typed envelopes and read models out, every emitted intent delivered through its generated payload model.

**It uses what the binary already knows.** The request models carry each transformation's parameters and kinds (a `Decimal` is a `Decimal`, a date is a `date` - never a float, never a guessed string); the envelope models distinguish a lawful business rejection (the over-cap second settlement) from an operational failure by construction; `explain` answers *why* a settlement would be refused before the trade is confirmed, through the same typed surface. The whole lifecycle, including the post-commit delivery of every emitted intent, goes through the CLI alone.

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
