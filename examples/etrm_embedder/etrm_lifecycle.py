#!/usr/bin/env python3
"""A worked embedder: an ETRM driving Morpholog through its generated client.

The smallest honest sketch of the reference energy-trading risk system
integrating with Morpholog the way any non-Rust system would - a
subprocess and JSON underneath, but spoken through the typed client the
binary itself emits:

    morpholog generate python-client ../10_trade_lifecycle/trade_lifecycle.morph --out .

The `morpholog_client/` package beside this script is that output,
committed so this example is runnable as-is and so CI can prove the
binary still generates it byte-for-byte (regenerate-and-diff). The
client is a projection of the programme, like the schema and the
envelopes - nothing here is hand-maintained interface code any more;
this script is only the business narrative:

    grant authority -> capture -> confirm -> correct the price -> settle

Run it against a DISPOSABLE database (the run path commits; this resets
the schema for a reproducible run). Needs `morpholog`, `psql`, and a
PostgreSQL database on hand:

    DATABASE_URL=postgres:///morpholog_bench python3 examples/etrm_embedder/etrm_lifecycle.py
"""

import dataclasses
import os
import subprocess
import sys
from datetime import date
from decimal import Decimal
from pathlib import Path

from morpholog_client import Morpholog, MorphologError, envelopes, models

REPO_ROOT = Path(__file__).resolve().parents[2]
MORPH_FILE = REPO_ROOT / "examples" / "10_trade_lifecycle" / "trade_lifecycle.morph"

# The friction this lifecycle still hits, printed at the end. (Decoding
# emitted intents used to be friction; it forced `morpholog schema
# --intent`, and the generated payload models have now retired even
# that runtime call. Reading governed state back used to be friction;
# it forced `inspect claims --predicate` and then `--named`. The
# hand-rolled client itself was the last friction - two embedders
# wrote the same one - and it forced `generate python-client`.)
READ_SURFACE_FRICTION = (
    "The targeted read stops at predicate granularity: picking THIS trade's pointer out "
    "of `inspect claims --predicate CurrentOfficialPrice` is client-side filtering. Fine "
    "at this scale; argument-level selection (e.g. `--where trade=t1`) awaits an example "
    "with a book big enough to force it."
)


def deliver(morph: Morpholog, outcome: object) -> None:
    """The deliverer side: for each intent the commit emitted, claim it,
    decode its payload through the generated model (the arg order was
    baked at generation time under the model hash), and resolve the
    lease."""
    if not isinstance(outcome, envelopes.Committed):
        raise MorphologError(f"expected a commit, got {outcome}")
    emitted = ", ".join(i.name for i in outcome.emitted_intents) or "none"
    print(f"  committed (emitted: {emitted})")
    for intent in outcome.emitted_intents:
        row = morph.outbox_claim(intent.name)
        if row is None:
            continue
        payload = models.INTENT_PAYLOADS[intent.name].from_args(row.arguments)
        print(f"    delivered {intent.name} -> {payload}")
        applied = morph.outbox_complete(row.intent_id, row.locked_by)
        if not applied.applied:
            raise MorphologError(f"lease lost completing {intent.name}: {applied}")


def reset_schema(morph: Morpholog) -> None:
    """Demo-only, so the script is re-runnable; a real embedder never drops
    the schema out from under itself. The drop needs psql; the provisioning
    goes through `init` - the same embedded-schema path a binary-only
    deployment uses, with nothing to vendor and nothing to drift."""
    subprocess.run(
        ["psql", morph.database_url, "-qc", "DROP SCHEMA IF EXISTS morpholog CASCADE"],
        check=True,
    )
    morph.init()


def main() -> None:
    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        raise MorphologError("set DATABASE_URL to a disposable database (the run path commits)")
    morph = Morpholog(str(MORPH_FILE), database_url)
    reset_schema(morph)

    trade, commodity, desk = "t1", "oil", "middle_office"

    print(f"driving {MORPH_FILE.name} via `{morph.binary}` (generated client over subprocess + JSON)\n")
    fields = [f.name for f in dataclasses.fields(models.CaptureTradeRequest)]
    print(f"capture_trade expects: {sorted(fields)}\n")

    print("1. grant the desk confirmation authority for oil")
    deliver(morph, morph.submit(
        models.GrantConfirmAuthorityRequest(principal=desk, commodity=commodity), desk,
    ))

    print("2. front office captures the trade")
    deliver(morph, morph.submit(
        models.CaptureTradeRequest(
            trade=trade, commodity=commodity, direction="buy", version_id="v1",
            quantity=Decimal("100"), delivery_period="2026Q3",
            captured_on=date(2026, 5, 1), price=Decimal("45.20"),
        ),
        "trader",
    ))

    print("3. before confirming, ask why a settlement would be refused")
    premature = models.SettleTradeRequest(
        trade=trade, settled_qty=Decimal("60"), settlement_id="s1",
        official_price_id="op1", effective_on=date(2026, 6, 30),
    )
    verdict = morph.explain(premature.TRANSFORMATION, desk, premature.to_args_named())
    print(f"    explain -> {verdict.rejection}")

    print("4. middle office confirms and sets the official price")
    confirmed = morph.submit(
        models.ConfirmTradeRequest(
            trade=trade, counterparty="acme", confirmation_id="conf1",
            official_price_id="op1", confirmed_price=Decimal("45.20"),
        ),
        desk,
    )
    deliver(morph, confirmed)

    print("5. the official price is corrected (restatement; prior settlements stand)")
    deliver(morph, morph.submit(
        models.CorrectOfficialPriceRequest(
            trade=trade, prior_official_price_id="op1",
            new_official_price_id="op2", corrected_price=Decimal("46.00"),
        ),
        desk,
    ))

    # We minted op2 ourselves, but a resumed process or separate service
    # would not know it. So settle the way that process would: read the
    # in-force pointer back through the typed read models, then look up
    # the figure it points at - `figure.price` arrives as a Decimal, not
    # a string, because the model parsed it by declared kind.
    #
    # A bare `next()` with no zero/many handling is deliberate. The
    # programme's `current_official_price_unique_by_trade` invariant means
    # two pointers for one trade is a state the runtime refuses to commit,
    # and the confirm step just guaranteed one exists. The model's
    # invariants are what license the simple read - governed state is not
    # untrusted input to be defensively re-checked.
    print("6. read the in-force figure back, then settle a 60-lot slice against it")
    pointer = next(
        p
        for p in (
            models.CurrentOfficialPriceClaim.from_named(c.args)
            for c in morph.claims_named("CurrentOfficialPrice")
        )
        if p.trade == trade
    )
    figure = next(
        f
        for f in (
            models.OfficialPriceClaim.from_named(c.args)
            for c in morph.claims_named("OfficialPrice")
        )
        if f.trade == trade and f.official_price_id == pointer.official_price_id
    )
    print(f"    in-force official price for {trade}: {figure.price} under {figure.official_price_id}")
    settled = morph.submit(
        models.SettleTradeRequest(
            trade=trade, settled_qty=Decimal("60"), settlement_id="s1",
            official_price_id=pointer.official_price_id, effective_on=date(2026, 6, 30),
        ),
        desk,
    )
    deliver(morph, settled)

    print("7. an over-cap second settlement (60 + 60 > 100) is refused")
    over = morph.submit(
        models.SettleTradeRequest(
            trade=trade, settled_qty=Decimal("60"), settlement_id="s2",
            official_price_id=pointer.official_price_id, effective_on=date(2026, 6, 30),
        ),
        desk,
    )
    if not isinstance(over, envelopes.Rejected):
        raise MorphologError(f"the over-cap settlement should be refused, got {over}")
    print(f"    settle_trade(s2) -> rejected: {over.reason}")

    # The needle: WHICH price was official is a question whose answer
    # depends on when you ask it. The corrected figure is in force
    # today - but as-of the confirmation transition, the original was.
    # Same read, one extra argument; the audit log answers both
    # truthfully, and neither answer ever overwrites the other.
    print("8. the needle: which price was official AS-OF the confirmation?")
    then = next(
        p
        for p in (
            models.CurrentOfficialPriceClaim.from_named(c.args)
            for c in morph.claims_named(
                "CurrentOfficialPrice", as_of=confirmed.transition_id
            )
        )
        if p.trade == trade
    )
    print(
        f"    as-of the confirmation: {then.official_price_id} was in force; "
        f"today it is {pointer.official_price_id}"
    )

    # The projector's tail: the read side of an ETRM does not poll
    # claims, it folds the audit log - every transition since its
    # cursor, in commit order, claims decoded by declared field name.
    # Resuming from the confirmation replays exactly what happened
    # after it; the next poll would pass the last line's id.
    print("9. the projector's tail: fold the settlements after the confirmation")
    for row in morph.audit_named(after=confirmed.transition_id):
        for claim in row.asserted_claims:
            if claim.predicate == "TradeSettled":
                print(
                    f"    blotter: {claim.args['settlement_id']} settled "
                    f"{claim.args['settled_qty']} (transition {row.transition_id})"
                )

    # A late-arriving correction: the terms are amended with a
    # backdated effective date AFTER a settlement was made against the
    # original version. Nothing is overwritten - the amendment is its
    # own governed commit, and the settlement stands.
    print("10. a backdated terms amendment lands after settlement")
    deliver(morph, morph.submit(
        models.AmendTradeTermsRequest(
            trade=trade, prior_version_id="v1", new_version_id="v2",
            quantity=Decimal("120"), delivery_period="2026Q3",
            effective_from=date(2026, 6, 1),
        ),
        desk,
    ))

    # The blast-radius read: which rows of a derived view did that
    # correction change? Two same-shape reads of the SAME view - one
    # as-of the coordinate just before the amendment, one live -
    # diffed by key. The derived reads compute live from the admitted
    # claims; the refresh feeds only the generated SQL views' cache,
    # and is shown here as the operational step a worker would
    # schedule after a correction lands.
    print("11. blast radius: which TermsTimeline rows did the amendment change?")
    report = morph.refresh_derived()
    print(
        f"    refreshed the SQL-view cache: {report.derived_claim_count} row(s) "
        f"across {report.derived_predicate_count} derived predicate(s)"
    )

    def timeline(rows):
        keyed = {}
        for c in rows:
            row = models.TermsTimelineClaim.from_named(c.args)
            keyed[(row.trade, row.version_id)] = row
        return keyed

    before = timeline(morph.derived_named("TermsTimeline", as_of=settled.transition_id))
    after = timeline(morph.derived_named("TermsTimeline"))
    added = after.keys() - before.keys()
    if added != {(trade, "v2")}:
        raise MorphologError(f"the amendment should add exactly ('{trade}', 'v2'): {added}")
    if (trade, "v1") not in after:
        raise MorphologError("the original terms version must survive the amendment")
    for key in sorted(added):
        row = after[key]
        print(
            f"    worklist: terms {row.version_id} (qty {row.quantity}, "
            f"effective {row.effective_from}) postdates the settlement made under v1"
        )
    if not all(isinstance(c, envelopes.ClaimInstance) for c in morph.derived("TermsTimeline")):
        raise MorphologError("the bare derived read returns tagged ClaimInstance rows")

    print(f"\n--- interface friction this lifecycle still hits ---\n  {READ_SURFACE_FRICTION}")


if __name__ == "__main__":
    try:
        main()
    except MorphologError as exc:
        sys.exit(f"error: {exc}")
