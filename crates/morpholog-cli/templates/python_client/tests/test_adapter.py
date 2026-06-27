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
if mode == "record_argv":
    with open(os.environ["STUB_ARGV_FILE"], "w") as f:
        f.write("\\n".join(sys.argv[1:]))
    print("[]")
    sys.exit(0)
if mode == "record_argv_empty":
    with open(os.environ["STUB_ARGV_FILE"], "w") as f:
        f.write("\\n".join(sys.argv[1:]))
    sys.exit(0)
if mode == "hang":
    import time
    time.sleep(30)
    sys.exit(0)
if mode == "audit_ndjson":
    row = ('{"transition_id": "01900000-0000-7000-8000-00000000000%d", '
           '"transformation_name": "post", "arguments": [], '
           '"actor": {"type": "subject", "value": "alex"}, '
           '"invariant_epoch": 1, "invariants_checked": [], '
           '"asserted_claims": [], "retracted_claims": [], '
           '"emitted_intents": [], '
           '"committed_at": "2026-06-01T12:00:0%d.000000Z"}')
    print(row % (1, 1))
    print(row % (2, 2))
    sys.exit(0)
raise SystemExit(f"unknown STUB_MODE {mode}")
"""


class AdapterDiscrimination(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._dir = tempfile.TemporaryDirectory()
        stub = Path(cls._dir.name) / "morpholog-stub"
        stub.write_text(STUB)
        stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
        cls.stub = stub
        cls.client = Morpholog("model.morph", "postgres:///stub", binary=str(stub))

    @classmethod
    def tearDownClass(cls):
        cls._dir.cleanup()

    def _mode(self, mode):
        os.environ["STUB_MODE"] = mode
        self.addCleanup(os.environ.pop, "STUB_MODE", None)

    def test_a_rejection_at_exit_1_is_a_decided_outcome_not_an_error(self):
        self._mode("rejected_exit_1")
        outcome = self.client.propose("t", "alex", {"x": "1"})
        self.assertIsInstance(outcome, envelopes.Rejected)
        self.assertIn("cap", outcome.reason)

    def test_empty_stdout_raises_with_the_stderr_text(self):
        self._mode("operational_failure")
        with self.assertRaises(MorphologError) as caught:
            self.client.propose("t", "alex", {"x": "1"})
        self.assertIn("failed to connect", str(caught.exception))

    def test_batch_returns_one_receipt_per_row(self):
        self._mode("batch_ok")
        receipts = self.client.propose_batch(
            [{"transformation": "t", "actor": "a", "args_named": {}}]
        )
        self.assertEqual([r.row for r in receipts], [1, 2])
        self.assertIsInstance(receipts[0].outcome, envelopes.Rejected)
        self.assertIsInstance(receipts[1].outcome, envelopes.BatchError)

    def test_an_aborted_batch_raises_naming_the_receipts_that_arrived(self):
        self._mode("batch_aborted")
        with self.assertRaises(MorphologError) as caught:
            self.client.propose_batch(
                [{"transformation": "t", "actor": "a", "args_named": {}}]
            )
        self.assertIn("1 receipt", str(caught.exception))
        self.assertIn("aborted at row 2", str(caught.exception))

    def test_as_of_threads_through_both_claims_reads(self):
        # The flag lands on the CLI argv exactly when supplied, on both
        # reads, and is absent otherwise - all four cases, since the
        # issue asked for both surfaces.
        self._mode("record_argv")
        with tempfile.NamedTemporaryFile(mode="r", suffix=".argv") as record:
            os.environ["STUB_ARGV_FILE"] = record.name
            self.addCleanup(os.environ.pop, "STUB_ARGV_FILE", None)

            def argv_after(call):
                call()
                with open(record.name) as handle:
                    return handle.read().splitlines()

            argv = argv_after(
                lambda: self.client.claims_named(
                    "OfficialCurve", as_of="2026-06-07T12:00:00Z"
                )
            )
            self.assertIn("--as-of", argv)
            self.assertEqual(argv[argv.index("--as-of") + 1], "2026-06-07T12:00:00Z")
            self.assertIn("--named", argv)

            argv = argv_after(lambda: self.client.claims_named("OfficialCurve"))
            self.assertNotIn("--as-of", argv)
            self.assertIn("--named", argv)

            argv = argv_after(
                lambda: self.client.claims("OfficialCurve", as_of="2026-06-07T12:00:00Z")
            )
            self.assertIn("--as-of", argv)
            self.assertNotIn("--named", argv)

            argv = argv_after(lambda: self.client.claims("OfficialCurve"))
            self.assertNotIn("--as-of", argv)
            self.assertNotIn("--named", argv)

    def test_audit_flags_land_on_argv_exactly_when_supplied(self):
        # The four-case matrix for the audit tail: --after and --named
        # each appear exactly when asked for.
        self._mode("record_argv_empty")
        with tempfile.NamedTemporaryFile(mode="r", suffix=".argv") as record:
            os.environ["STUB_ARGV_FILE"] = record.name
            self.addCleanup(os.environ.pop, "STUB_ARGV_FILE", None)

            def argv_after(call):
                call()
                with open(record.name) as handle:
                    return handle.read().splitlines()

            tid = "01900000-0000-7000-8000-000000000001"
            argv = argv_after(lambda: self.client.audit_named(after=tid))
            self.assertIn("--after", argv)
            self.assertEqual(argv[argv.index("--after") + 1], tid)
            self.assertIn("--named", argv)

            argv = argv_after(lambda: self.client.audit_named())
            self.assertNotIn("--after", argv)
            self.assertIn("--named", argv)

            argv = argv_after(lambda: self.client.audit(after=tid))
            self.assertIn("--after", argv)
            self.assertNotIn("--named", argv)

            argv = argv_after(lambda: self.client.audit())
            self.assertNotIn("--after", argv)
            self.assertNotIn("--named", argv)

    def test_audit_empty_tail_is_a_lawful_empty_list(self):
        self._mode("record_argv_empty")
        with tempfile.NamedTemporaryFile(mode="r", suffix=".argv") as record:
            os.environ["STUB_ARGV_FILE"] = record.name
            self.addCleanup(os.environ.pop, "STUB_ARGV_FILE", None)
            self.assertEqual(self.client.audit(), [])

    def test_audit_parses_one_row_per_ndjson_line_in_order(self):
        self._mode("audit_ndjson")
        rows = self.client.audit()
        self.assertEqual(len(rows), 2)
        self.assertTrue(rows[0].transition_id.endswith("1"))
        self.assertTrue(rows[1].transition_id.endswith("2"))
        self.assertEqual(rows[0].actor, "alex")

    def test_audit_operational_failure_raises_with_stderr(self):
        self._mode("operational_failure")
        with self.assertRaises(MorphologError) as caught:
            self.client.audit()
        self.assertIn("failed to connect", str(caught.exception))

    def test_a_client_timeout_surfaces_as_an_operational_error(self):
        self._mode("hang")
        bounded = Morpholog(
            "model.morph", "postgres:///stub", binary=str(self.stub), timeout=0.2
        )
        with self.assertRaises(MorphologError) as caught:
            bounded.check()
        self.assertIn("timed out", str(caught.exception))

    def test_audit_uses_the_client_timeout(self):
        # The audit path does not go through _invoke (empty stdout is
        # lawful there), so it needs its own pin on the _run seam.
        self._mode("hang")
        bounded = Morpholog(
            "model.morph", "postgres:///stub", binary=str(self.stub), timeout=0.2
        )
        with self.assertRaises(MorphologError) as caught:
            bounded.audit()
        self.assertIn("timed out", str(caught.exception))

    def test_batch_takes_a_per_call_timeout_override(self):
        # The default client carries no timeout; the override bounds
        # this one batch.
        self._mode("hang")
        with self.assertRaises(MorphologError) as caught:
            self.client.propose_batch(
                [{"transformation": "t", "actor": "a", "args_named": {}}], timeout=0.2
            )
        self.assertIn("timed out", str(caught.exception))

    def test_batch_timeout_stays_the_second_positional_arg(self):
        # explain_on_reject is keyword-only, so an old caller passing the
        # timeout positionally still bounds the call - it does not silently
        # flip the explain flag (the API-compat catch from review).
        self._mode("hang")
        with self.assertRaises(MorphologError) as caught:
            self.client.propose_batch(
                [{"transformation": "t", "actor": "a", "args_named": {}}], 0.2
            )
        self.assertIn("timed out", str(caught.exception))

    def test_batch_explain_on_reject_lands_on_argv_exactly_when_supplied(self):
        # The flag composes with --batch on the CLI; the client passes
        # it through only when asked, so each rejected row carries the
        # same-snapshot why.
        self._mode("record_argv_empty")
        with tempfile.NamedTemporaryFile(mode="r", suffix=".argv") as record:
            os.environ["STUB_ARGV_FILE"] = record.name
            self.addCleanup(os.environ.pop, "STUB_ARGV_FILE", None)

            def argv_after(call):
                call()
                with open(record.name) as handle:
                    return handle.read().splitlines()

            rows = [{"transformation": "t", "actor": "a", "args_named": {}}]
            argv = argv_after(
                lambda: self.client.propose_batch(rows, explain_on_reject=True)
            )
            self.assertIn("--batch", argv)
            self.assertIn("--explain-on-reject", argv)

            argv = argv_after(lambda: self.client.propose_batch(rows))
            self.assertIn("--batch", argv)
            self.assertNotIn("--explain-on-reject", argv)

    def test_submit_is_duck_typed_on_the_request_protocol(self):
        self._mode("rejected_exit_1")

        class FakeRequest:
            TRANSFORMATION = "t"

            @staticmethod
            def to_args_named():
                return {"x": "1"}

        outcome = self.client.submit(FakeRequest(), "alex")
        self.assertIsInstance(outcome, envelopes.Rejected)

    def test_checkpoint_signing_key_and_key_id_must_be_given_together(self):
        # The guard raises before any subprocess, so the stub never runs.
        with self.assertRaises(ValueError):
            self.client.checkpoint(signing_key="k.pem")
        with self.assertRaises(ValueError):
            self.client.checkpoint(key_id="k1")


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
