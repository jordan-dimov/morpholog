#!/usr/bin/env python3
"""A worked embedder: an ETRM driving Morpholog over the CLI contract.

The smallest honest sketch of the reference energy-trading risk system
integrating with Morpholog the way any non-Rust system would - a
subprocess and JSON, no FFI, no generated client. It drives one commodity
trade through its whole governed life against
`../10_trade_lifecycle/trade_lifecycle.morph`:

    grant authority -> capture -> confirm -> correct the price -> settle

using only the public surface in `docs/embedder-integration.md`:
`morpholog init` to provision the schema from the binary itself,
`morpholog schema` to learn a contract, `run` to commit, `explain` to
diagnose a refusal, `inspect claims --predicate --named` to read
governed state back decoded by declared field name, and `outbox
claim`/`complete` to deliver the intents each commit emits.

It is written to lean on the contract's edges, not glide over them: it
forced `morpholog schema --intent` (so a deliverer decodes payloads by
name, not hand-coded position), and it prints the friction it still hits
at the end as the next pressure on the interface.

Run it against a DISPOSABLE database (the run path commits; this resets
the schema for a reproducible run). Needs `morpholog`, `psql`, and a
PostgreSQL database on hand:

    DATABASE_URL=postgres:///morpholog_bench python3 examples/etrm_embedder/etrm_lifecycle.py
"""

import json
import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
MORPH_FILE = REPO_ROOT / "examples" / "10_trade_lifecycle" / "trade_lifecycle.morph"

# The friction this lifecycle still hits, printed at the end. (Decoding
# emitted intents used to be friction; it forced `morpholog schema
# --intent`. Reading governed state back used to be friction; it forced
# `inspect claims --predicate`. Decoding those reads by field name was
# the next friction - this script and Glasshouse both hand-rolled the
# same zip-and-guard - and it forced `--named`, so the helper is gone.)
READ_SURFACE_FRICTION = (
    "The targeted read stops at predicate granularity: picking THIS trade's pointer out "
    "of `inspect claims --predicate CurrentOfficialPrice` is client-side filtering. Fine "
    "at this scale; argument-level selection (e.g. `--where trade=t1`) awaits an example "
    "with a book big enough to force it."
)


class MorphologError(RuntimeError):
    """An operational failure from the CLI - distinct from a lawful business
    rejection, which `run` reports as a `{"status": "rejected"}` outcome."""


class Morpholog:
    """A thin client over the `morpholog` CLI: arguments in, JSON out. This
    is the entire integration surface - a subprocess and a pipe."""

    def __init__(self, file: Path, database_url: str, binary: str = "morpholog") -> None:
        self.file = str(file)
        self.database_url = database_url
        self.binary = binary

    def _json(self, *args: str) -> dict | list:
        proc = subprocess.run([self.binary, *args], capture_output=True, text=True)
        # The contract puts the result on stdout - a committed or rejected
        # outcome, a schema, an outbox row - even when a non-zero exit flags
        # a business rejection or a lost lease. Empty stdout is the only
        # genuinely operational failure.
        if not proc.stdout.strip():
            raise MorphologError(f"`{' '.join(args)}`:\n{proc.stderr.strip()}")
        return json.loads(proc.stdout)

    def schema(self, transformation: str) -> dict:
        return self._json("schema", self.file, transformation)

    def intent_schema(self, intent: str) -> dict:
        return self._json("schema", self.file, "--intent", intent)

    def run(self, transformation: str, actor: str, **args: str) -> dict:
        return self._json(
            "run", self.file, transformation,
            "--actor", actor, "--args-named", json.dumps(args),
            "--database-url", self.database_url,
        )
    def explain(self, transformation: str, actor: str, **args: str) -> dict:
        return self._json(
            "explain", self.file, transformation,
            "--actor", actor, "--args-named", json.dumps(args),
            "--json", "--database-url", self.database_url,
        )
    def claims_named(self, *predicates: str) -> list:
        flags = [flag for p in predicates for flag in ("--predicate", p)]
        return self._json(
            "inspect", "claims", *flags, "--named", self.file,
            "--database-url", self.database_url,
        )
    def claim(self, intent_type: str) -> dict | None:
        return self._json(
            "outbox", "claim", "--intent-type", intent_type,
            "--database-url", self.database_url,
        )["row"]
    def complete(self, intent_id: str, worker_id: str) -> dict:
        return self._json(
            "outbox", "complete", intent_id,
            "--worker-id", worker_id, "--outcome", "delivered",
            "--database-url", self.database_url,
        )

def decode_payload(morph: Morpholog, intent_type: str, args: list[dict]) -> dict:
    """Map an emitted intent's positional, tagged payload to named fields,
    using the declared contract - `x-morpholog-arg-order` gives the order,
    so the deliverer never hard-codes it (and `required`, a set keyword, is
    never relied on for ordering)."""
    order = morph.intent_schema(intent_type)["x-morpholog-arg-order"]
    values = [arg["value"] for arg in args]
    if len(order) != len(values):
        raise MorphologError(
            f"{intent_type}: payload arity {len(values)} != contract arity {len(order)} "
            f"(schema/payload skew); fields={order}, values={values}"
        )
    return dict(zip(order, values, strict=True))


def deliver(morph: Morpholog, outcome: dict) -> None:
    """The deliverer side: for each intent the commit emitted, claim it,
    decode its payload by name, "deliver" it, and resolve the lease."""
    if outcome.get("status") != "committed":
        raise MorphologError(f"expected a commit, got {outcome}")
    emitted = ", ".join(i["name"] for i in outcome["emitted_intents"]) or "none"
    print(f"  committed (emitted: {emitted})")
    for intent in outcome["emitted_intents"]:
        name = intent["name"]
        row = morph.claim(name)
        if row is None:
            continue
        print(f"    delivered {name} -> {decode_payload(morph, name, row['arguments'])}")
        applied = morph.complete(row["intent_id"], row["locked_by"])
        if applied.get("status") != "applied":
            raise MorphologError(f"lease lost completing {name}: {applied}")


def reset_schema(database_url: str, binary: str) -> None:
    """Demo-only, so the script is re-runnable; a real embedder never drops
    the schema out from under itself. The drop needs psql; the provisioning
    goes through `morpholog init` - the same embedded-schema path a
    binary-only deployment uses, with nothing to vendor and nothing to drift."""
    subprocess.run(["psql", database_url, "-qc", "DROP SCHEMA IF EXISTS morpholog CASCADE"], check=True)
    subprocess.run([binary, "init", "--database-url", database_url], check=True, capture_output=True)


def main() -> None:
    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        raise MorphologError("set DATABASE_URL to a disposable database (the run path commits)")
    binary = os.environ.get("MORPHOLOG_BIN", "morpholog")
    reset_schema(database_url, binary)

    morph = Morpholog(MORPH_FILE, database_url, binary)
    trade, commodity, desk = "t1", "oil", "middle_office"

    print(f"driving {MORPH_FILE.name} via `{morph.binary}` (subprocess + JSON)\n")
    print(f"capture_trade expects: {sorted(morph.schema('capture_trade')['properties'])}\n")

    print("1. grant the desk confirmation authority for oil")
    deliver(morph, morph.run("grant_confirm_authority", desk, principal=desk, commodity=commodity))

    print("2. front office captures the trade")
    deliver(morph, morph.run(
        "capture_trade", "trader",
        trade=trade, commodity=commodity, direction="buy", version_id="v1",
        quantity="100", delivery_period="2026Q3", captured_on="2026-05-01", price="45.20",
    ))
    print("3. before confirming, ask why a settlement would be refused")
    verdict = morph.explain(
        "settle_trade", desk,
        trade=trade, settled_qty="60", settlement_id="s1",
        official_price_id="op1", effective_on="2026-06-30",
    )
    print(f"    explain -> {json.dumps(verdict['verdict'])[:160]}")

    print("4. middle office confirms and sets the official price")
    deliver(morph, morph.run(
        "confirm_trade", desk,
        trade=trade, counterparty="acme", confirmation_id="conf1",
        official_price_id="op1", confirmed_price="45.20",
    ))
    print("5. the official price is corrected (restatement; prior settlements stand)")
    deliver(morph, morph.run(
        "correct_official_price", desk,
        trade=trade, prior_official_price_id="op1",
        new_official_price_id="op2", corrected_price="46.00",
    ))
    # We minted op2 ourselves, but a resumed process or separate service
    # would not know it. So settle the way that process would: read the
    # in-force pointer back through the targeted claim query, then look
    # up the figure it points at.
    #
    # A bare `next()` with no zero/many handling is deliberate. The
    # programme's `at_most_one_current_official_price` invariant means two
    # pointers for one trade is a state the runtime refuses to commit, and
    # the confirm step just guaranteed one exists. The model's invariants
    # are what license the simple read - governed state is not untrusted
    # input to be defensively re-checked.
    print("6. read the in-force figure back, then settle a 60-lot slice against it")
    pointer = next(
        c["args"]
        for c in morph.claims_named("CurrentOfficialPrice")
        if c["args"]["trade"] == trade
    )
    in_force = pointer["official_price_id"]
    figure = next(
        c["args"]
        for c in morph.claims_named("OfficialPrice")
        if c["args"]["trade"] == trade and c["args"]["official_price_id"] == in_force
    )
    print(f"    in-force official price for {trade}: {figure['price']} under {in_force}")
    deliver(morph, morph.run(
        "settle_trade", desk,
        trade=trade, settled_qty="60", settlement_id="s1",
        official_price_id=in_force, effective_on="2026-06-30",
    ))
    print("7. an over-cap second settlement (60 + 60 > 100) is refused")
    over = morph.run(
        "settle_trade", desk,
        trade=trade, settled_qty="60", settlement_id="s2",
        official_price_id=in_force, effective_on="2026-06-30",
    )
    print(f"    settle_trade(s2) -> {json.dumps(over)}")

    print(f"\n--- interface friction this lifecycle still hits ---\n  {READ_SURFACE_FRICTION}")


if __name__ == "__main__":
    try:
        main()
    except MorphologError as exc:
        sys.exit(f"error: {exc}")
