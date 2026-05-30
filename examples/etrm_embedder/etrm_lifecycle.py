#!/usr/bin/env python3
"""A worked embedder: an ETRM driving Morpholog over the CLI contract.

This is the smallest honest version of the reference energy-trading risk
system integrating with Morpholog the way a non-Rust system actually
would - a subprocess and JSON, no FFI, no generated client. It drives one
commodity trade through its whole governed lifecycle:

    grant authority -> capture -> confirm -> correct the official price -> settle

against `examples/10_trade_lifecycle/trade_lifecycle.morph`, using exactly
the public surface in `docs/embedder-integration.md`:

  * `morpholog schema`  - discover a transformation's argument contract
  * `morpholog run`     - commit a governed transition
  * `morpholog explain` - diagnose a rejection without proposing it
  * `morpholog outbox`  - claim / complete emitted intents (the deliverer side)

It is deliberately written to lean on the contract's edges, not to glide
over them. Where the interface makes the embedder do work it should not
have to, the friction is recorded and printed at the end as concrete
pressure on Morpholog - see FRICTION below. The point of a worked example
here is to force the next improvement, not to look polished.

Run it (against a DISPOSABLE database - the run path commits, and this
script resets the schema for reproducibility):

    DATABASE_URL=postgres:///morpholog_bench python3 examples/etrm_embedder/etrm_lifecycle.py

Override the binary with MORPHOLOG_BIN (defaults to `morpholog` on PATH).
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

MORPHOLOG = os.environ.get("MORPHOLOG_BIN", "morpholog")
DATABASE_URL = os.environ.get("DATABASE_URL")
MORPH_FILE = (
    Path(__file__).resolve().parent.parent / "10_trade_lifecycle" / "trade_lifecycle.morph"
)
SCHEMA_SQL = (
    Path(__file__).resolve().parent.parent.parent
    / "crates"
    / "morpholog-core"
    / "sql"
    / "schema.sql"
)

# Interface friction this embedder hit - printed at the end as the
# concrete pressure the example puts on Morpholog. Each entry is a place
# the contract made the embedder do something it should not have to.
FRICTION: list[str] = []


def note_friction(item: str) -> None:
    if item not in FRICTION:
        FRICTION.append(item)


def _cli(args: list[str]) -> subprocess.CompletedProcess[str]:
    """Run the morpholog CLI as a subprocess - the whole integration surface."""
    return subprocess.run(
        [MORPHOLOG, *args],
        capture_output=True,
        text=True,
    )


def schema(transformation: str) -> dict:
    """The transformation's JSON Schema argument contract (`morpholog schema`)."""
    proc = _cli(["schema", str(MORPH_FILE), transformation])
    if proc.returncode != 0:
        sys.exit(f"schema {transformation} failed:\n{proc.stderr}")
    return json.loads(proc.stdout)


def run(transformation: str, actor: str, args_named: dict) -> dict:
    """Commit a governed transition (`morpholog run --args-named`).

    Returns the parsed outcome. Exits on an operational failure; a lawful
    business rejection is returned (status == "rejected") for the caller
    to handle, not treated as a crash.
    """
    proc = _cli(
        [
            "run",
            str(MORPH_FILE),
            transformation,
            "--actor",
            actor,
            "--args-named",
            json.dumps(args_named),
            "--database-url",
            DATABASE_URL,
        ]
    )
    # run exits 1 on a business rejection too; distinguish by parsing stdout.
    if proc.stdout.strip():
        return json.loads(proc.stdout)
    sys.exit(f"run {transformation} failed operationally:\n{proc.stderr}")


def explain(transformation: str, actor: str, args_named: dict) -> dict:
    """Diagnose without proposing (`morpholog explain --json`). Read-only."""
    proc = _cli(
        [
            "explain",
            str(MORPH_FILE),
            transformation,
            "--actor",
            actor,
            "--args-named",
            json.dumps(args_named),
            "--json",
            "--database-url",
            DATABASE_URL,
        ]
    )
    if proc.returncode != 0:
        sys.exit(f"explain {transformation} failed:\n{proc.stderr}")
    return json.loads(proc.stdout)


def outbox_claim(intent_type: str) -> dict | None:
    """Claim the next pending intent of a type (the deliverer's first move)."""
    proc = _cli(
        ["outbox", "claim", "--intent-type", intent_type, "--database-url", DATABASE_URL]
    )
    if proc.returncode != 0:
        sys.exit(f"outbox claim {intent_type} failed:\n{proc.stderr}")
    return json.loads(proc.stdout)["row"]


def outbox_complete(intent_id: str, worker_id: str, outcome: str = "delivered") -> None:
    proc = _cli(
        [
            "outbox",
            "complete",
            intent_id,
            "--worker-id",
            worker_id,
            "--outcome",
            outcome,
            "--database-url",
            DATABASE_URL,
        ]
    )
    if proc.returncode != 0:
        # `complete` exits 1 (with `{"status":"lease_lost"}` on stdout)
        # when the lease was lost to another worker; surface both streams
        # so that outcome is legible rather than a bare non-zero exit.
        detail = (proc.stdout.strip() + " " + proc.stderr.strip()).strip()
        sys.exit(f"outbox complete {intent_id} failed (exit {proc.returncode}): {detail}")


# --- Decoding an emitted intent's payload --------------------------------
#
# An emitted intent comes back as positional, tagged EvalValue:
#   TradeSettlementRequested ->
#     [{"type":"subject","value":"s1"},
#      {"type":"subject","value":"t1"},
#      {"type":"decimal","value":"60"}]
# To act on it (request a payment, notify a counterparty) the deliverer
# needs the field names. `morpholog schema --intent <Type>` returns the
# intent's declared payload contract - the same self-describing surface
# the embedder already uses for transformation arguments - so the
# deliverer decodes by name, never by a hand-maintained positional table.
#
# That subcommand did not exist until this example forced it: an earlier
# draft had to hard-code every intent's positional shape, which is
# exactly the contract drift a schema prevents. Landing `schema --intent`
# alongside the example is the forcing-example-plus-the-thing-it-forces
# discipline; the hand-coding is gone.

_intent_schema_cache: dict[str, dict] = {}


def intent_schema(intent_type: str) -> dict:
    """The intent payload's JSON Schema contract (`morpholog schema --intent`)."""
    if intent_type not in _intent_schema_cache:
        proc = _cli(["schema", str(MORPH_FILE), "--intent", intent_type])
        if proc.returncode != 0:
            sys.exit(f"schema --intent {intent_type} failed:\n{proc.stderr}")
        _intent_schema_cache[intent_type] = json.loads(proc.stdout)
    return _intent_schema_cache[intent_type]


def decode_intent_payload(intent_type: str, args: list[dict]) -> dict:
    """Decode a tagged-EvalValue intent payload into a named dict, taking
    the field names and their order from the declared payload contract."""
    names = intent_schema(intent_type)["required"]
    bare = [a["value"] for a in args]
    if len(names) != len(bare):
        # The schema and the emitted payload should always agree; a
        # mismatch means schema/payload skew. Fail loudly rather than let
        # zip() silently drop or ignore fields.
        sys.exit(
            f"{intent_type}: payload arity {len(bare)} != schema arity {len(names)} "
            f"(schema/payload skew); fields={names}, values={bare}"
        )
    return dict(zip(names, bare))


def deliver_emitted_intent(intent_type: str) -> None:
    """The deliverer side: claim the intent, decode it, 'deliver', complete."""
    row = outbox_claim(intent_type)
    if row is None:
        print(f"    (no pending {intent_type} to deliver)")
        return
    payload = decode_intent_payload(intent_type, row["arguments"])
    print(f"    delivered {intent_type} -> {payload}")
    outbox_complete(row["intent_id"], row["locked_by"])


def reset_schema() -> None:
    """Demo-only: drop and recreate the schema for a reproducible run. A
    real embedder never does this; it is here so the script is re-runnable."""
    subprocess.run(
        ["psql", DATABASE_URL, "-q", "-c", "DROP SCHEMA IF EXISTS morpholog CASCADE"],
        check=True,
    )
    subprocess.run(
        ["psql", DATABASE_URL, "-q", "-f", str(SCHEMA_SQL)],
        check=True,
    )


def expect_committed(outcome: dict, label: str) -> None:
    if outcome.get("status") != "committed":
        sys.exit(f"{label}: expected commit, got {json.dumps(outcome)}")
    intents = [i["name"] for i in outcome["emitted_intents"]]
    print(f"  committed {label}  (emitted: {', '.join(intents) or 'none'})")


def main() -> None:
    if not DATABASE_URL:
        sys.exit("set DATABASE_URL to a disposable database (the run path commits)")

    reset_schema()
    print(f"driving {MORPH_FILE.name} via `{MORPHOLOG}` (subprocess + JSON)\n")

    # The embedder discovers the input contract from the source itself.
    cap_schema = schema("capture_trade")
    print(f"capture_trade expects: {sorted(cap_schema['properties'])}\n")

    trade, commodity, desk = "t1", "oil", "middle_office"

    print("1. grant the desk confirmation authority for oil")
    expect_committed(
        run("grant_confirm_authority", desk, {"principal": desk, "commodity": commodity}),
        "grant_confirm_authority",
    )
    deliver_emitted_intent("ConfirmAuthorityGranted")

    print("2. front office captures the trade")
    expect_committed(
        run(
            "capture_trade",
            "trader",
            {
                "trade": trade,
                "commodity": commodity,
                "direction": "buy",
                "version_id": "v1",
                "quantity": "100",
                "delivery_period": "2026Q3",
                "captured_on": "2026-05-01",
                "price": "45.20",
            },
        ),
        "capture_trade",
    )
    deliver_emitted_intent("TradeCapturedAdmitted")

    print("3. before confirming, ask why a settlement would be refused")
    verdict = explain(
        "settle_trade",
        desk,
        {
            "trade": trade,
            "settled_qty": "60",
            "settlement_id": "s1",
            "official_price_id": "op1",
            "effective_on": "2026-06-30",
        },
    )
    print(f"    explain(settle_trade) -> {json.dumps(verdict)[:200]}")

    print("4. middle office confirms and sets the official price")
    expect_committed(
        run(
            "confirm_trade",
            desk,
            {
                "trade": trade,
                "counterparty": "acme",
                "confirmation_id": "conf1",
                "official_price_id": "op1",
                "confirmed_price": "45.20",
            },
        ),
        "confirm_trade",
    )
    deliver_emitted_intent("TradeConfirmedAdmitted")

    print("5. the official price is corrected (restatement; prior settlements would stand)")
    # We minted op1 and op2 ourselves, so we know the current price id is
    # now op2. An embedder that did NOT mint these (a resumed process, a
    # separate service) would need to *read* the current official price id
    # back from governed state - and there is no targeted query for that.
    note_friction(
        "Building the next transition needs the current governed state (here, the "
        "in-force official_price_id after a correction). This embedder gets away with "
        "tracking ids it minted itself; a resumed or separate process cannot. There is "
        "no targeted read of current claims (only `inspect derived` / `explain`). "
        "Forces: a governed read surface, e.g. `morpholog inspect claims <Predicate> --where ...`."
    )
    expect_committed(
        run(
            "correct_official_price",
            desk,
            {
                "trade": trade,
                "prior_official_price_id": "op1",
                "new_official_price_id": "op2",
                "corrected_price": "46.00",
            },
        ),
        "correct_official_price",
    )
    deliver_emitted_intent("OfficialPriceCorrected")

    print("6. settle a 60-lot slice against the corrected price op2")
    expect_committed(
        run(
            "settle_trade",
            desk,
            {
                "trade": trade,
                "settled_qty": "60",
                "settlement_id": "s1",
                "official_price_id": "op2",
                "effective_on": "2026-06-30",
            },
        ),
        "settle_trade",
    )
    deliver_emitted_intent("TradeSettlementRequested")

    print("7. an over-cap second settlement (60 + 60 > 100) is refused; ask why")
    over = run(
        "settle_trade",
        desk,
        {
            "trade": trade,
            "settled_qty": "60",
            "settlement_id": "s2",
            "official_price_id": "op2",
            "effective_on": "2026-06-30",
        },
    )
    print(f"    settle_trade(s2) -> {json.dumps(over)}")

    print("\n--- interface friction this lifecycle hit ---")
    if not FRICTION:
        print("  (none)")
    for i, item in enumerate(FRICTION, 1):
        print(f"  {i}. {item}\n")


if __name__ == "__main__":
    main()
