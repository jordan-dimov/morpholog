"""Value codecs for the Morpholog CLI contract, both directions.

Decode: tagged wire values (``{"type": ..., "value": ...}``) into bare
Python values. Encode: bare Python values into the ``--args-named``
codec's JSON shapes. Exactness is the contract: decimals travel as
strings end to end and become ``decimal.Decimal``, never a float;
timestamps are aware ``datetime`` objects, and a naive datetime is
refused on write because an instant must name an instant.

Durations and collections have no encoder here: the generator refuses
programmes whose contracts need them, so a codec that silently
half-supported them would lie about the client's coverage.
"""

from __future__ import annotations

import re
from datetime import date, datetime, timezone
from decimal import Decimal

# The schema pattern for decimal text, mirrored from the binary's
# argument codec: optional sign, no leading zeros, optional fraction.
_DECIMAL_PATTERN = re.compile(r"^-?(0|[1-9]\d*)(\.\d+)?$")


class CodecError(ValueError):
    """A value that cannot cross the wire contract in this direction."""


def _expect_str(what: str, value: object) -> str:
    """The wire carries these kinds as JSON strings, always. Accepting
    a number here would be coercion, not decoding - ``Decimal(1.1)``
    smuggles a float in inexactly, and that path must not exist."""
    if not isinstance(value, str):
        raise CodecError(f"{what} must arrive as a string: {value!r}")
    return value


def parse_decimal(text: str) -> Decimal:
    """Wire decimal text -> exact ``Decimal``. String in, exact out;
    the wire shape is pattern-constrained on top."""
    return Decimal(_expect_str("a decimal", text))


def parse_date(text: str) -> date:
    """Wire civil date (ISO 8601 ``YYYY-MM-DD``) -> ``date``."""
    return date.fromisoformat(_expect_str("a date", text))


def parse_timestamp(text: str) -> datetime:
    """Wire instant (RFC 3339) -> aware ``datetime``.

    Python 3.10's ``fromisoformat`` cannot parse the ``Z`` suffix the
    binary emits (that arrived in 3.11), so it is normalised first.
    Sub-microsecond fractions are refused by name rather than silently
    truncated: ``datetime`` carries microseconds, and an exact instant
    that cannot be represented exactly should fail loudly.
    """
    text = _expect_str("a timestamp", text)
    if text.endswith(("Z", "z")):
        text = text[:-1] + "+00:00"
    fraction = re.search(r"\.(\d+)", text)
    if fraction and len(fraction.group(1)) > 6:
        raise CodecError(
            f"timestamp {text!r} carries sub-microsecond precision, which "
            f"Python's datetime cannot represent exactly"
        )
    parsed = datetime.fromisoformat(text)
    if parsed.tzinfo is None:
        raise CodecError(f"timestamp {text!r} arrived without an offset")
    return parsed


def encode_decimal(value: Decimal) -> str:
    """``Decimal`` -> wire decimal text. Plain form always -
    ``Decimal.__str__`` goes scientific for some values, which the
    schema pattern (rightly) refuses - and NaN/Inf are not numbers the
    contract knows."""
    if not value.is_finite():
        raise CodecError(f"{value} is not a finite decimal")
    text = format(value, "f")
    if not _DECIMAL_PATTERN.match(text):
        raise CodecError(f"{value} renders as {text!r}, outside the wire pattern")
    return text


def encode_timestamp(value: datetime) -> str:
    """Aware ``datetime`` -> RFC 3339 text. Naive datetimes are refused:
    an instant must name an instant, and a naive datetime names one only
    relative to an unstated zone."""
    if value.tzinfo is None:
        raise CodecError(f"naive datetime {value!r}; an instant must carry its offset")
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def encode_named(value: object) -> object:
    """A bare Python value -> its ``--args-named`` JSON shape.

    Strings pass through (subjects are opaque; quantity amounts arrive
    as ``Decimal``); the declaration supplies units and kinds, so the
    named codec never tags.
    """
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        return value
    if isinstance(value, Decimal):
        return encode_decimal(value)
    if isinstance(value, datetime):
        # Checked before date: datetime is a date subclass.
        return encode_timestamp(value)
    if isinstance(value, date):
        return value.isoformat()
    raise CodecError(f"no named-codec encoding for {type(value).__name__}: {value!r}")


def decode_tagged(tagged: object) -> object:
    """A tagged wire value -> a bare Python value. Strict on shape: an
    unknown tag or a malformed payload raises rather than passing
    through, because a silently skipped value is contract drift."""
    if not isinstance(tagged, dict) or set(tagged) != {"type", "value"}:
        raise CodecError(f"not a tagged value: {tagged!r}")
    tag, value = tagged["type"], tagged["value"]
    match tag:
        case "subject":
            return _expect_str("a subject", value)
        case "decimal":
            return parse_decimal(value)
        case "bool":
            if not isinstance(value, bool):
                raise CodecError(f"a bool must arrive as a boolean: {value!r}")
            return value
        case "date":
            return parse_date(value)
        case "timestamp":
            return parse_timestamp(value)
        case "duration":
            # No exact stdlib representation (timedelta rounds
            # nanoseconds); the ISO text IS the exact value.
            return _expect_str("a duration", value)
        case "quantity":
            if not isinstance(value, dict) or set(value) != {"amount", "unit"}:
                raise CodecError(f"malformed quantity payload: {value!r}")
            # The bare amount; the unit is declared, not carried, on
            # the named surfaces this client generates models for.
            return parse_decimal(value["amount"])
        case "collection":
            if not isinstance(value, list):
                raise CodecError(f"a collection must arrive as an array: {value!r}")
            return [decode_tagged(item) for item in value]
        case _:
            raise CodecError(f"unknown value tag {tag!r}")
