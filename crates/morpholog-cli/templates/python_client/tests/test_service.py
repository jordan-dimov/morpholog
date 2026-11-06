"""``Session.connect`` against a scripted stand-in for ``morpholog serve``:
a Unix socket served from a thread. The exchange is the session's own, so
what is pinned here is what the socket changes - how a connection ends,
what a lost, late or out-of-step response means over it, and that a call
sends its request once and never again.

The failure modelled is the transport's: a service that hangs up before
or after it answers, answers late, or answers out of step. What a hostile
caller can do to the service is the service's own tests' concern."""

import json
import os
import socket
import tempfile
import threading
import time
import unittest

from _support import GOLDEN_DIR

from python_client import envelopes
from python_client.adapter import MorphologError
from python_client.session import (
    _READER,
    MorphologOutcomeUnknown,
    MorphologRequestError,
    Session,
)

TRANSCRIPT = (GOLDEN_DIR.parent / "session" / "transcript.ndjson").read_text(
    encoding="utf-8"
).splitlines()
READY = json.loads(TRANSCRIPT[0])
OPEN = {"account": "acct_1", "opened_on": "2026-01-15"}


def wire(value: object) -> str:
    return json.dumps(value, separators=(",", ":"), sort_keys=True)


def committed(row: int) -> str:
    return wire(
        {
            "actor": {"type": "subject", "value": "teller"},
            "asserted_claims": [],
            "emitted_intents": [],
            "retracted_claims": [],
            "row": row,
            "status": "committed",
            "transition_id": "00000000-0000-0000-0000-000000000000",
        }
    )


def refused(row: int, code: str, error: str) -> str:
    return wire({"code": code, "error": error, "row": row, "status": "error"})


class Conversation:
    """One accepted connection, as the script sees it."""

    def __init__(self, service: "FakeService", conn: socket.socket) -> None:
        self.service = service
        self.conn = conn
        self._stream = conn.makefile("rb")

    def say(self, line: str) -> None:
        self.conn.sendall(line.encode("utf-8") + b"\n")

    def ready(self, **changes: object) -> None:
        self.say(wire({**READY, **changes}))

    def request(self) -> dict | None:
        """The next request, recorded; ``None`` once the caller hung up."""
        line = self._stream.readline()
        if not line:
            return None
        text = line.decode("utf-8").rstrip("\n")
        self.service.received.append(text)
        return json.loads(text)

    def close(self) -> None:
        self._stream.close()
        self.conn.close()


class FakeService:
    """Accepts connections on a socket in a private directory and runs
    ``script(conversation)`` for each one, on its own thread."""

    def __init__(self, test: unittest.TestCase, script) -> None:
        directory = tempfile.TemporaryDirectory()
        test.addCleanup(directory.cleanup)
        self.path = os.path.join(directory.name, "serve.sock")
        self.received: list[str] = []
        self.ledger: list[dict] = []
        self.hung_up = threading.Event()
        self._script = script
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(self.path)
        self._listener.listen()
        threading.Thread(target=self._accept, daemon=True).start()
        test.addCleanup(self._listener.close)

    def _accept(self) -> None:
        while True:
            try:
                conn, _ = self._listener.accept()
            except OSError:
                return
            threading.Thread(target=self._run, args=(conn,), daemon=True).start()

    def _run(self, conn: socket.socket) -> None:
        conversation = Conversation(self, conn)
        try:
            self._script(conversation)
        except OSError:
            pass
        finally:
            conversation.close()


def readers_alive() -> int:
    return sum(1 for thread in threading.enumerate() if thread.name == _READER)


class ServiceHarness(unittest.TestCase):
    def serve(self, script) -> FakeService:
        return FakeService(self, script)

    def connect(self, service: FakeService, timeout: float = 10.0, **pins) -> Session:
        session = Session.connect(service.path, timeout=timeout, **pins)
        self.addCleanup(session.close)
        return session

    def assert_readers_end(self) -> None:
        deadline = time.monotonic() + 5
        while readers_alive() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertEqual(readers_alive(), 0, "a connection's reader thread outlived it")


class TheWire(ServiceHarness):
    def test_the_requests_are_the_pinned_ones_and_the_receipts_parse(self):
        """The socket carries the session's own wire: the first two
        pinned exchanges go out byte-identically and parse to their
        typed outcomes."""

        def transcript(c: Conversation) -> None:
            c.say(TRANSCRIPT[0])
            for response in TRANSCRIPT[2::2]:
                if c.request() is None:
                    return
                c.say(response)

        service = self.serve(transcript)
        with Session.connect(service.path, timeout=10.0) as s:
            self.assertEqual(s.model_hash, READY["model_hash"])
            one = s.propose("open_account", "teller", OPEN)
            two = s.propose("post_balance", "teller", {"account": "acct_1", "figure": "100"})
        self.assertIsInstance(one, envelopes.Committed)
        self.assertIsInstance(two, envelopes.Committed)
        self.assertEqual(service.received, TRANSCRIPT[1::2][:2])
        self.assert_readers_end()

    def test_a_refused_read_leaves_the_connection_usable(self):
        """A read the service did not grant is answered in step, so the
        connection carries on."""

        def ungranted(c: Conversation) -> None:
            c.ready()
            c.request()
            c.say(
                refused(
                    1,
                    "unknown_operation",
                    "`claims` is not granted to this caller; it may propose, "
                    "transact and ping",
                )
            )
            c.request()
            c.ready()

        s = self.connect(self.serve(ungranted))
        with self.assertRaises(MorphologRequestError) as err:
            s.claims()
        self.assertEqual(err.exception.code, "unknown_operation")
        self.assertEqual(s.ping(), s.ready)
        self.assertIsNone(s._poisoned)


class Opening(ServiceHarness):
    def test_a_handshake_it_does_not_pin_closes_the_connection(self):
        """Each refusal at the handshake hangs up on the service and ends
        the reader, so a refused connection holds nothing open."""
        cases = [
            ("version", {"morpholog_version": "9.9.9"}, {"expected_version": "0.0.0"}),
            ("protocol", {"protocol": 2}, {}),
            ("model", {}, {"expected_model_hash": "sha256:" + "2" * 64}),
        ]
        for name, changes, pins in cases:
            with self.subTest(name):

                def handshake(c: Conversation, changes=changes) -> None:
                    c.ready(**changes)
                    if c.request() is None:
                        c.service.hung_up.set()

                service = self.serve(handshake)
                with self.assertRaises(MorphologError):
                    Session.connect(service.path, timeout=10.0, **pins)
                self.assertTrue(service.hung_up.wait(5), "the connection was left open")
                self.assert_readers_end()

    def test_a_service_that_hangs_up_before_ready_is_reported_not_diagnosed(self):
        """The service closes before its ready line on an OS user it does
        not map, but a crash looks the same: the message names the
        possible causes and claims none of them."""
        service = self.serve(lambda c: None)
        started = time.monotonic()
        with self.assertRaises(MorphologError) as err:
            Session.connect(service.path, timeout=10.0)
        self.assertLess(time.monotonic() - started, 5, "it waited out the timeout")
        message = str(err.exception)
        self.assertIn("closed the connection before it was ready", message)
        self.assertIn("presenters", message)
        self.assertIn("looks the same", message)
        self.assert_readers_end()

    def test_an_unreachable_socket_names_its_path(self):
        missing = os.path.join(tempfile.gettempdir(), "no-such-morpholog.sock")
        with self.assertRaises(MorphologError) as err:
            Session.connect(missing, timeout=1.0)
        self.assertIn(missing, str(err.exception))
        self.assertNotIsInstance(err.exception, MorphologOutcomeUnknown)


class LostResponses(ServiceHarness):
    def test_a_commit_whose_receipt_is_lost_is_unknown_and_the_record_has_it(self):
        """The service commits and hangs up before the receipt: the caller
        is told the outcome is unknown, the connection refuses from then
        on, and a fresh connection reads the commit. What the caller knows
        about the response is not what happened in the database."""

        def commits_then_hangs_up(c: Conversation) -> None:
            c.ready()
            request = c.request()
            if request is None:
                return
            if request["op"] == "propose":
                c.service.ledger.append(request["args_named"])
                return
            c.say(
                wire(
                    [
                        {
                            "args": args,
                            "asserted_in": "00000000-0000-0000-0000-000000000000",
                            "predicate": "Account",
                        }
                        for args in c.service.ledger
                    ]
                )
            )

        service = self.serve(commits_then_hangs_up)
        s = self.connect(service)
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("open_account", "teller", OPEN)
        with self.assertRaises(MorphologError) as unusable:
            s.ping()
        self.assertIn("unusable", str(unusable.exception))
        fresh = self.connect(service)
        accounts = fresh.claims_named("Account")
        self.assertEqual([row.args["account"] for row in accounts], ["acct_1"])
        self.assertEqual(len(service.received), 2, "only the propose and the read went out")

    def test_a_late_receipt_is_never_the_next_answer(self):
        """A receipt that arrives after the timeout belongs to a request
        the caller has given up on. The proposal is unknown, and the
        connection takes no further request, so the late line can never
        be read as another call's answer."""

        def answers_late(c: Conversation) -> None:
            c.ready()
            if c.request() is None:
                return
            time.sleep(0.5)
            # The caller has hung up by now, so the late line may not
            # even be deliverable.
            try:
                c.say(committed(1))
                c.request()
            except OSError:
                pass
            c.service.hung_up.set()

        service = self.serve(answers_late)
        s = self.connect(service, timeout=0.2)
        with self.assertRaises(MorphologOutcomeUnknown) as err:
            s.propose("open_account", "teller", OPEN)
        self.assertIn("no response arrived", str(err.exception))
        with self.assertRaises(MorphologError) as unusable:
            s.ping()
        self.assertIn("unusable", str(unusable.exception))
        self.assertTrue(service.hung_up.wait(5))
        self.assertEqual(len(service.received), 1, "the ping must not have gone out")

    def test_a_late_read_is_an_ordinary_error_and_still_poisons(self):
        """A read commits nothing, so its lost answer is a plain error,
        but the lockstep is broken all the same."""

        def answers_late(c: Conversation) -> None:
            c.ready()
            if c.request() is None:
                return
            time.sleep(0.5)
            c.say("[]")

        s = self.connect(self.serve(answers_late), timeout=0.2)
        with self.assertRaises(MorphologError) as err:
            s.claims()
        self.assertNotIsInstance(err.exception, MorphologOutcomeUnknown)
        self.assertIsNotNone(s._poisoned)

    def test_an_answer_to_another_row_is_unknown_and_poisons(self):
        def out_of_step(c: Conversation) -> None:
            c.ready()
            if c.request() is not None:
                c.say(committed(99))

        s = self.connect(self.serve(out_of_step))
        with self.assertRaises(MorphologOutcomeUnknown):
            s.propose("open_account", "teller", OPEN)
        self.assertIsNotNone(s._poisoned)

    def test_a_write_cut_off_part_way_is_unknown(self):
        """Some of the request crossed before the service hung up, so a
        failed write proves nothing about whether it was received."""

        def reads_a_little(c: Conversation) -> None:
            c.ready()
            c.conn.recv(1000)

        s = self.connect(self.serve(reads_a_little))
        with self.assertRaises(MorphologOutcomeUnknown) as err:
            s.propose("open_account", "teller", {"account": "x" * (16 << 20)})
        self.assertIn("before the write failed", str(err.exception))
        self.assertIsNotNone(s._poisoned)


class NoRetry(ServiceHarness):
    def test_a_known_non_commit_is_reported_once_and_not_resent(self):
        """A serialisation failure is safe to re-submit, but that is the
        caller's decision: the client sends the request once, reports the
        refusal, and stays in step."""

        def conflicts(c: Conversation) -> None:
            c.ready()
            c.request()
            c.say(refused(1, "serialization_failure", "could not serialize access"))
            c.request()
            c.ready()

        service = self.serve(conflicts)
        s = self.connect(service)
        with self.assertRaises(MorphologRequestError) as err:
            s.propose("open_account", "teller", OPEN)
        self.assertEqual(err.exception.code, "serialization_failure")
        self.assertEqual(s.ping(), s.ready)
        self.assertEqual(
            [json.loads(line)["op"] for line in service.received], ["propose", "ping"]
        )


if __name__ == "__main__":
    unittest.main()
