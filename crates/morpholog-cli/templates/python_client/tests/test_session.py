"""The Session seam, pinned against a scripted stub binary and the
SAME golden transcript the Rust session test records against a real
database: the requests this client must emit byte-identically, the
responses it must parse, and the lifecycle rules - a broken lockstep
poisons the whole session, and a lost response to a submitted propose
is an UNKNOWN outcome, never a plain error."""

import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

from _support import GOLDEN_DIR, add_client_to_path

add_client_to_path()

from python_client import envelopes
from python_client.session import (
    MorphologOutcomeUnknown,
    MorphologRequestError,
    Session,
)
from python_client.adapter import MorphologError

TRANSCRIPT = GOLDEN_DIR.parent / "session" / "transcript.ndjson"

# A scripted stand-in for `morpholog session`, mode-selected by
# SESSION_STUB_MODE. The transcript mode replays the pinned golden:
# ready first, then the pinned response for each request received
# (recording what arrived, so the emit half can be asserted).
_STUB = r'''
import json, os, sys, time

mode = os.environ.get("SESSION_STUB_MODE", "transcript")
record = os.environ.get("SESSION_STUB_RECORD")


def note(line):
    if record:
        with open(record, "a", encoding="utf-8") as f:
            f.write(line.rstrip("\n") + "\n")


def say(line):
    sys.stdout.write(line.rstrip("\n") + "\n")
    sys.stdout.flush()


READY = json.dumps(
    {
        "model_hash": "sha256:" + "1" * 64,
        "morpholog_version": "0.0.0",
        "program": "stub",
        "protocol": 2 if mode == "bad_protocol" else 1,
        "status": "ready",
    },
    separators=(",", ":"),
    sort_keys=True,
)

note("ARGV " + json.dumps(sys.argv))
note("HAS_DATABASE_URL " + str("DATABASE_URL" in os.environ))

if mode == "no_ready":
    sys.stderr.write("cannot connect: " + os.environ.get("DATABASE_URL", "") + "\n")
    sys.exit(3)
if mode == "exit_after_ready":
    say(READY)
    sys.exit(0)
if mode == "never_ready":
    time.sleep(30)
    sys.exit(3)

if mode == "transcript":
    lines = open(os.environ["SESSION_TRANSCRIPT"], encoding="utf-8").read().splitlines()
    say(lines[0])
    responses = lines[2::2]
    i = 0
    for request in sys.stdin:
        note(request)
        say(responses[i])
        i += 1
    sys.exit(0)

say(READY)
committed = json.dumps(
    {
        "actor": {"type": "subject", "value": "café"},
        "asserted_claims": [],
        "emitted_intents": [],
        "retracted_claims": [],
        "row": 99 if mode == "wrong_row" else 1,
        "status": "committed",
        "transition_id": "00000000-0000-0000-0000-000000000000",
    },
    separators=(",", ":"),
    sort_keys=True,
)
n = 0
for request in sys.stdin:
    n += 1
    note(request)
    if mode == "die_after_ready":
        sys.exit(4)
    if mode == "hang_after_ready":
        time.sleep(30)
    if mode == "garbage_after_ready":
        say("this is not json")
    elif mode == "uncoded_error":
        say(json.dumps({"error": "prose only", "row": n, "status": "error"}))
    elif mode == "wrong_row_error":
        say(json.dumps({"code": "kernel_error", "error": "x", "row": 99, "status": "error"}))
    elif mode == "bogus_rows":
        say(json.dumps([{"bogus": True}]))
    elif mode == "binary_garbage":
        sys.stdout.buffer.write(b"\xff\xfe\n")
        sys.stdout.buffer.flush()
    elif mode == "stderr_flood":
        sys.stderr.write("x" * 1000000)
        sys.stderr.flush()
        say("[]")
    elif mode == "slow_echo":
        time.sleep(0.2)
        say("[]")
    else:
        say(committed)
if mode == "ignore_eof":
    time.sleep(60)
'''


class SessionHarness(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        stub = Path(self._dir.name) / "stub.py"
        stub.write_text(_STUB, encoding="utf-8")
        launcher = Path(self._dir.name) / "morpholog-stub"
        launcher.write_text(f"#!{sys.executable}\n{_STUB}", encoding="utf-8")
        launcher.chmod(0o755)
        self.binary = str(launcher)
        self.record = Path(self._dir.name) / "requests.log"

    def env(self, mode, **extra):
        os.environ["SESSION_STUB_MODE"] = mode
        os.environ["SESSION_STUB_RECORD"] = str(self.record)
        os.environ["SESSION_TRANSCRIPT"] = str(TRANSCRIPT)
        for key, value in extra.items():
            os.environ[key] = value
        self.addCleanup(os.environ.pop, "SESSION_STUB_MODE", None)
        self.addCleanup(os.environ.pop, "SESSION_STUB_RECORD", None)
        self.addCleanup(os.environ.pop, "SESSION_TRANSCRIPT", None)

    def session(self, mode="transcript", timeout=10.0, **kwargs):
        self.env(mode)
        return Session(
            "rules.morph",
            "postgres://user:secret@db/prod",
            binary=self.binary,
            timeout=timeout,
            **kwargs,
        )

    def recorded(self):
        lines = self.record.read_text(encoding="utf-8").splitlines()
        return [l for l in lines if not l.startswith(("ARGV ", "HAS_DATABASE_URL "))]

    def stub_meta(self):
        lines = self.record.read_text(encoding="utf-8").splitlines()
        argv = json.loads(next(l for l in lines if l.startswith("ARGV "))[5:])
        has_url = next(l for l in lines if l.startswith("HAS_DATABASE_URL "))
        return argv, has_url.endswith("True")


class TranscriptConversation(SessionHarness):
    def test_the_pinned_conversation_end_to_end(self):
        """Every request the transcript pins is emitted byte-identically,
        and every pinned response parses to its typed outcome."""
        transcript = TRANSCRIPT.read_text(encoding="utf-8").splitlines()
        pinned_requests = transcript[1::2]
        pinned_ready = json.loads(transcript[0])
        with self.session() as s:
            self.assertEqual(s.model_hash, pinned_ready["model_hash"])
            one = s.propose(
                "open_account", "teller", {"account": "acct_1", "opened_on": "2026-01-15"}
            )
            self.assertIsInstance(one, envelopes.Committed)
            two = s.propose(
                "post_balance", "teller", {"account": "acct_1", "figure": "100"}
            )
            self.assertIsInstance(two, envelopes.Committed)
            three = s.propose(
                "post_balance",
                "teller",
                {"account": "ghost", "figure": "5"},
                explain_on_reject=True,
            )
            self.assertIsInstance(three, envelopes.Rejected)
            self.assertEqual(three.rule, "balances_name_an_account")
            self.assertIsNotNone(three.explanation)
            accounts = s.claims_named("Account")
            self.assertEqual(accounts[0].args["account"], "acct_1")
            balances = s.claims_named("Balance", where={"figure": "100"})
            self.assertEqual(len(balances), 1)
            totals = s.derived_named("BookTotal")
            self.assertEqual(totals[0].args["total"], "100")
            with self.assertRaises(MorphologRequestError) as refused:
                s.propose("no_such_act", "teller", {})
            self.assertEqual(refused.exception.code, "unknown_transformation")
            with self.assertRaises(MorphologRequestError) as bad_as_of:
                s.claims(as_of="not-a-coordinate")
            self.assertEqual(bad_as_of.exception.code, "invalid_arguments")
            # Request errors do not poison: the session still answers.
            self.assertFalse(s._poisoned)
        self.assertEqual(self.recorded(), pinned_requests)

    def test_a_request_error_is_not_a_poisoned_session(self):
        with self.session() as s:
            s.propose("open_account", "teller", {"account": "acct_1", "opened_on": "2026-01-15"})
            self.assertIsNone(s._poisoned)


class Lifecycle(SessionHarness):
    def test_a_timeout_poisons_and_is_outcome_unknown_for_a_propose(self):
        s = self.session(mode="hang_after_ready", timeout=0.3)
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("t", "a", {})
        # A late line must never answer a newer request: the session
        # refuses everything after the break.
        with self.assertRaises(MorphologError):
            s.claims()
        self.assertIsNotNone(s._child.poll())

    def test_process_death_after_a_propose_is_outcome_unknown(self):
        s = self.session(mode="die_after_ready")
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("t", "a", {})

    def test_process_death_on_a_read_is_plain_operational(self):
        s = self.session(mode="die_after_ready")
        with self.assertRaises(MorphologError) as err:
            s.claims()
        self.assertNotIsInstance(err.exception, MorphologOutcomeUnknown)

    def test_malformed_stdout_poisons(self):
        s = self.session(mode="garbage_after_ready")
        with self.assertRaises(MorphologError):
            s.claims()
        with self.assertRaises(MorphologError):
            s.claims()

    def test_a_receipt_for_the_wrong_row_is_outcome_unknown(self):
        s = self.session(mode="wrong_row")
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("t", "a", {})

    def test_a_wrong_row_error_receipt_is_also_outcome_unknown(self):
        # The coded receipt cannot be associated with the submitted
        # proposal once the row disagrees; commitful means unknown.
        s = self.session(mode="wrong_row_error")
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("t", "a", {})
        self.assertIsNotNone(s._poisoned)

    def test_a_failed_write_after_child_exit_is_outcome_unknown(self):
        # The child exits right after ready; once it is gone, writing
        # the request fails - but a failed write does not prove zero
        # bytes crossed, so a propose is unknown, never plain error.
        s = self.session(mode="exit_after_ready")
        s._child.wait(timeout=10)
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("t", "a", {})

    def test_a_failed_write_on_a_read_stays_plain_operational(self):
        s = self.session(mode="exit_after_ready")
        s._child.wait(timeout=10)
        with self.assertRaises(MorphologError) as err:
            s.claims()
        self.assertNotIsInstance(err.exception, MorphologOutcomeUnknown)

    def test_undecodable_stdout_fails_promptly_not_a_hang(self):
        # Invalid UTF-8 under the strict decoder kills the drain
        # thread's iterator; without the sentinel guard the caller
        # would block for the whole timeout (or forever without one).
        import time

        s = self.session(mode="binary_garbage", timeout=30.0)
        t0 = time.monotonic()
        with self.assertRaises(MorphologError):
            s.claims()
        self.assertLess(time.monotonic() - t0, 10.0, "must fail via the sentinel, not the timeout")
        self.assertIsNotNone(s._poisoned)

    def test_a_malformed_row_inside_a_valid_array_poisons(self):
        # The framing is intact, but a binary/client contract mismatch
        # will not heal on the next call.
        s = self.session(mode="bogus_rows")
        with self.assertRaises(MorphologError):
            s.claims()
        self.assertIsNotNone(s._poisoned)

    def test_an_uncoded_error_receipt_is_drift_and_poisons(self):
        s = self.session(mode="uncoded_error")
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("t", "a", {})
        self.assertIsNotNone(s._poisoned)

    def test_concurrent_callers_are_serialised_not_interleaved(self):
        with self.session(mode="slow_echo") as s:
            results, errors = [], []

            def read():
                try:
                    results.append(s.claims())
                except Exception as exc:  # noqa: BLE001 - the test collects
                    errors.append(exc)

            threads = [threading.Thread(target=read) for _ in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            self.assertEqual(errors, [])
            self.assertEqual(results, [[], [], [], []])

    def test_close_sends_eof_and_reaps_the_child(self):
        s = self.session()
        s.close()
        self.assertEqual(s._child.poll(), 0)
        with self.assertRaises(MorphologError):
            s.claims()

    def test_a_child_that_ignores_eof_is_terminated(self):
        s = self.session(mode="ignore_eof")
        s.close()
        self.assertIsNotNone(s._child.poll())

    def test_stderr_flooding_cannot_deadlock_the_conversation(self):
        with self.session(mode="stderr_flood", timeout=30.0) as s:
            self.assertEqual(s.claims(), [])


class Startup(SessionHarness):
    def test_startup_failure_surfaces_redacted_stderr(self):
        self.env("no_ready")
        with self.assertRaises(MorphologError) as err:
            Session("rules.morph", "postgres://user:secret@db/prod", binary=self.binary)
        self.assertNotIn("secret", str(err.exception))
        self.assertIn("<redacted>", str(err.exception))

    def test_startup_timeout_is_operational(self):
        self.env("never_ready")
        with self.assertRaises(MorphologError):
            Session(
                "rules.morph",
                "postgres://u:p@db/x",
                binary=self.binary,
                timeout=0.3,
            )

    def test_a_protocol_this_client_does_not_speak_refuses(self):
        self.env("bad_protocol")
        with self.assertRaises(MorphologError) as err:
            Session("rules.morph", "postgres://u:p@db/x", binary=self.binary)
        self.assertIn("protocol", str(err.exception))

    def test_an_unexpected_model_hash_refuses_to_open(self):
        with self.assertRaises(MorphologError) as err:
            self.session(expected_model_hash="sha256:" + "f" * 64)
        self.assertIn("expected", str(err.exception))

    def test_the_connection_string_travels_in_env_not_argv(self):
        with self.session(mode="slow_echo") as s:
            s.claims()
        argv, has_url = self.stub_meta()
        self.assertTrue(has_url, "DATABASE_URL must reach the child's environment")
        for arg in argv:
            self.assertNotIn("secret", arg)

    def test_a_missing_binary_is_a_clean_operational_error(self):
        with self.assertRaises(MorphologError):
            Session("rules.morph", "postgres://u:p@db/x", binary="/nonexistent/morpholog")


class Encoding(SessionHarness):
    def test_non_ascii_values_round_trip_under_utf8(self):
        with self.session(mode="echo_committed") as s:
            outcome = s.propose("t", "café", {"note": "élève"})
            self.assertEqual(outcome.actor, "café")
        recorded = self.recorded()
        self.assertIn("élève", recorded[0])


if __name__ == "__main__":
    unittest.main()
