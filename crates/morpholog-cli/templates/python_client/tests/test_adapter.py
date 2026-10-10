"""The subprocess adapter against a stub binary, pinning the
discrimination rule: decided results arrive on stdout even at exit 1.
For a read, empty stdout is an operational failure. For a proposal,
only the binary's own statement - a decided envelope or a coded error
object - settles the outcome; anything else is unknown."""

import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from _support import GOLDEN_DIR, recording_argv

from python_client import envelopes
from python_client.adapter import (
    Morpholog,
    MorphologBatchIncomplete,
    MorphologError,
    MorphologOutcomeUnknown,
    MorphologRequestError,
    MorphologTimeout,
)

STUB = """#!/usr/bin/env python3
import os, sys
mode = os.environ["STUB_MODE"]
if mode == "rejected_exit_1":
    print('{"status": "rejected", "reason": "invariant `cap` violated"}')
    sys.exit(1)
if mode == "operational_failure":
    print("error: failed to connect to PostgreSQL", file=sys.stderr)
    sys.exit(1)
if mode == "not_committed_exit_1":
    print("Error: the proposal was not committed: check constraint", file=sys.stderr)
    sys.exit(1)
if mode == "not_committed_object":
    print('{"status": "error", "code": "not_committed", "error": "check constraint"}')
    print("Error: the proposal was not committed: check constraint", file=sys.stderr)
    sys.exit(1)
if mode == "killed":
    sys.stdout.flush()
    os.kill(os.getpid(), 9)
if mode == "stdout_then_exit":
    sys.stdout.write(os.environ["STUB_STDOUT"])
    sys.stdout.flush()
    sys.exit(int(os.environ.get("STUB_EXIT", "0")))
if mode == "batch_receipt_then_killed":
    print('{"row": 1, "status": "rejected", "reason": "closed period"}', flush=True)
    os.kill(os.getpid(), 9)
if mode == "echo_conninfo_then_hang":
    i = sys.argv.index("--database-url")
    print(f"connecting to {sys.argv[i + 1]}", file=sys.stderr, flush=True)
    import time
    time.sleep(30)
if mode == "batch_receipt_then_hang":
    print('{"row": 1, "status": "rejected", "reason": "closed period"}', flush=True)
    import time
    time.sleep(30)
if mode == "commit_outcome_unknown_exit_3":
    print("Error: the commit outcome is unknown - read the record", file=sys.stderr)
    sys.exit(3)
if mode == "transact_stdout":
    sys.stdin.read()
    print(os.environ["STUB_STDOUT"])
    sys.exit(int(os.environ.get("STUB_EXIT", "0")))
if mode == "usage_error_exit_2":
    print("error: unexpected argument '--bogus'", file=sys.stderr)
    sys.exit(2)
if mode == "batch_ok":
    print('{"row": 1, "status": "rejected", "reason": "closed period"}')
    print('{"row": 2, "status": "error", "code": "invalid_request", "error": "malformed batch row"}')
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
if mode == "record_argv_stdout":
    with open(os.environ["STUB_ARGV_FILE"], "w") as f:
        f.write("\\n".join(sys.argv[1:]))
    print(os.environ["STUB_STDOUT"])
    sys.exit(0)
if mode == "hang":
    import time
    time.sleep(30)
    sys.exit(0)
if mode == "pinned":
    with open(os.environ["STUB_ARGV_FILE"], "a") as f:
        f.write(" ".join(sys.argv[1:3]) + "\\n")
    if sys.argv[1] == "hash":
        print(os.environ["STUB_HASH_STDOUT"])
        sys.exit(0)
    print("[]")
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
if mode == "stderr_echoes_conninfo":
    i = sys.argv.index("--database-url")
    print(f"error: could not connect to {sys.argv[i + 1]}", file=sys.stderr)
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

    def test_where_narrows_by_argument_on_both_named_reads(self):
        # The read pattern the trial reported: reconcile.py read every
        # InvoiceLine and filtered by invoice in Python. Both named reads
        # now carry the filter, so the question goes to the binary.
        self._mode("record_argv")
        with recording_argv() as argv_after:
            argv = argv_after(
                lambda: self.client.claims_named(
                    "InvoiceLine", where={"invoice_id": "inv_1"}
                )
            )
            self.assertIn("--where", argv)
            self.assertEqual(argv[argv.index("--where") + 1], "invoice_id=inv_1")

            argv = argv_after(lambda: self.client.claims_named("InvoiceLine"))
            self.assertNotIn("--where", argv)

            argv = argv_after(
                lambda: self.client.derived_named("StaleLine", where={"invoice_id": "inv_1"})
            )
            self.assertIn("--where", argv)

    def test_init_can_reach_the_destructive_reset(self):
        # The fixture embedder-integration.md documents was reachable only
        # from the CLI, so an embedder wanting it had to shell out around
        # its own generated client. Both flags pass straight through: the
        # binary owns the pairing rule, this layer does not re-implement it.
        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = json.dumps({"status": "created", "schema": "morpholog"})
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with recording_argv() as argv_after:
            argv = argv_after(
                lambda: self.client.init(reset=True, i_know_this_deletes_data=True)
            )
            self.assertIn("--reset", argv)
            self.assertIn("--i-know-this-deletes-data", argv)

            argv = argv_after(lambda: self.client.init())
            self.assertNotIn("--reset", argv)
            self.assertNotIn("--i-know-this-deletes-data", argv)

    def test_provisioning_names_every_programme_and_asks_for_the_envelope(self):
        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "provision_report_pruned.json").read_text()
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with recording_argv() as argv_after:
            argv = argv_after(
                lambda: self.client.provision_indexes(
                    prune=True, with_programs=["billing.morph"]
                )
            )
            self.assertEqual(
                argv[:4], ["provision", "indexes", self.client.file, "billing.morph"]
            )
            self.assertIn("--json", argv)
            self.assertIn("--prune", argv)
            self.assertNotIn("--dry-run", argv)

            argv = argv_after(lambda: self.client.provision_indexes(dry_run=True))
            self.assertEqual(argv[:3], ["provision", "indexes", self.client.file])
            self.assertIn("--dry-run", argv)
            self.assertNotIn("--prune", argv)
            self.assertNotIn("--check", argv)

            argv = argv_after(lambda: self.client.provision_indexes(check=True))
            self.assertIn("--check", argv)
            self.assertNotIn("--dry-run", argv)

    def test_a_provisioning_conflict_is_a_report_not_a_raise(self):
        # The binary exits non-zero on a conflict with the report already
        # on stdout. A raise would throw away which index conflicts.
        self._export_stdout((GOLDEN_DIR / "provision_report_conflict.json").read_text(), 1)
        report = self.client.provision_indexes()
        self.assertTrue(report.has_conflict)
        self.assertFalse(report.applied)

    def test_provisioning_with_nothing_on_stdout_is_operational(self):
        self._mode("operational_failure")
        with self.assertRaises(MorphologError) as caught:
            self.client.provision_indexes()
        self.assertIn("failed to connect", str(caught.exception))

    def test_an_index_build_is_not_bounded_by_the_client_wide_timeout(self):
        self._export_stdout((GOLDEN_DIR / "provision_report_applied.json").read_text(), 0)
        hurried = Morpholog(
            self.client.file, self.client.database_url, binary=self.client.binary, timeout=1e-9
        )
        self.assertTrue(hurried.provision_indexes().applied)
        self._mode("hang")
        with self.assertRaises(MorphologTimeout):
            hurried.provision_indexes(timeout=0.5)

    # ------------------------------------------------------------
    # The pin: a generated client checks the binary before its first
    # call, through the one subprocess seam.
    # ------------------------------------------------------------

    HASH = "sha256:" + "a" * 64

    def _pinned(self, hash_stdout: object, **pins) -> tuple:
        """A pinned client over a stub answering `hash` with
        ``hash_stdout`` and every other command with ``[]``, plus the
        path of the stub's record of what ran."""
        self._mode("pinned")
        record = tempfile.NamedTemporaryFile(mode="r", suffix=".argv", delete=False)
        self.addCleanup(os.unlink, record.name)
        os.environ["STUB_ARGV_FILE"] = record.name
        self.addCleanup(os.environ.pop, "STUB_ARGV_FILE", None)
        os.environ["STUB_HASH_STDOUT"] = json.dumps(hash_stdout)
        self.addCleanup(os.environ.pop, "STUB_HASH_STDOUT", None)
        pins.setdefault("expected_version", "0.0.0")
        pins.setdefault("expected_model_hash", self.HASH)
        client = Morpholog("model.morph", "postgres:///stub", binary=str(self.stub), **pins)
        return client, record.name

    @staticmethod
    def _ran(record: str) -> list[str]:
        return Path(record).read_text().splitlines()

    def test_a_pinned_client_refuses_another_version_before_its_first_call(self):
        client, record = self._pinned(
            {"hash": self.HASH, "morpholog_version": "9.9.9", "program": "p"}
        )
        with self.assertRaises(MorphologError) as caught:
            client.claims()
        message = str(caught.exception)
        self.assertIn("9.9.9", message)
        self.assertIn("0.0.0", message)
        self.assertIn("MORPHOLOG_BIN", message)
        self.assertEqual(self._ran(record), ["hash model.morph"], "nothing else ran")
        # Once refused, refused: no second look at the binary.
        with self.assertRaises(MorphologError) as again:
            client.claims()
        self.assertEqual(str(again.exception), message)
        self.assertEqual(self._ran(record), ["hash model.morph"])

    def test_a_binary_that_predates_the_handshake_is_refused_by_that_name(self):
        # A binary from before the report carried a version answers with
        # the two-field report. The client cannot say which version it is,
        # only that it is not the one it was generated for.
        client, record = self._pinned({"hash": self.HASH, "program": "p"})
        with self.assertRaises(MorphologError) as caught:
            client.claims()
        self.assertIn("predates the versioned hash report", str(caught.exception))
        self.assertIn("0.0.0", str(caught.exception))
        self.assertEqual(self._ran(record), ["hash model.morph"])

    def test_a_report_stating_no_version_in_a_new_shape_is_drift_not_an_old_binary(self):
        # Only the exact pre-versioned shape is named as old. Anything else
        # without a version is a report this client cannot read.
        client, _ = self._pinned({"hash": self.HASH, "program": "p", "novel": 1})
        with self.assertRaises(envelopes.EnvelopeError):
            client.claims()

    def test_the_version_is_diagnosed_before_envelope_drift(self):
        # A report this client cannot parse, from a binary of another
        # version: the version is the reason, so it is the diagnosis.
        client, _ = self._pinned(
            {"hash": self.HASH, "morpholog_version": "9.9.9", "program": "p", "novel": 1}
        )
        with self.assertRaises(MorphologError) as caught:
            client.claims()
        self.assertIn("9.9.9", str(caught.exception))
        self.assertNotIn("unknown key", str(caught.exception))

    def test_strictness_holds_once_the_versions_agree(self):
        client, _ = self._pinned(
            {"hash": self.HASH, "morpholog_version": "0.0.0", "program": "p", "novel": 1}
        )
        with self.assertRaises(envelopes.EnvelopeError) as caught:
            client.claims()
        self.assertIn("unknown key", str(caught.exception))

    def test_a_pinned_client_that_agrees_runs_and_checks_once(self):
        client, record = self._pinned(
            {"hash": self.HASH, "morpholog_version": "0.0.0", "program": "p"}
        )
        client.claims()
        client.claims()
        ran = self._ran(record)
        self.assertEqual(ran[0], "hash model.morph")
        self.assertEqual(ran.count("hash model.morph"), 1)
        self.assertEqual(len(ran), 3)

    def test_other_rules_than_the_client_was_generated_from_refuse(self):
        other = "sha256:" + "b" * 64
        client, record = self._pinned(
            {"hash": other, "morpholog_version": "0.0.0", "program": "p"}
        )
        with self.assertRaises(MorphologError) as caught:
            client.claims()
        self.assertIn(other, str(caught.exception))
        self.assertIn(self.HASH, str(caught.exception))
        self.assertEqual(self._ran(record), ["hash model.morph"])

    def test_a_client_constructed_directly_is_unpinned(self):
        client, record = self._pinned(
            {"hash": self.HASH, "morpholog_version": "9.9.9", "program": "p"},
            expected_version=None,
            expected_model_hash=None,
        )
        client.claims()
        self.assertNotIn("hash model.morph", self._ran(record))

    def test_the_pin_guards_every_path_to_the_binary(self):
        # Paths that do not go through the ordinary invocation still go
        # through the one seam, so the pin is asked first there too.
        client, record = self._pinned(
            {"hash": self.HASH, "morpholog_version": "9.9.9", "program": "p"}
        )
        with self.assertRaises(MorphologError):
            client.provision_indexes()
        self.assertEqual(self._ran(record), ["hash model.morph"])

    def test_the_constructor_pins_are_the_generated_clients_surface(self):
        # An embedder that subclasses the client passes the two pins by
        # name; ``open_client`` / ``open_session`` are the same call. The
        # drift gate cannot see a subclass, so the names are pinned here.
        import inspect

        from python_client.session import Session

        for cls in (Morpholog, Session):
            params = inspect.signature(cls.__init__).parameters
            for name in ("expected_version", "expected_model_hash"):
                self.assertIn(name, params, f"{cls.__name__}({name}=...)")
                param = params[name]
                self.assertIsNone(param.default)
                self.assertIn(
                    param.kind,
                    (param.POSITIONAL_OR_KEYWORD, param.KEYWORD_ONLY),
                    f"{cls.__name__}({name}=...) is passed by keyword",
                )

    def test_the_check_keeps_its_report_and_the_first_hash_returns_it(self):
        agreeing = {"hash": self.HASH, "morpholog_version": "0.0.0", "program": "p"}
        # A verify that opens with hash(): the check's own report, one spawn.
        client, record = self._pinned(agreeing)
        report = client.hash()
        self.assertEqual(report.hash, self.HASH)
        self.assertEqual(self._ran(record), ["hash model.morph"])
        # Later calls ask the binary again, as before.
        client.hash()
        self.assertEqual(self._ran(record), ["hash model.morph"] * 2)
        # The kept report survives other calls until hash() takes it.
        client, record = self._pinned(agreeing)
        client.claims()
        client.hash()
        self.assertEqual(self._ran(record).count("hash model.morph"), 1)
        # An unpinned client has no check to keep a report from.
        client, record = self._pinned(agreeing, expected_version=None, expected_model_hash=None)
        client.hash()
        self.assertEqual(self._ran(record), ["hash model.morph"])

    def test_a_binary_that_cannot_start_is_an_operational_error(self):
        # A mis-set MORPHOLOG_BIN raises the client's own error from every
        # generated method, never the OSError out of subprocess. Nothing
        # ran, so a proposal is plain operational, not outcome-unknown.
        missing = str(Path(self._dir.name) / "no-such-morpholog")
        client = Morpholog("model.morph", "postgres:///stub", binary=missing)
        with self.assertRaises(MorphologError) as caught:
            client.claims()
        self.assertIn("failed to start", str(caught.exception))
        self.assertIn(missing, str(caught.exception))
        with self.assertRaises(MorphologError) as proposal:
            client.propose("post", "alex", {})
        self.assertNotIsInstance(proposal.exception, MorphologOutcomeUnknown)
        pinned = Morpholog(
            "model.morph",
            "postgres:///stub",
            binary=missing,
            expected_version="0.0.0",
            expected_model_hash=self.HASH,
        )
        with self.assertRaises(MorphologError) as checked:
            pinned.hash()
        self.assertIn("failed to start", str(checked.exception))

    def test_empty_stdout_on_a_read_is_operational_and_on_a_proposal_is_unknown(self):
        self._mode("operational_failure")
        with self.assertRaises(MorphologError) as read:
            self.client.claims("Entry")
        self.assertNotIsInstance(read.exception, MorphologOutcomeUnknown)
        self.assertIn("failed to connect", str(read.exception))
        with self.assertRaises(MorphologOutcomeUnknown) as proposal:
            self.client.propose("t", "alex", {"x": "1"})
        self.assertIn("failed to connect", str(proposal.exception))

    def test_batch_returns_one_receipt_per_row(self):
        self._mode("batch_ok")
        row = {"transformation": "t", "actor": "a", "args_named": {}}
        receipts = self.client.propose_batch([row, row])
        self.assertEqual([r.row for r in receipts], [1, 2])
        self.assertIsInstance(receipts[0].outcome, envelopes.Rejected)
        self.assertIsInstance(receipts[1].outcome, envelopes.BatchError)

    def test_an_aborted_batch_names_what_finished_what_is_unknown_and_what_never_ran(self):
        self._mode("batch_aborted")
        row = {"transformation": "t", "actor": "a", "args_named": {}}
        with self.assertRaises(MorphologBatchIncomplete) as caught:
            self.client.propose_batch([row, row, row])
        self.assertEqual([r.row for r in caught.exception.receipts], [1])
        self.assertEqual(caught.exception.unknown_rows, [2])
        self.assertEqual(caught.exception.not_attempted, [3])
        self.assertFalse(caught.exception.retriable)
        self.assertIn("aborted at row 2", str(caught.exception))

    def test_a_killed_batch_keeps_the_receipts_that_arrived(self):
        # The binary flushes each receipt; a kill after row 1 leaves row 1
        # decided, row 2 unknown and row 3 never run.
        self._mode("batch_receipt_then_killed")
        row = {"transformation": "t", "actor": "a", "args_named": {}}
        with self.assertRaises(MorphologBatchIncomplete) as caught:
            self.client.propose_batch([row, row, row])
        self.assertEqual([r.row for r in caught.exception.receipts], [1])
        self.assertEqual(caught.exception.unknown_rows, [2])
        self.assertEqual(caught.exception.not_attempted, [3])

    def test_a_timed_out_batch_keeps_the_receipts_that_arrived(self):
        self._mode("batch_receipt_then_hang")
        row = {"transformation": "t", "actor": "a", "args_named": {}}
        with self.assertRaises(MorphologBatchIncomplete) as caught:
            self.client.propose_batch([row, row], timeout=0.5)
        self.assertEqual([r.row for r in caught.exception.receipts], [1])
        self.assertEqual(caught.exception.unknown_rows, [2])
        self.assertEqual(caught.exception.not_attempted, [])
        self.assertIn("timed out", str(caught.exception))

    def _export_stdout(self, stdout, exit_code):
        self._mode("stdout_then_exit")
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        os.environ["STUB_STDOUT"] = stdout
        os.environ["STUB_EXIT"] = str(exit_code)

    def test_an_export_lands_whole_and_returns_its_manifest(self):
        manifest = json.dumps(json.loads((GOLDEN_DIR / "prefix_pack_manifest.json").read_text()))
        pack = manifest + "\n{}\n"
        self._export_stdout(pack, 0)
        with tempfile.TemporaryDirectory() as out:
            path = os.path.join(out, "pack.ndjson")
            got = self.client.audit_export(path)
            self.assertEqual(got.pack_kind, "prefix")
            self.assertEqual(Path(path).read_text(), pack)
            self.assertEqual(os.listdir(out), ["pack.ndjson"])

    def test_a_failed_export_leaves_no_partial_pack(self):
        # Output the binary began before failing never reaches the path,
        # and a pack already there is left as it was.
        self._export_stdout('{"pack_format_version": 17}\n{"tree_size":', 1)
        with tempfile.TemporaryDirectory() as out:
            path = os.path.join(out, "pack.ndjson")
            Path(path).write_text("an earlier pack")
            with self.assertRaises(MorphologError):
                self.client.audit_export(path)
            self.assertEqual(Path(path).read_text(), "an earlier pack")
            self.assertEqual(os.listdir(out), ["pack.ndjson"])

    def test_a_timed_out_export_leaves_no_partial_pack(self):
        self._mode("hang")
        with tempfile.TemporaryDirectory() as out:
            with self.assertRaises(MorphologTimeout):
                self.client.audit_export(os.path.join(out, "pack.ndjson"), timeout=0.2)
            self.assertEqual(os.listdir(out), [])

    def test_a_batch_refused_before_its_first_row_ran_nothing(self):
        self._mode("stdout_then_exit")
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        os.environ["STUB_STDOUT"] = (
            '{"status": "error", "code": "not_committed", "error": "no connection"}\n'
        )
        os.environ["STUB_EXIT"] = "1"
        row = {"transformation": "t", "actor": "a", "args_named": {}}
        with self.assertRaises(MorphologRequestError) as caught:
            self.client.propose_batch([row, row])
        self.assertEqual(caught.exception.code, "not_committed")

    def test_a_batch_short_of_receipts_is_incomplete_even_at_exit_0(self):
        # A clean exit with a missing receipt is a broken promise, never a
        # short list; so is a last line the binary did not finish.
        self._mode("stdout_then_exit")
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        row = {"transformation": "t", "actor": "a", "args_named": {}}
        first = '{"row": 1, "status": "rejected", "reason": "closed"}\n'
        for stdout in (first, first + '{"row": 2, "status": "rej'):
            os.environ["STUB_STDOUT"] = stdout
            with self.assertRaises(MorphologBatchIncomplete) as caught:
                self.client.propose_batch([row, row])
            self.assertEqual(caught.exception.unknown_rows, [2])

    def test_a_timeout_never_keeps_the_password_it_captured(self):
        self._mode("echo_conninfo_then_hang")
        secret = "postgres://user:hunter2@db.internal/ledger"
        client = Morpholog("model.morph", secret, binary=str(self.stub), timeout=0.5)
        with self.assertRaises(MorphologTimeout) as caught:
            client.claims("Entry")
        self.assertNotIn("hunter2", caught.exception.stderr)
        self.assertIn("<redacted>", caught.exception.stderr)

    def test_an_empty_batch_succeeds_only_on_a_clean_silent_exit(self):
        self._mode("stdout_then_exit")
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        os.environ["STUB_STDOUT"] = ""
        os.environ["STUB_EXIT"] = "0"
        self.assertEqual(self.client.propose_batch([]), [])
        for stdout, exit_code in (("", "1"), ("garbage\n", "0")):
            os.environ["STUB_STDOUT"] = stdout
            os.environ["STUB_EXIT"] = exit_code
            with self.assertRaises(MorphologError, msg=f"{stdout!r} exit {exit_code}"):
                self.client.propose_batch([])
        self._mode("hang")
        with self.assertRaises(MorphologError):
            self.client.propose_batch([], timeout=0.2)

    def test_a_batch_row_with_an_unpublished_code_is_where_certainty_ends(self):
        self._mode("stdout_then_exit")
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        os.environ["STUB_STDOUT"] = (
            '{"row": 1, "status": "rejected", "reason": "closed"}\n'
            '{"row": 2, "status": "error", "code": "a_future_code", "error": "x"}\n'
        )
        row = {"transformation": "t", "actor": "a", "args_named": {}}
        with self.assertRaises(MorphologBatchIncomplete) as caught:
            self.client.propose_batch([row, row])
        self.assertEqual(caught.exception.unknown_rows, [2])
        # A receipt the client cannot trust says nothing about the rows
        # after it: the binary may have gone on, so none is claimed as
        # never run.
        os.environ["STUB_STDOUT"] = (
            '{"row": 1, "status": "rejected", "reason": "closed"}\n'
            '{"row": 2, "status": "error", "code": "a_future_code", "error": "x"}\n'
            '{"row": 3, "status": "committed", "asserted_claims": [], '
            '"retracted_claims": [], "emitted_intents": []}\n'
        )
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        for exit_code in ("0", "1"):
            # Even when the binary then exits non-zero, the untrusted row
            # came first, so the rows after it are unknown, not unrun.
            os.environ["STUB_EXIT"] = exit_code
            with self.assertRaises(MorphologBatchIncomplete, msg=exit_code) as drift:
                self.client.propose_batch([row, row, row, row])
            self.assertEqual(drift.exception.unknown_rows, [2, 3, 4], exit_code)
            self.assertEqual(drift.exception.not_attempted, [], exit_code)
        os.environ["STUB_EXIT"] = "0"
        # An explicit unknown receipt is still a receipt.
        os.environ["STUB_STDOUT"] = (
            '{"row": 1, "status": "error", "code": "commit_outcome_unknown", "error": "x"}\n'
        )
        receipts = self.client.propose_batch([row])
        self.assertEqual(receipts[0].outcome.code, "commit_outcome_unknown")

    def test_a_proposal_is_not_committed_only_when_the_binary_says_so(self):
        # The doctrine line: no exit code, stderr text, silence or signal
        # proves a non-commit. Only a strictly parsed error object with a
        # published code other than commit_outcome_unknown does.
        self._mode("not_committed_object")
        with self.assertRaises(MorphologRequestError) as said:
            self.client.propose("post", "alex", {})
        self.assertEqual(said.exception.code, "not_committed")
        self.assertNotIsInstance(said.exception, MorphologOutcomeUnknown)

        self._mode("killed")
        with self.assertRaises(MorphologOutcomeUnknown):
            self.client.propose("post", "alex", {})

        self._mode("stdout_then_exit")
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        silent = [
            ("", "0"),
            ("", "1"),
            ("", "2"),
            ("not json", "1"),
            ('{"status": "error", "code": "not_committed"', "1"),
            ('{"status": "error", "code": "a_future_code", "error": "x"}', "1"),
            ('{"status": "error", "error": "no code"}', "1"),
            ('{"status": "error", "code": "commit_outcome_unknown", "error": "x"}', "3"),
            ('{"status": "surprise"}', "0"),
        ]
        for stdout, exit_code in silent:
            os.environ["STUB_STDOUT"] = stdout
            os.environ["STUB_EXIT"] = exit_code
            for call in (
                lambda: self.client.propose("post", "alex", {}),
                lambda: self.client.transact([{"transformation": "t", "actor": "a"}]),
            ):
                with self.assertRaises(MorphologOutcomeUnknown, msg=f"{stdout!r} exit {exit_code}"):
                    call()

    def test_as_of_threads_through_both_claims_reads(self):
        # The flag lands on the CLI argv exactly when supplied, on both
        # reads, and is absent otherwise - all four cases, since the
        # issue asked for both surfaces.
        self._mode("record_argv")
        with recording_argv() as argv_after:

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

    def test_derived_reads_thread_as_of_and_named_exactly_when_supplied(self):
        # Same matrix as the claims reads: --as-of and --named each
        # land on argv exactly when asked for, after the positional
        # file + derived-claim name.
        self._mode("record_argv")
        with recording_argv() as argv_after:

            argv = argv_after(
                lambda: self.client.derived_named(
                    "TermsTimeline", as_of="2026-06-07T12:00:00Z"
                )
            )
            self.assertEqual(argv[:4], ["inspect", "derived", "model.morph", "TermsTimeline"])
            self.assertIn("--named", argv)
            self.assertEqual(argv[argv.index("--as-of") + 1], "2026-06-07T12:00:00Z")

            argv = argv_after(lambda: self.client.derived_named("TermsTimeline"))
            self.assertIn("--named", argv)
            self.assertNotIn("--as-of", argv)

            argv = argv_after(
                lambda: self.client.derived("TermsTimeline", as_of="2026-06-07T12:00:00Z")
            )
            self.assertNotIn("--named", argv)
            self.assertIn("--as-of", argv)

            argv = argv_after(lambda: self.client.derived("TermsTimeline"))
            self.assertEqual(argv[:4], ["inspect", "derived", "model.morph", "TermsTimeline"])
            self.assertNotIn("--named", argv)
            self.assertNotIn("--as-of", argv)

    def test_refresh_derived_parses_the_typed_report(self):
        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "refresh_derived_report.json").read_text()
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with recording_argv() as argv_after:
            report = None

            def call():
                nonlocal report
                report = self.client.refresh_derived()

            argv = argv_after(call)

        self.assertEqual(argv[:3], ["refresh", "derived", "model.morph"])
        self.assertEqual(report.derived_claim_count, 4)
        self.assertEqual(
            report.source_snapshot_transition_id,
            "01900000-0000-7000-8000-000000000001",
        )
        self.assertIsNotNone(report.source_snapshot_committed_at)

    def test_audit_flags_land_on_argv_exactly_when_supplied(self):
        # The four-case matrix for the audit tail: --after and --named
        # each appear exactly when asked for.
        self._mode("record_argv_empty")
        with recording_argv() as argv_after:

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

    def test_mentions_lands_on_the_audit_and_rejections_argv_and_an_empty_one_is_refused(self):
        self._mode("record_argv_empty")
        with recording_argv() as argv_after:
            argv = argv_after(lambda: self.client.audit(mentions="order_7"))
            self.assertEqual(argv[argv.index("--mentions") + 1], "order_7")
            argv = argv_after(lambda: self.client.audit_named(mentions="order_7"))
            self.assertIn("--mentions", argv)
            self.assertIn("--named", argv)
            argv = argv_after(lambda: self.client.audit())
            self.assertNotIn("--mentions", argv)
        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = "[]"
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with recording_argv() as argv_after:
            argv = argv_after(lambda: self.client.rejections(mentions="order_7", limit=5))
            self.assertEqual(argv[argv.index("--mentions") + 1], "order_7")
            self.assertEqual(argv[argv.index("--limit") + 1], "5")
        for call in (
            lambda: self.client.audit(mentions=""),
            lambda: self.client.rejections(mentions=""),
        ):
            with self.assertRaises(ValueError):
                call()

    def test_writer_roles_repeat_on_the_audit_and_checkpoint_argv(self):
        # The managed-Postgres assertion: one --writer-role pair per
        # role, on every watermark consumer, and absent when not asked.
        self._mode("record_argv_empty")
        with recording_argv() as argv_after:

            argv = argv_after(lambda: self.client.audit(writer_roles=["app_rw", "batch_rw"]))
            pairs = [
                (argv[i], argv[i + 1])
                for i in range(len(argv) - 1)
                if argv[i] == "--writer-role"
            ]
            self.assertEqual(pairs, [("--writer-role", "app_rw"), ("--writer-role", "batch_rw")])

            argv = argv_after(lambda: self.client.audit_named(writer_roles=["app_rw"]))
            self.assertIn("--writer-role", argv)
            self.assertIn("--named", argv)

            argv = argv_after(lambda: self.client.audit())
            self.assertNotIn("--writer-role", argv)

        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "checkpoint_created.json").read_text()
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with recording_argv() as argv_after:
            argv = argv_after(lambda: self.client.audit_checkpoint(writer_roles=["app_rw"]))
            self.assertEqual(argv[argv.index("--writer-role") + 1], "app_rw")

            argv = argv_after(lambda: self.client.audit_checkpoint())
            self.assertNotIn("--writer-role", argv)
            self.assertNotIn("--witness", argv)

            argv = argv_after(
                lambda: self.client.audit_checkpoint(
                    witnesses=["rfc3161:http://a.example/tsr", "rfc3161:http://b.example/tsr"]
                )
            )
            self.assertEqual(argv.count("--witness"), 2)
            self.assertEqual(argv[argv.index("--witness") + 1], "rfc3161:http://a.example/tsr")

            os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "checkpoint_witnessed.json").read_text()
            checkpoint = self.client.audit_witness(3, ["rfc3161:http://a.example/tsr"])
            self.assertIsInstance(checkpoint, envelopes.Checkpoint)
            argv = argv_after(
                lambda: self.client.audit_witness(3, ["rfc3161:http://a.example/tsr"])
            )
            self.assertEqual(argv[argv.index("--tree-size") + 1], "3")
            self.assertEqual(argv[argv.index("--witness") + 1], "rfc3161:http://a.example/tsr")
            with self.assertRaises(ValueError):
                self.client.audit_witness(3, [])

    def test_transact_returns_the_decision_and_raises_a_coded_error_with_retriable(self):
        self._mode("transact_stdout")
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        acts = [{"transformation": "open", "actor": "teller", "args_named": {"id": "a1"}}]

        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "transact_committed.json").read_text()
        committed = self.client.transact(acts)
        self.assertIsInstance(committed, envelopes.AtomicCommitted)

        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "transact_rejected.json").read_text()
        os.environ["STUB_EXIT"] = "1"
        rejected = self.client.transact(acts)
        self.assertIsInstance(rejected, envelopes.AtomicRejected)
        self.assertEqual(rejected.act, 2)

        # The error object is raised with its code; only a serialization
        # failure is retriable, and the predicate says so.
        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "transact_error.json").read_text()
        with self.assertRaises(MorphologRequestError) as coded:
            self.client.transact(acts)
        self.assertEqual(coded.exception.code, "serialization_failure")
        self.assertTrue(coded.exception.retriable)
        self.assertIsNone(coded.exception.row)
        os.environ["STUB_STDOUT"] = json.dumps(
            {"status": "error", "code": "not_committed", "error": "check constraint"}
        )
        with self.assertRaises(MorphologRequestError) as fixed_first:
            self.client.transact(acts)
        self.assertFalse(fixed_first.exception.retriable)

        # Exit 3 and a timeout are the unknown standing, never retriable.
        os.environ["STUB_STDOUT"] = ""
        os.environ["STUB_EXIT"] = "3"
        with self.assertRaises(MorphologOutcomeUnknown) as unknown:
            self.client.transact(acts)
        self.assertFalse(unknown.exception.retriable)
        self._mode("hang")
        with self.assertRaises(MorphologOutcomeUnknown):
            self.client.transact(acts, timeout=0.2)

    def test_a_binary_that_only_explains_on_stderr_leaves_the_outcome_unknown(self):
        # An older binary said "not committed" in prose only, and a usage
        # error says nothing about the database. Neither is a statement
        # the client can rely on, so both read as unknown; exit 3 always
        # did.
        for mode in (
            "not_committed_exit_1",
            "usage_error_exit_2",
            "commit_outcome_unknown_exit_3",
        ):
            self._mode(mode)
            with self.assertRaises(MorphologOutcomeUnknown, msg=mode) as unknown:
                self.client.propose("post", "alex", {})
            self.assertIn("read the record", str(unknown.exception))

    def test_verify_flags_land_on_argv_exactly_when_supplied(self):
        # The verdict-affecting verify flags: each appears exactly when
        # asked for, so the whole pinned verdict surface (signatures,
        # sealed views) is reachable through the blessed method.
        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "verify_report_consistent.json").read_text()
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with recording_argv() as argv_after:

            argv = argv_after(
                lambda: self.client.audit_verify(
                    anchor_file="head.json",
                    require_signatures=True,
                    views_schema="morpholog_views",
                )
            )
            self.assertEqual(argv[argv.index("--anchor-file") + 1], "head.json")
            self.assertIn("--require-signatures", argv)
            self.assertEqual(argv[argv.index("--views-schema") + 1], "morpholog_views")

            argv = argv_after(lambda: self.client.audit_verify())
            self.assertNotIn("--anchor-file", argv)
            self.assertNotIn("--require-signatures", argv)
            self.assertNotIn("--views-schema", argv)
            self.assertNotIn("--require-signatures-from", argv)
            self.assertNotIn("--require-signing-key", argv)

            # The policy refinements ride the same builder, on every
            # verify method: threshold and pin land exactly when asked.
            argv = argv_after(
                lambda: self.client.audit_verify(
                    require_signatures_from=1842, require_signing_key="honest.pub"
                )
            )
            self.assertEqual(argv[argv.index("--require-signatures-from") + 1], "1842")
            self.assertEqual(argv[argv.index("--require-signing-key") + 1], "honest.pub")
            self.assertNotIn("--require-signatures", argv)

            # All three pack-verify methods plumb require_signatures
            # into the shared argv builder; each is asserted on its own
            # method, replying with its own kind's signature-required
            # verdict golden.
            for method, verdict_golden in [
                (self.client.audit_verify_pack, "tree_verification_signature_required.json"),
                (
                    self.client.audit_verify_pack_window,
                    "window_verification_signature_required.json",
                ),
                (
                    self.client.audit_verify_pack_selective,
                    "selective_verification_signature_required.json",
                ),
            ]:
                os.environ["STUB_STDOUT"] = json.dumps(
                    {
                        "passes": False,
                        "verdict": json.loads((GOLDEN_DIR / verdict_golden).read_text()),
                        "role_rebindings": {"status": "not_evaluated"},
                    }
                )
                argv = argv_after(
                    lambda m=method: m("pack.json", require_signatures=True)
                )
                self.assertIn("--require-signatures", argv)

                argv = argv_after(lambda m=method: m("pack.json"))
                self.assertNotIn("--require-signatures", argv)
                self.assertNotIn("--require-signatures-from", argv)
                self.assertNotIn("--require-signing-key", argv)

                argv = argv_after(
                    lambda m=method: m(
                        "pack.json", require_signatures_from=7, require_signing_key="k.pub"
                    )
                )
                self.assertEqual(argv[argv.index("--require-signatures-from") + 1], "7")
                self.assertEqual(argv[argv.index("--require-signing-key") + 1], "k.pub")

    def test_the_witness_axis_flags_and_the_wrapper_report(self):
        # Trust anchors land on the live verify. The pack verifiers always
        # reply with the wrapper report; asking for witnesses (or
        # supplying anchors) adds them, and each kind's parser still reads
        # the verdict inside it.
        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "verify_report_witnessed.json").read_text()
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with recording_argv() as argv_after:
            argv = argv_after(lambda: self.client.audit_verify(trusted_tsa_file="tsa.pem"))
            self.assertEqual(argv[argv.index("--trusted-tsa-file") + 1], "tsa.pem")
            argv = argv_after(lambda: self.client.audit_verify())
            self.assertNotIn("--trusted-tsa-file", argv)

            os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "pack_verification_report.json").read_text()
            report = self.client.audit_verify_pack_window("pack.json", witnesses=True)
            self.assertIsInstance(report, envelopes.PackVerificationReport)
            self.assertIsInstance(report.verdict, envelopes.WindowIntact)
            self.assertEqual(report.witnesses.checkpoints[0].tree_size, 2)
            argv = argv_after(
                lambda: self.client.audit_verify_pack_window("pack.json", witnesses=True)
            )
            self.assertIn("--witnesses", argv)
            self.assertNotIn("--trusted-tsa-file", argv)
            argv = argv_after(
                lambda: self.client.audit_verify_pack_window(
                    "pack.json", trusted_tsa_file="tsa.pem"
                )
            )
            self.assertNotIn("--witnesses", argv)
            self.assertEqual(argv[argv.index("--trusted-tsa-file") + 1], "tsa.pem")

            os.environ["STUB_STDOUT"] = (
                GOLDEN_DIR / "pack_verification_report_selective_rebinding.json"
            ).read_text()
            report = self.client.audit_verify_pack_selective("pack.json")
            self.assertIsInstance(report.verdict, envelopes.SelectiveIntact)
            self.assertIsNone(report.witnesses)
            rebindings = report.role_rebindings
            self.assertIsInstance(rebindings, envelopes.RoleRebindingsEvaluated)
            self.assertEqual(rebindings.scope, "selective")
            change = rebindings.changes[0]
            self.assertEqual((change.role, change.previous_oid, change.new_oid),
                             ("gm_human", 16384, 16391))

    def test_a_receipt_is_written_as_issued_and_a_refused_one_is_a_report(self):
        self._mode("record_argv_stdout")
        os.environ["STUB_STDOUT"] = (GOLDEN_DIR / "evaluation_receipt.json").read_text()
        self.addCleanup(os.environ.pop, "STUB_STDOUT", None)
        with tempfile.TemporaryDirectory() as tmp, recording_argv() as argv_after:
            path = os.path.join(tmp, "receipt.json")
            argv = argv_after(
                lambda: self.client.audit_receipt("FacilityUtilisation", "pack.ndjson", path)
            )
            self.assertEqual(
                argv,
                [
                    "audit",
                    "receipt",
                    "model.morph",
                    "--pack",
                    "pack.ndjson",
                    "--derived",
                    "FacilityUtilisation",
                ],
            )
            with open(path, encoding="utf-8") as written:
                self.assertEqual(written.read(), os.environ["STUB_STDOUT"] + "\n")
            receipt = self.client.audit_receipt("FacilityUtilisation", "pack.ndjson", path)
            self.assertEqual(receipt.checkpoint.tree_size, 2)

            os.environ["STUB_STDOUT"] = (
                GOLDEN_DIR / "receipt_verification_report_reproduced.json"
            ).read_text()
            argv = argv_after(
                lambda: self.client.audit_verify_receipt(
                    "receipt.json", "pack.ndjson", anchor_file="cp.json", witnesses=True
                )
            )
            self.assertEqual(
                argv[:7],
                [
                    "audit",
                    "verify-receipt",
                    "model.morph",
                    "--receipt",
                    "receipt.json",
                    "--pack",
                    "pack.ndjson",
                ],
            )
            self.assertEqual(argv[argv.index("--anchor-file") + 1], "cp.json")
            self.assertIn("--witnesses", argv)

        # Refused at exit 1, the report is still the decided result.
        self._mode("stdout_then_exit")
        os.environ["STUB_STDOUT"] = (
            GOLDEN_DIR / "receipt_verification_report_mismatched.json"
        ).read_text()
        os.environ["STUB_EXIT"] = "1"
        self.addCleanup(os.environ.pop, "STUB_EXIT", None)
        report = self.client.audit_verify_receipt("receipt.json", "pack.ndjson")
        self.assertIsInstance(report.program, envelopes.ProgramDiffers)
        self.assertEqual(report.evaluation, "not_evaluated")

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

    def test_a_propose_timeout_is_outcome_unknown_and_a_read_timeout_is_not(self):
        # The kill can land after COMMIT was sent, so a timed-out
        # proposal is exactly as unknown as exit 3; a timed-out read
        # changed nothing and stays an ordinary operational error.
        self._mode("hang")
        bounded = Morpholog(
            "model.morph", "postgres:///stub", binary=str(self.stub), timeout=0.2
        )
        with self.assertRaises(MorphologOutcomeUnknown) as unknown:
            bounded.propose("post", "alex", {})
        self.assertIn("read the record", str(unknown.exception))
        with self.assertRaises(MorphologError) as read:
            bounded.claims("Entry")
        self.assertIsInstance(read.exception, MorphologTimeout)
        self.assertNotIsInstance(read.exception, MorphologOutcomeUnknown)

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

    def test_a_timeout_message_redacts_the_database_url(self):
        # Glasshouse's forcing case: the timeout message is structured-
        # logged and may be reflected to a caller, so the conninfo (and
        # its password) must never ride in it.
        self._mode("hang")
        secret = "postgres://user:hunter2@db.internal/ledger"
        bounded = Morpholog("model.morph", secret, binary=str(self.stub), timeout=0.2)
        with self.assertRaises(MorphologError) as caught:
            bounded.audit_verify()
        msg = str(caught.exception)
        self.assertNotIn("hunter2", msg)
        self.assertNotIn(secret, msg)
        self.assertIn("--database-url <redacted>", msg)

    def test_an_operational_failure_message_redacts_the_database_url(self):
        # The other raised message (empty stdout) masks the argv too.
        self._mode("operational_failure")
        secret = "postgres://user:hunter2@db.internal/ledger"
        client = Morpholog("model.morph", secret, binary=str(self.stub))
        with self.assertRaises(MorphologError) as caught:
            client.audit_verify()
        msg = str(caught.exception)
        self.assertNotIn("hunter2", msg)
        self.assertIn("--database-url <redacted>", msg)

    def test_stderr_echoing_the_conninfo_is_masked(self):
        # A PG driver error can echo the connection string itself; the
        # client masks its own database_url in any stderr it surfaces.
        self._mode("stderr_echoes_conninfo")
        secret = "postgres://user:hunter2@db.internal/ledger"
        client = Morpholog("model.morph", secret, binary=str(self.stub))
        with self.assertRaises(MorphologError) as caught:
            client.audit_verify()
        msg = str(caught.exception)
        self.assertNotIn("hunter2", msg)
        self.assertNotIn(secret, msg)
        self.assertIn("<redacted>", msg)

    def test_batch_stderr_echoing_the_conninfo_is_masked(self):
        # propose_batch bypasses _invoke and raises with raw stderr; it
        # must mask the conninfo on the abort path too.
        self._mode("stderr_echoes_conninfo")
        secret = "postgres://user:hunter2@db.internal/ledger"
        client = Morpholog("model.morph", secret, binary=str(self.stub))
        with self.assertRaises(MorphologError) as caught:
            client.propose_batch([{"transformation": "t", "actor": "a", "args_named": {}}])
        msg = str(caught.exception)
        self.assertNotIn("hunter2", msg)
        self.assertNotIn(secret, msg)

    def test_audit_stderr_echoing_the_conninfo_is_masked(self):
        # _audit_lines also bypasses _invoke (an empty tail is lawful).
        self._mode("stderr_echoes_conninfo")
        secret = "postgres://user:hunter2@db.internal/ledger"
        client = Morpholog("model.morph", secret, binary=str(self.stub))
        with self.assertRaises(MorphologError) as caught:
            client.audit()
        msg = str(caught.exception)
        self.assertNotIn("hunter2", msg)
        self.assertNotIn(secret, msg)

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
        with recording_argv() as argv_after:

            # No rows, so no receipts is the complete answer.
            rows: list[dict[str, object]] = []
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

    def test_submit_all_is_transact_over_one_row_per_request_and_actor(self):
        class Grant:
            TRANSFORMATION = "grant"

            def to_args_named(self):
                return {"principal": "desk"}

        class Capture:
            TRANSFORMATION = "capture"

            def to_args_named(self):
                return {"trade": "t1"}

        with mock.patch.object(self.client, "transact", return_value="decision") as transact:
            decision = self.client.submit_all(
                [(Grant(), "desk"), (Capture(), "trader")], timeout=5
            )
        self.assertEqual(decision, "decision")
        transact.assert_called_once_with(
            [
                {"transformation": "grant", "actor": "desk", "args_named": {"principal": "desk"}},
                {"transformation": "capture", "actor": "trader", "args_named": {"trade": "t1"}},
            ],
            5,
        )

    def test_checkpoint_signing_key_and_key_id_must_be_given_together(self):
        # The guard raises before any subprocess, so the stub never runs.
        with self.assertRaises(ValueError):
            self.client.audit_checkpoint(signing_key="k.pem")
        with self.assertRaises(ValueError):
            self.client.audit_checkpoint(key_id="k1")


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
