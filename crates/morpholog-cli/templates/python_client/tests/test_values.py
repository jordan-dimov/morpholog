"""The value codecs, both directions, including the traps the floor
version makes real (Z-suffix timestamps on 3.10, scientific Decimal
rendering, naive datetimes)."""

import sys
import unittest
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from python_client import values


class DecodeTagged(unittest.TestCase):
    def test_every_tag_decodes_to_its_bare_python_value(self):
        cases = [
            ({"type": "subject", "value": "acct_1"}, "acct_1"),
            ({"type": "decimal", "value": "100.50"}, Decimal("100.50")),
            ({"type": "bool", "value": True}, True),
            ({"type": "date", "value": "2026-06-01"}, date(2026, 6, 1)),
            (
                {"type": "timestamp", "value": "2026-06-01T12:00:00Z"},
                datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc),
            ),
            ({"type": "duration", "value": "PT6H"}, "PT6H"),
            (
                {"type": "quantity", "value": {"amount": "25000", "unit": "USD"}},
                Decimal("25000"),
            ),
            (
                {"type": "collection", "value": [{"type": "subject", "value": "nested"}]},
                ["nested"],
            ),
        ]
        for tagged, expected in cases:
            self.assertEqual(values.decode_tagged(tagged), expected, tagged)

    def test_decimal_decodes_exactly_never_a_float(self):
        decoded = values.decode_tagged({"type": "decimal", "value": "0.1"})
        self.assertIsInstance(decoded, Decimal)
        self.assertEqual(str(decoded), "0.1")

    def test_unknown_tag_raises(self):
        with self.assertRaises(ValueError):
            values.decode_tagged({"type": "mystery", "value": 1})

    def test_malformed_shapes_raise(self):
        for bad in [
            "bare",
            {"type": "subject"},
            {"type": "subject", "value": "x", "extra": 1},
            {"type": "quantity", "value": {"amount": "1"}},
        ]:
            with self.assertRaises(ValueError, msg=repr(bad)):
                values.decode_tagged(bad)


class EncodeNamed(unittest.TestCase):
    def test_bare_kinds_encode_to_their_wire_shapes(self):
        cases = [
            ("acct_1", "acct_1"),
            (Decimal("100.50"), "100.50"),
            (True, True),
            (date(2026, 6, 1), "2026-06-01"),
            (
                datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc),
                "2026-06-01T12:00:00Z",
            ),
        ]
        for value, expected in cases:
            self.assertEqual(values.encode_named(value), expected, repr(value))

    def test_scientific_decimal_renders_plain(self):
        # Decimal("1E+2").__str__() is "1E+2"; the wire pattern refuses
        # scientific notation, so the encoder renders plain form.
        self.assertEqual(values.encode_named(Decimal("1E+2")), "100")

    def test_non_finite_decimals_are_refused(self):
        for bad in [Decimal("NaN"), Decimal("Infinity")]:
            with self.assertRaises(ValueError, msg=repr(bad)):
                values.encode_named(bad)

    def test_naive_datetime_is_refused(self):
        # An instant must name an instant; a naive datetime names one
        # only relative to an unstated zone.
        with self.assertRaises(ValueError):
            values.encode_named(datetime(2026, 6, 1, 12, 0, 0))

    def test_non_utc_offset_normalises_to_utc(self):
        from datetime import timedelta

        plus_two = timezone(timedelta(hours=2))
        encoded = values.encode_named(datetime(2026, 6, 1, 14, 0, 0, tzinfo=plus_two))
        self.assertEqual(encoded, "2026-06-01T12:00:00Z")

    def test_unsupported_types_are_refused(self):
        for bad in [1, 1.5, None, [1], {"k": "v"}]:
            with self.assertRaises(ValueError, msg=repr(bad)):
                values.encode_named(bad)


class ParseTimestamp(unittest.TestCase):
    def test_z_suffix_parses_on_the_floor_version(self):
        parsed = values.parse_timestamp("2026-06-01T12:00:00Z")
        self.assertEqual(parsed, datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc))

    def test_offsetless_text_is_refused(self):
        with self.assertRaises(ValueError):
            values.parse_timestamp("2026-06-01T12:00:00")

    def test_microsecond_fractions_parse_and_nanoseconds_are_refused_by_name(self):
        parsed = values.parse_timestamp("2026-06-01T12:00:00.123456Z")
        self.assertEqual(parsed.microsecond, 123456)
        with self.assertRaises(ValueError) as caught:
            values.parse_timestamp("2026-06-01T12:00:00.123456789Z")
        self.assertIn("sub-microsecond", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
