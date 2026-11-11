"""The client's digest against the kernel's golden: one file holds both
encoders to the same bytes, so a commitment computed before the close is
the one the record holds a reveal to."""

import json
import unittest
from datetime import date, datetime, timedelta, timezone
from decimal import Decimal

from _support import TEMPLATES_DIR
from python_client import values
from python_client.values import CodecError, digest

GOLDEN = TEMPLATES_DIR.parents[1] / "morpholog-core" / "tests" / "golden" / "digest_v1.json"
# The tags the helper takes bare values for; the rest have no bare form
# in this client and are refused.
BARE = {"subject", "decimal", "bool", "date", "timestamp"}


class AgainstTheKernelsGolden(unittest.TestCase):
    def test_the_golden_is_the_kernels(self):
        self.assertTrue(GOLDEN.is_file(), GOLDEN)

    def test_every_bare_case_digests_to_the_kernels_bytes(self):
        cases = json.loads(GOLDEN.read_text())
        covered = 0
        for case in cases:
            if any(arg["type"] not in BARE for arg in case["args"]):
                continue
            try:
                bare = [values.decode_tagged(arg) for arg in case["args"]]
            except CodecError:
                # An instant finer than a microsecond: the client refuses
                # it on decode, so it can never commit to one either.
                continue
            with self.subTest(case["name"]):
                self.assertEqual(values._digest_preimage(tuple(bare)), case["preimage"])
                self.assertEqual(digest(*bare), case["digest"])
            covered += 1
        self.assertGreaterEqual(covered, 15)


class WhatTheHelperRefuses(unittest.TestCase):
    def test_an_int_or_a_float_is_not_a_decimal(self):
        for value in (1, 1.0, 100):
            with self.subTest(repr(value)), self.assertRaises(CodecError):
                digest(value)

    def test_a_naive_datetime_and_a_non_finite_decimal_are_refused(self):
        with self.assertRaises(CodecError):
            digest(datetime(2026, 6, 1, 12))
        with self.assertRaises(CodecError):
            digest(Decimal("NaN"))
        with self.assertRaises(CodecError):
            digest()


class WhatTheKernelCanHold(unittest.TestCase):
    """A commitment over a decimal the kernel would round could never be
    revealed, so the codec refuses it at the kernel's own limits: a
    96-bit coefficient and 28 decimal places."""

    def test_the_largest_and_smallest_decimals_are_the_kernels(self):
        digest(Decimal("79228162514264337593543950335"))
        digest(Decimal("-79228162514264337593543950335"))
        digest(Decimal("0.0000000000000000000000000001"))
        for too_far in ("79228162514264337593543950336", "1E+29", "1E-29", "1E+1000000"):
            with self.subTest(too_far), self.assertRaises(CodecError):
                digest(Decimal(too_far))

    def test_trailing_zeros_past_the_limit_are_not_precision(self):
        self.assertEqual(
            digest(Decimal("1.0000000000000000000000000000000")), digest(Decimal("1"))
        )
        self.assertEqual(digest(Decimal("0E-40")), digest(Decimal("0")))
        with self.assertRaises(CodecError):
            digest(Decimal("1.00000000000000000000000000001"))


class WhatTheHelperEquates(unittest.TestCase):
    def test_scale_is_not_part_of_a_decimal(self):
        self.assertEqual(digest(Decimal("100.00")), digest(Decimal("100")))
        self.assertEqual(digest(Decimal("-0.0")), digest(Decimal("0")))

    def test_an_instant_at_another_offset_is_the_same_instant(self):
        utc = datetime(2026, 6, 1, 12, tzinfo=timezone.utc)
        plus_one = datetime(2026, 6, 1, 13, tzinfo=timezone(timedelta(hours=1)))
        self.assertEqual(digest(utc), digest(plus_one))

    def test_kinds_and_order_tell_values_apart(self):
        self.assertNotEqual(digest(True), digest("true"))
        self.assertNotEqual(digest(Decimal("1")), digest("1"))
        self.assertNotEqual(digest("a", "b"), digest("b", "a"))
        self.assertNotEqual(digest(date(2026, 6, 1)), digest("2026-06-01"))


if __name__ == "__main__":
    unittest.main()
