"""The value codecs, both directions, including the traps the standard
library makes real (scientific Decimal rendering, naive datetimes)."""

import unittest
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path

from _support import TEMPLATES_DIR
from python_client import values


class ModuleUnderTest(unittest.TestCase):
    def test_the_codec_under_test_is_the_template_not_an_installed_copy(self):
        self.assertEqual(Path(values.__file__).resolve().parent, TEMPLATES_DIR / "python_client")


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
                values.Quantity(Decimal("25000"), "USD"),
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

    def test_wrong_payload_types_are_refused_never_coerced(self):
        # Decoding validates, never coerces: bool("false") is True,
        # Decimal(1.1) smuggles a float in inexactly, and iterating a
        # string "collection" iterates its characters - none of those
        # paths may exist in a drift tripwire.
        for bad in [
            {"type": "bool", "value": "false"},
            {"type": "bool", "value": 1},
            {"type": "subject", "value": 7},
            {"type": "decimal", "value": 123},
            {"type": "decimal", "value": 1.1},
            {"type": "date", "value": 20260601},
            {"type": "timestamp", "value": 1750000000},
            {"type": "duration", "value": 21600},
            {"type": "quantity", "value": {"amount": 25000, "unit": "USD"}},
            {"type": "collection", "value": "abc"},
        ]:
            with self.assertRaises(values.CodecError, msg=repr(bad)):
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
    def test_either_case_of_z_is_utc(self):
        expected = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)
        self.assertEqual(values.parse_timestamp("2026-06-01T12:00:00Z"), expected)
        self.assertEqual(values.parse_timestamp("2026-06-01T12:00:00z"), expected)

    def test_offsetless_text_is_refused(self):
        with self.assertRaises(ValueError):
            values.parse_timestamp("2026-06-01T12:00:00")

    def test_microsecond_fractions_parse_and_nanoseconds_are_refused_by_name(self):
        parsed = values.parse_timestamp("2026-06-01T12:00:00.123456Z")
        self.assertEqual(parsed.microsecond, 123456)
        with self.assertRaises(ValueError) as caught:
            values.parse_timestamp("2026-06-01T12:00:00.123456789Z")
        self.assertIn("sub-microsecond", str(caught.exception))



class WireFormats(unittest.TestCase):
    """Decoding reads only what the schema publishes, and every refusal is
    a `CodecError`, never the standard library's own exception."""

    def refused(self, tagged):
        with self.assertRaises(values.CodecError, msg=repr(tagged)):
            values.decode_tagged(tagged)

    def test_decimal_text_outside_the_wire_pattern_is_refused(self):
        for text in ("NaN", "Infinity", "1e5", "01", "1.", ".5", "+1", " 1", "1_000"):
            self.refused({"type": "decimal", "value": text})
            self.refused({"type": "quantity", "value": {"amount": text, "unit": "USD"}})

    def test_dates_outside_yyyy_mm_dd_or_the_calendar_are_refused(self):
        for text in ("20260102", "2026-W01-1", "2026-02-30", "2026-1-2", "0000-01-01", ""):
            self.refused({"type": "date", "value": text})

    def test_timestamps_outside_rfc_3339_are_refused(self):
        for text in (
            "2026-06-01 12:00:00Z",
            "2026-06-01T12:00Z",
            "2026-06-01T12:00:00",
            "2026-06-01T12:00:00+0000",
            "2026-06-01T25:00:00Z",
            "20260601T120000Z",
        ):
            self.refused({"type": "timestamp", "value": text})

    def test_a_quantity_unit_must_be_a_string(self):
        for unit in (None, 7, ["USD"]):
            self.refused({"type": "quantity", "value": {"amount": "1", "unit": unit}})

    def test_durations_outside_exact_spans_are_refused(self):
        for text in ("6H", "P1D", "PT", "PT1.S", "PT6h", ""):
            self.refused({"type": "duration", "value": text})

    def test_the_spellings_the_contract_allows_are_read(self):
        cases = [
            ({"type": "decimal", "value": "-0.50"}, Decimal("-0.50")),
            (
                {"type": "timestamp", "value": "2026-06-01t13:30:00+01:30"},
                datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc),
            ),
            ({"type": "duration", "value": "-PT1H30M"}, "-PT1H30M"),
            ({"type": "duration", "value": "PT0.5S"}, "PT0.5S"),
            ({"type": "date", "value": "0001-01-01"}, date(1, 1, 1)),
        ]
        for tagged, expected in cases:
            self.assertEqual(values.decode_tagged(tagged), expected, tagged)


if __name__ == "__main__":
    unittest.main()
