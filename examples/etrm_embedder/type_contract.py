"""What an embedder's type checker sees when it hands generated request
models to the client. The gate type-checks this file and never runs it.

A refused call carries a narrow ignore; the gate warns on an unused
one, so a client that started accepting any object would fail here.
"""

from datetime import date
from decimal import Decimal

from morpholog_client import Morpholog, Request, Session, envelopes, models

grant = models.GrantConfirmAuthorityRequest(principal="desk", commodity="oil")
capture = models.CaptureTradeRequest(
    trade="t1", commodity="oil", direction="buy", version_id="v1",
    quantity=Decimal("100"), delivery_period="2026Q3",
    captured_on=date(2026, 5, 1), price=Decimal("45.20"),
)


def accepted(client: Morpholog, session: Session) -> None:
    one: envelopes.Committed | envelopes.Rejected = client.submit(capture, "trader")
    one = session.submit(capture, "trader")
    acts: list[tuple[Request, str]] = [(grant, "desk"), (capture, "trader")]
    decision: envelopes.AtomicCommitted | envelopes.AtomicRejected = client.submit_all(acts)
    decision = session.submit_all([(grant, "desk"), (capture, "trader")])


class NotARequest:
    TRANSFORMATION = "capture_trade"


def refused(client: Morpholog, session: Session) -> None:
    client.submit(object(), "trader")  # type: ignore[arg-type]
    session.submit(NotARequest(), "trader")  # type: ignore[arg-type]
    client.submit_all([(object(), "trader")])  # type: ignore[list-item]
    session.submit_all([(NotARequest(), "trader")])  # type: ignore[list-item]
