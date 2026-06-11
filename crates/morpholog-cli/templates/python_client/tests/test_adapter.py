"""The subprocess adapter against a stub binary, pinning the
discrimination rule: decided results arrive on stdout even at exit 1;
empty stdout is the only operational failure."""

import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from python_client import envelopes
from python_client.adapter import Morpholog, MorphologError

STUB = """#!/usr/bin/env python3
import os, sys
mode = os.environ["STUB_MODE"]
if mode == "rejected_exit_1":
    print('{"status": "rejected", "reason": "invariant `cap` violated"}')
    sys.exit(1)
if mode == "operational_failure":
    print("error: failed to connect to PostgreSQL", file=sys.stderr)
    sys.exit(1)
if mode == "batch_ok":
    print('{"row": 1, "status": "rejected", "reason": "closed period"}')
    print('{"row": 2, "status": "error", "error": "malformed batch row"}')
    sys.exit(0)
if mode == "batch_aborted":
    print('{"row": 1, "status": "rejected", "reason": "closed period"}')
    print("batch aborted at row 2", file=sys.stderr)
    sys.exit(1)
raise SystemExit(f"unknown STUB_MODE {mode}")
"""


class AdapterDiscrimination(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._dir = tempfile.TemporaryDirectory()
        stub = Path(cls._dir.name) / "morpholog-stub"
        stub.write_text(STUB)
        stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
        cls.client = Morpholog("model.morph", "postgres:///stub", binary=str(stub))

    @classmethod
    def tearDownClass(cls):
        cls._dir.cleanup()

    def _mode(self, mode):
        os.environ["STUB_MODE"] = mode
        self.addCleanup(os.environ.pop, "STUB_MODE", None)

    def test_a_rejection_at_exit_1_is_a_decided_outcome_not_an_error(self):
        self._mode("rejected_exit_1")
        outcome = self.client.run("t", "alex", {"x": "1"})
        self.assertIsInstance(outcome, envelopes.Rejected)
        self.assertIn("cap", outcome.reason)

    def test_empty_stdout_raises_with_the_stderr_text(self):
        self._mode("operational_failure")
        with self.assertRaises(MorphologError) as caught:
            self.client.run("t", "alex", {"x": "1"})
        self.assertIn("failed to connect", str(caught.exception))

    def test_batch_returns_one_receipt_per_row(self):
        self._mode("batch_ok")
        receipts = self.client.run_batch(
            [{"transformation": "t", "actor": "a", "args_named": {}}]
        )
        self.assertEqual([r.row for r in receipts], [1, 2])
        self.assertIsInstance(receipts[0].outcome, envelopes.Rejected)
        self.assertIsInstance(receipts[1].outcome, envelopes.BatchError)

    def test_an_aborted_batch_raises_naming_the_receipts_that_arrived(self):
        self._mode("batch_aborted")
        with self.assertRaises(MorphologError) as caught:
            self.client.run_batch(
                [{"transformation": "t", "actor": "a", "args_named": {}}]
            )
        self.assertIn("1 receipt", str(caught.exception))
        self.assertIn("aborted at row 2", str(caught.exception))

    def test_submit_is_duck_typed_on_the_request_protocol(self):
        self._mode("rejected_exit_1")

        class FakeRequest:
            TRANSFORMATION = "t"

            @staticmethod
            def to_args_named():
                return {"x": "1"}

        outcome = self.client.submit(FakeRequest(), "alex")
        self.assertIsInstance(outcome, envelopes.Rejected)


class BinaryDiscovery(unittest.TestCase):
    def test_explicit_argument_then_env_then_path_default(self):
        explicit = Morpholog("m.morph", "db", binary="/custom/bin")
        self.assertEqual(explicit.binary, "/custom/bin")
        os.environ["MORPHOLOG_BIN"] = "/from/env"
        self.addCleanup(os.environ.pop, "MORPHOLOG_BIN", None)
        from_env = Morpholog("m.morph", "db")
        self.assertEqual(from_env.binary, "/from/env")
        del os.environ["MORPHOLOG_BIN"]
        defaulted = Morpholog("m.morph", "db")
        self.assertEqual(defaulted.binary, "morpholog")


if __name__ == "__main__":
    unittest.main()
