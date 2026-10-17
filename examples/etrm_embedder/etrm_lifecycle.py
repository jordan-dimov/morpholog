"""A worked embedder: an ETRM driving Morpholog through its generated client.

The smallest honest sketch of an energy-trading risk system integrating
with Morpholog the way any non-Rust system would: a subprocess and JSON
underneath, spoken through the typed client the binary itself emits.

    morpholog generate python-client ../10_trade_lifecycle/trade_lifecycle.morph --out .

The `morpholog_client/` package beside this script is that output,
committed so this example runs as-is and so CI can prove the binary still
generates it byte for byte. This script is only the business narrative:

    grant authority -> capture -> confirm -> correct the price -> settle

Run it against a DISPOSABLE database: the run path commits, and the script
resets the schema first. Needs `morpholog`, `psql` and PostgreSQL:

    DATABASE_URL=postgres:///morpholog_bench python3 examples/etrm_embedder/etrm_lifecycle.py
"""

from __future__ import annotations

import os
import subprocess
import sys
from datetime import date
from decimal import Decimal
from pathlib import Path

from morpholog_client import (
    Morpholog,
    MorphologError,
    Request,
    envelopes,
    models,
    open_client,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
MORPH_FILE = REPO_ROOT / "examples" / "10_trade_lifecycle" / "trade_lifecycle.morph"

TRADE = "t1"
COMMODITY = "oil"
DESK = "middle_office"
TRADER = "trader"
SETTLEMENT_DAY = date(2026, 6, 30)

# The friction this lifecycle still hits, printed at the end.
READ_SURFACE_FRICTION = (
    "The targeted read stops at predicate granularity: picking THIS trade's pointer "
    "out of `inspect claims --predicate CurrentOfficialPrice` is client-side "
    "filtering. Fine at this scale; argument-level selection awaits an example with "
    "a book big enough to force it."
)


class Unexpected(Exception):
    """The record answered in a way this narrative does not allow for."""


def reset_schema(morph: Morpholog) -> None:
    """Demo only: a real embedder never drops the schema from under itself.
    Provisioning goes through `init`, the path a binary-only deployment uses."""
    subprocess.run(
        ["psql", morph.database_url, "-qc", "DROP SCHEMA IF EXISTS morpholog CASCADE"],
        check=True,
    )
    morph.init()
    state = morph.migrate(check=True)
    if not state.is_current:
        raise Unexpected(
            f"a freshly provisioned database should be current: {state.pending}"
        )


def deliver(morph: Morpholog, intent: envelopes.IntentInstance) -> None:
    """The deliverer side: claim the intent, decode its payload through the
    generated model, and release the lease."""
    row = morph.outbox_claim(intent.name)
    if row is None:
        return
    if row.locked_by is None:
        raise Unexpected(f"claimed {intent.name} without a lease: {row}")
    payload = models.INTENT_PAYLOADS[intent.name].from_args(row.arguments)
    print(f"    delivered {intent.name} -> {payload}")
    applied = morph.outbox_complete(row.intent_id, row.locked_by)
    if not applied.applied:
        raise Unexpected(f"lease lost completing {intent.name}: {applied}")


def act(morph: Morpholog, request: Request, actor: str) -> envelopes.Committed:
    """Submit an act that the narrative needs to commit, and deliver what it emitted."""
    outcome = morph.submit(request, actor)
    if not isinstance(outcome, envelopes.Committed):
        raise Unexpected(f"{request.TRANSFORMATION}: expected a commit, got {outcome}")
    emitted = ", ".join(intent.name for intent in outcome.emitted_intents) or "none"
    print(f"  committed (emitted: {emitted})")
    for intent in outcome.emitted_intents:
        deliver(morph, intent)
    return outcome


def in_force_price(
    morph: Morpholog, as_of: str | None = None
) -> models.CurrentOfficialPriceClaim:
    """The trade's in-force pointer, read back through the typed model. A
    bare `next` is right here: the programme's uniqueness rule means the
    record cannot hold two pointers for one trade."""
    claims = morph.claims_named("CurrentOfficialPrice", as_of=as_of)
    pointers = (models.CurrentOfficialPriceClaim.from_named(c.args) for c in claims)
    return next(p for p in pointers if p.trade == TRADE)


def official_price(morph: Morpholog, price_id: str) -> models.OfficialPriceClaim:
    figures = (
        models.OfficialPriceClaim.from_named(c.args)
        for c in morph.claims_named("OfficialPrice")
    )
    return next(
        f for f in figures if f.trade == TRADE and f.official_price_id == price_id
    )


def terms_timeline(
    morph: Morpholog, as_of: str | None = None
) -> dict[tuple[str, str], models.TermsTimelineClaim]:
    rows = (
        models.TermsTimelineClaim.from_named(c.args)
        for c in morph.derived_named("TermsTimeline", as_of=as_of)
    )
    return {(row.trade, row.version_id): row for row in rows}


def settlement(settlement_id: str, price_id: str) -> models.SettleTradeRequest:
    return models.SettleTradeRequest(
        trade=TRADE,
        settled_qty=Decimal("60"),
        settlement_id=settlement_id,
        official_price_id=price_id,
        effective_on=SETTLEMENT_DAY,
    )


def lifecycle(morph: Morpholog) -> None:
    print("1. grant the desk confirmation authority for oil")
    act(
        morph,
        models.GrantConfirmAuthorityRequest(principal=DESK, commodity=COMMODITY),
        DESK,
    )

    print("2. front office captures the trade")
    capture = models.CaptureTradeRequest(
        trade=TRADE,
        commodity=COMMODITY,
        direction="buy",
        version_id="v1",
        quantity=Decimal("100"),
        delivery_period="2026Q3",
        captured_on=date(2026, 5, 1),
        price=Decimal("45.20"),
    )
    act(morph, capture, TRADER)

    print("3. before confirming, ask why a settlement would be refused")
    premature = settlement("s1", "op1")
    verdict = morph.explain(premature.TRANSFORMATION, DESK, premature.to_args_named())
    print(f"    explain -> {verdict.rejection}")

    print("4. middle office confirms and sets the official price")
    confirm = models.ConfirmTradeRequest(
        trade=TRADE,
        counterparty="acme",
        confirmation_id="conf1",
        official_price_id="op1",
        confirmed_price=Decimal("45.20"),
    )
    confirmed = act(morph, confirm, DESK)

    print("5. the official price is corrected (restatement; prior settlements stand)")
    correction = models.CorrectOfficialPriceRequest(
        trade=TRADE,
        prior_official_price_id="op1",
        new_official_price_id="op2",
        corrected_price=Decimal("46.00"),
    )
    act(morph, correction, DESK)

    # A resumed process would not know the id it minted, so settle the way
    # that process would: read the pointer back, then the figure it names.
    print("6. read the in-force figure back, then settle a 60-lot slice against it")
    pointer = in_force_price(morph)
    figure = official_price(morph, pointer.official_price_id)
    print(
        f"    in-force official price for {TRADE}: {figure.price} "
        f"under {figure.official_price_id}"
    )
    settled = act(morph, settlement("s1", pointer.official_price_id), DESK)

    print("7. an over-cap second settlement (60 + 60 > 100) is refused")
    over = morph.submit(settlement("s2", pointer.official_price_id), DESK)
    if not isinstance(over, envelopes.Rejected):
        raise Unexpected(f"the over-cap settlement should be refused, got {over}")
    print(f"    settle_trade(s2) -> rejected: {over.reason}")

    # Which price was official depends on when you ask. Same read, one more
    # argument; neither answer overwrites the other.
    print("8. the needle: which price was official AS-OF the confirmation?")
    then = in_force_price(morph, as_of=confirmed.transition_id)
    print(
        f"    as-of the confirmation: {then.official_price_id} was in force; "
        f"today it is {pointer.official_price_id}"
    )

    # The read side of an ETRM folds the audit log from its cursor, in
    # commit order, claims decoded by declared field name.
    print("9. the projector's tail: fold the settlements after the confirmation")
    for row in morph.audit_named(after=confirmed.transition_id):
        for claim in row.asserted_claims:
            if claim.predicate == "TradeSettled":
                print(
                    f"    blotter: {claim.args['settlement_id']} settled "
                    f"{claim.args['settled_qty']} (transition {row.transition_id})"
                )

    print("10. a backdated terms amendment lands after settlement")
    amendment = models.AmendTradeTermsRequest(
        trade=TRADE,
        prior_version_id="v1",
        new_version_id="v2",
        quantity=Decimal("120"),
        delivery_period="2026Q3",
        effective_from=date(2026, 6, 1),
    )
    act(morph, amendment, DESK)

    # Two reads of the same view, one as-of the coordinate before the
    # amendment and one live, diffed by key. The refresh feeds only the
    # generated SQL views' cache; a worker would schedule it after a correction.
    print("11. blast radius: which TermsTimeline rows did the amendment change?")
    report = morph.refresh_derived()
    print(
        f"    refreshed the SQL-view cache: {report.derived_claim_count} row(s) "
        f"across {report.derived_predicate_count} derived predicate(s)"
    )
    before = terms_timeline(morph, as_of=settled.transition_id)
    after = terms_timeline(morph)
    added = after.keys() - before.keys()
    if added != {(TRADE, "v2")}:
        raise Unexpected(f"the amendment should add exactly ('{TRADE}', 'v2'): {added}")
    if (TRADE, "v1") not in after:
        raise Unexpected("the original terms version must survive the amendment")
    for key in sorted(added):
        terms = after[key]
        print(
            f"    worklist: terms {terms.version_id} (qty {terms.quantity}, "
            f"effective {terms.effective_from}) postdates the settlement made under v1"
        )

    print(
        "\n--- interface friction this lifecycle still hits ---\n"
        f"  {READ_SURFACE_FRICTION}"
    )


def main() -> int:
    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        print(
            "set DATABASE_URL to a disposable database (the run path commits)",
            file=sys.stderr,
        )
        return 2
    morph = open_client(str(MORPH_FILE), database_url)
    reset_schema(morph)
    print(
        f"driving {MORPH_FILE.name} via `{morph.binary}` "
        "(generated client over subprocess + JSON)\n"
    )
    try:
        lifecycle(morph)
    except (MorphologError, Unexpected) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
