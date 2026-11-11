"""Value codecs for the Morpholog CLI contract, both directions.

Decode: tagged wire values (``{"type": ..., "value": ...}``) into bare
Python values, a quantity keeping its unit. Encode: bare Python values into the ``--args-named``
codec's JSON shapes. Exactness is the contract: decimals travel as
strings end to end and become ``decimal.Decimal``, never a float;
timestamps are aware ``datetime`` objects, and a naive datetime is
refused on write because an instant must name an instant.

Durations have no encoder here: the generator refuses programmes whose
contracts need a Duration argument, so a codec that silently
half-supported it would lie about the client's coverage. A collection
argument needs no encoder of its own either - the generated request
model encodes it element by element, each item passing through
``encode_named`` as the scalar it is.
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from datetime import date, datetime, timezone
from decimal import Decimal
from typing import TypeVar

# The schema pattern for decimal text, mirrored from the binary's
# argument codec: optional sign, no leading zeros, optional fraction.
# ASCII digits only and the whole text, as the binary reads it.
_DECIMAL_PATTERN = re.compile(r"^-?(0|[1-9]\d*)(\.\d+)?$", re.ASCII)
# The schema's `date` and `date-time` formats (RFC 3339), and the
# signed exact-seconds spans the binary writes for a duration.
_DATE_PATTERN = re.compile(r"^\d{4}-\d{2}-\d{2}$")
_TIMESTAMP_PATTERN = re.compile(
    r"^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$"
)
_DURATION_PATTERN = re.compile(r"^-?PT(?=\d)(\d+H)?(\d+M)?(\d+(\.\d+)?S)?$")


class CodecError(ValueError):
    """A value that cannot cross the wire contract in this direction."""


@dataclass(frozen=True)
class Quantity:
    """A tagged quantity: an exact amount and the unit it carried."""

    amount: Decimal
    unit: str


def _expect_str(what: str, value: object) -> str:
    """The wire carries these kinds as JSON strings, always. Accepting
    a number here would be coercion, not decoding - ``Decimal(1.1)``
    smuggles a float in inexactly, and that path must not exist."""
    if not isinstance(value, str):
        raise CodecError(f"{what} must arrive as a string: {value!r}")
    return value


def parse_decimal(text: object) -> Decimal:
    """Wire decimal text -> exact ``Decimal``. Only the wire pattern is
    read: ``NaN``, exponents and leading zeros are refused, not parsed."""
    return Decimal(_wire_decimal(_expect_str("a decimal", text)))


def _wire_decimal(text: str) -> str:
    """The text, if it is a decimal the kernel holds as written: the wire
    pattern, at most 28 fractional digits, and its digits as one integer
    below 2^96, trailing zeros counted. Past that the kernel refuses
    rather than rounds, and so does this client on both directions."""
    if not _DECIMAL_PATTERN.fullmatch(text):
        raise CodecError(f"decimal {text!r} is outside the wire pattern")
    whole, _, fraction = text.lstrip("-").partition(".")
    if len(fraction) > 28 or int(whole + fraction) >= 1 << 96:
        raise CodecError(
            f"decimal {text!r} is not one the kernel holds as written: at most 28 "
            f"fractional digits, with an unsigned, unscaled coefficient smaller than 2^96"
        )
    return text


def parse_date(text: object) -> date:
    """Wire civil date (ISO 8601 ``YYYY-MM-DD``) -> ``date``. A year
    outside 1-9999, which the binary can write, has no ``date``."""
    text = _expect_str("a date", text)
    if not _DATE_PATTERN.match(text):
        raise CodecError(f"date {text!r} is not YYYY-MM-DD")
    try:
        return date.fromisoformat(text)
    except ValueError as exc:
        raise CodecError(f"date {text!r}: {exc}") from exc


def parse_timestamp(text: object) -> datetime:
    """Wire instant (RFC 3339) -> aware ``datetime``.

    Sub-microsecond fractions are refused by name rather than silently
    truncated: ``datetime`` carries microseconds, and an exact instant
    that cannot be represented exactly should fail loudly.
    """
    text = _expect_str("a timestamp", text)
    if not _TIMESTAMP_PATTERN.match(text):
        raise CodecError(f"timestamp {text!r} is not an RFC 3339 instant with an offset")
    fraction = re.search(r"\.(\d+)", text)
    if fraction and len(fraction.group(1)) > 6:
        raise CodecError(
            f"timestamp {text!r} carries sub-microsecond precision, which "
            f"Python's datetime cannot represent exactly"
        )
    try:
        return datetime.fromisoformat(text.upper())
    except ValueError as exc:
        raise CodecError(f"timestamp {text!r}: {exc}") from exc


def encode_decimal(value: Decimal) -> str:
    """``Decimal`` -> wire decimal text. Plain form always -
    ``Decimal.__str__`` goes scientific for some values, which the
    schema pattern (rightly) refuses - and NaN/Inf are not numbers the
    contract knows. A value crosses as written or not at all. The limit
    is read off the digits and exponent before any text is written,
    since ``format`` writes about as many characters as the exponent."""
    exponent = value.as_tuple().exponent
    if not isinstance(exponent, int):
        raise CodecError(f"{value} is not a decimal the kernel holds")
    if exponent < -28 or len(value.as_tuple().digits) + max(exponent, 0) > 29:
        raise CodecError(f"{value} is beyond the decimal the kernel holds as written")
    return _wire_decimal(format(value, "f"))


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


def digest(*values: object) -> str:
    """``sha256:<hex>`` over these values in order, the bytes the
    programme's ``digest(...)`` hashes, so a commitment computed here is
    the one the record later holds a reveal to.

    Each value is encoded as the record stores it: a ``str`` is a
    subject, a ``Decimal`` a decimal with its scale trimmed (so
    ``Decimal("100.00")`` and ``Decimal("100")`` commit alike), a ``bool``
    a bool, a ``date`` a date and an aware ``datetime`` an instant. An
    ``int`` or a ``float`` is refused rather than read as a decimal, and
    so is a naive datetime: a commitment the kernel could not reproduce
    is worse than none. No database, subprocess or network.
    """
    if not values:
        raise CodecError("digest takes at least one value")
    preimage = _digest_preimage(values)
    return "sha256:" + hashlib.sha256(preimage.encode("utf-8")).hexdigest()


def _digest_preimage(values: tuple[object, ...]) -> str:
    # PostgreSQL's jsonb text: ", " between items, non-ASCII as it is,
    # the short escapes and \u00xx for the other control characters,
    # which is what json.dumps writes with these settings.
    keys = [_value_key(value) for value in values]
    return json.dumps(keys, separators=(", ", ": "), ensure_ascii=False)


def _value_key(value: object) -> list[object]:
    # bool before the rest: it is an int subclass.
    if isinstance(value, bool):
        return ["bool", value]
    if isinstance(value, str):
        return ["subject", value]
    if isinstance(value, Decimal):
        return ["decimal", _trimmed_decimal(value)]
    if isinstance(value, datetime):
        # datetime before date: it is a date subclass.
        return ["timestamp", _trimmed_timestamp(encode_timestamp(value))]
    if isinstance(value, date):
        return ["date", value.isoformat()]
    raise CodecError(
        "digest takes a str, a Decimal, a bool, a date or an aware datetime, "
        f"not {type(value).__name__}: {value!r}"
    )


def _trimmed_decimal(value: Decimal) -> str:
    """``trim_scale(numeric)::text``: no trailing zeros, no exponent, and
    a zero is ``0`` whatever its sign or scale. Trimmed on the digits
    before any text is written, so a long scale costs nothing and no
    non-zero digit is ever dropped; ``Decimal.normalize`` would round to
    the context's precision instead."""
    sign, digits, exponent = value.as_tuple()
    if not isinstance(exponent, int):
        raise CodecError(f"{value} is not a decimal the kernel holds")
    if value == 0:
        return "0"
    while digits[-1] == 0:
        digits = digits[:-1]
        exponent += 1
    return encode_decimal(Decimal((sign, digits, exponent)))


def _trimmed_timestamp(text: str) -> str:
    """The stored instant's text: a fraction only when non-zero, with
    its trailing zeros trimmed."""
    if "." not in text:
        return text
    whole, fraction = text[:-1].split(".")
    fraction = fraction.rstrip("0")
    return f"{whole}.{fraction}Z" if fraction else f"{whole}Z"


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
            text = _expect_str("a duration", value)
            if not _DURATION_PATTERN.match(text):
                raise CodecError(f"duration {text!r} is not an exact span like PT6H")
            return text
        case "quantity":
            if (
                not isinstance(value, dict)
                or set(value) != {"amount", "unit"}
                or not isinstance(value["unit"], str)
            ):
                raise CodecError(f"malformed quantity payload: {value!r}")
            return Quantity(parse_decimal(value["amount"]), value["unit"])
        case "collection":
            if not isinstance(value, list):
                raise CodecError(f"a collection must arrive as an array: {value!r}")
            return [decode_tagged(item) for item in value]
        case _:
            raise CodecError(f"unknown value tag {tag!r}")


# The generated read and payload models read their fields through these,
# so a wrong kind names the field it arrived in.


def named_fields(owner: str, args: dict[str, object], fields: tuple[str, ...]) -> None:
    """A named claim carries exactly its declared fields."""
    if set(args) != set(fields):
        raise CodecError(
            f"{owner}: fields {sorted(args)} are not the declared {sorted(fields)}; "
            f"the programme has changed since this client was generated"
        )


def named_text(where: str, value: object) -> str:
    if not isinstance(value, str):
        raise CodecError(f"{where}: expected a string, got {value!r}")
    return value


def named_bool(where: str, value: object) -> bool:
    if not isinstance(value, bool):
        raise CodecError(f"{where}: expected a boolean, got {value!r}")
    return value


def named_decimal(where: str, value: object) -> Decimal:
    try:
        return parse_decimal(value)
    except CodecError as exc:
        raise CodecError(f"{where}: {exc}") from exc


def named_date(where: str, value: object) -> date:
    try:
        return parse_date(value)
    except CodecError as exc:
        raise CodecError(f"{where}: {exc}") from exc


def named_timestamp(where: str, value: object) -> datetime:
    try:
        return parse_timestamp(value)
    except CodecError as exc:
        raise CodecError(f"{where}: {exc}") from exc


def quantity_amount(where: str, value: object, unit: str) -> Decimal:
    """A decoded quantity's amount, held to the unit its declaration names."""
    if not isinstance(value, Quantity) or value.unit != unit:
        raise CodecError(f"{where}: expected a quantity in {unit}, got {value!r}")
    return value.amount


_K = TypeVar("_K")


def decoded(where: str, value: object, kind: type[_K]) -> _K:
    """A value ``decode_tagged`` produced, held to its declared kind. A
    ``datetime`` is a ``date`` to Python, but not to the contract."""
    if not isinstance(value, kind) or (kind is date and isinstance(value, datetime)):
        raise CodecError(f"{where}: expected {kind.__name__}, got {value!r}")
    return value
