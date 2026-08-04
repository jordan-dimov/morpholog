"""A resident ``morpholog session`` process: one child, one warm
database connection, many operations - the escape from the per-call
subprocess and connection tax.

The wire is lockstep NDJSON: one request line in, one response line
out, in order, with no correlation ids - so this client serialises
callers with a lock rather than documenting "not thread-safe", and
treats any break in the lockstep as fatal to the whole session. That
is the load-bearing lifecycle rule: after a timeout, a dead process,
or a malformed response, a late line could otherwise be consumed as
the answer to the NEXT request, silently desynchronising every
outcome that follows. The session is POISONED instead: the child is
terminated and every subsequent call refuses.

The second load-bearing rule is what a lost response means. Once a
propose request has been written and flushed, no trustworthy response
means the commit outcome is UNKNOWN - the database may have committed
before the failure. That case raises ``MorphologOutcomeUnknown``,
never plain ``MorphologError``: blindly re-submitting can duplicate a
business action. A ``serialization_failure`` error receipt, by
contrast, is the one answer that is safe to re-submit on - the
request was decided as not-committed.

The connection string travels in the child's environment, never its
argv: a resident process would otherwise show it in the OS process
list for its whole lifetime.
"""

from __future__ import annotations

import collections
import json
import os
import queue
import subprocess
import threading
import time

from . import envelopes
from .adapter import MorphologError, _redact_argv

#: The wire version this client speaks; the ready line must agree.
PROTOCOL = 1

#: How many trailing stderr lines are kept for error messages.
_STDERR_TAIL = 50

#: The whole reaping sequence - graceful exit, terminate, kill, drain
#: joins - shares this many seconds. Staged under one deadline so a
#: child that ignores every signal cannot stack one wait on the next.
_SHUTDOWN_BUDGET = 5.0


class _ResponseContract(Exception):
    """A response that broke its pinned shape, raised by a decoder
    while the exchange lock is still held. The exchange poisons the
    session and re-raises it as the caller's error, so no decision
    about a response is ever made after the lock has been released."""

    def __init__(self, poison: str, detail: str) -> None:
        super().__init__(detail)
        self.poison = poison
        self.detail = detail


def _decode_receipt(payload: object, expected_row: int) -> object:
    """The propose decoder: parse the receipt, match it to the row
    THIS caller sent, and refuse an uncoded error. Every check reads
    the local expected row - never the session's shared counter,
    which a concurrent caller may already have advanced."""
    try:
        receipt = envelopes.BatchReceipt.from_json(payload)
    except envelopes.EnvelopeError as exc:
        raise _ResponseContract(
            "a propose response did not match the receipt contract",
            f"unparseable propose receipt: {exc}",
        ) from None
    if receipt.row != expected_row:
        raise _ResponseContract(
            "response row does not match the request",
            f"session answered row {receipt.row} to request {expected_row}",
        )
    if isinstance(receipt.outcome, envelopes.BatchError):
        raise _ResponseContract(
            "a propose response carried an uncoded error",
            f"uncoded session error: {receipt.outcome.error}",
        )
    return receipt.outcome


def _decode_rows(cls: type):
    """The read decoder: the pinned array shape, then every row
    against its envelope contract."""

    def decode(payload: object, _expected_row: int) -> list[object]:
        if not isinstance(payload, list):
            raise _ResponseContract(
                "a read response was not the pinned array shape",
                f"malformed read response: {payload!r}",
            )
        try:
            return [cls.from_json(r) for r in payload]
        except envelopes.EnvelopeError as exc:
            raise _ResponseContract(
                "a read row did not match the pinned contract",
                f"malformed read row: {exc}",
            ) from None

    return decode


class MorphologRequestError(MorphologError):
    """A per-request session error receipt: the request was received,
    classified, and refused, and the session is still healthy. The
    stable ``code`` says whether re-submitting is safe -
    ``serialization_failure`` is the one re-submittable code."""

    def __init__(self, code: str, error: str, row: int) -> None:
        super().__init__(f"session request {row} refused ({code}): {error}")
        self.code = code
        self.error = error
        self.row = row


class MorphologOutcomeUnknown(MorphologError):
    """A propose request was submitted but no trustworthy response
    arrived: the commit outcome is unknown. The database may have
    committed before the session died, so re-submitting blindly can
    duplicate a business action - read the record first."""


class Session:
    """A context manager over one resident ``morpholog session`` child.

    Opening spawns the child, waits for its ready line, and - when
    ``expected_model_hash`` is given - refuses to open against a
    programme whose canonical hash is not the one this deployment
    expects. Prefer the generated ``open_session``, which pins the
    hash this package was built against; construct ``Session``
    directly to open deliberately unpinned.

    ``timeout`` bounds how long each request waits for its RESPONSE in
    seconds (and the ready handshake); ``None`` waits indefinitely. It
    does not bound the call as a whole: a request that times out then
    reaps the child, which is capped separately at ``_SHUTDOWN_BUDGET``
    seconds, so the worst case a caller sees is roughly the two added
    together.

    The programme is pinned by the child at startup: editing the file
    does not change a running session, and rolling out a new model
    means starting new sessions and draining old ones.
    """

    def __init__(
        self,
        file: str,
        database_url: str,
        binary: str | None = None,
        timeout: float | None = None,
        expected_model_hash: str | None = None,
    ) -> None:
        self.file = str(file)
        self.database_url = database_url
        self.binary = binary or os.environ.get("MORPHOLOG_BIN", "morpholog")
        self.timeout = timeout
        self._lock = threading.Lock()
        self._poisoned: str | None = None
        self._row = 0

        argv = [self.binary, "session", self.file]
        env = dict(os.environ)
        env["DATABASE_URL"] = database_url
        try:
            self._child = subprocess.Popen(
                argv,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                encoding="utf-8",
                errors="strict",
                env=env,
            )
        except OSError as exc:
            raise MorphologError(f"`{_redact_argv(argv)}` failed to start: {exc}") from None

        # Both pipes are drained by threads: stdout into the response
        # queue this client waits on, stderr into a bounded tail - an
        # unread stderr pipe could otherwise fill and deadlock the
        # child. ``None`` in the queue means the child closed stdout.
        self._responses: queue.Queue[str | None] = queue.Queue()
        self._stderr_tail: collections.deque[str] = collections.deque(maxlen=_STDERR_TAIL)
        # The tail is written by the drain thread and read by error
        # paths that already hold the operation lock, so the ordering
        # is always operation lock then stderr lock, never the reverse.
        self._stderr_lock = threading.Lock()
        self._drains = [
            threading.Thread(target=self._drain_stdout, daemon=True),
            threading.Thread(target=self._drain_stderr, daemon=True),
        ]
        for thread in self._drains:
            thread.start()

        try:
            ready_line = self._next_line(self.timeout)
        except MorphologError:
            self._poison("the session never became ready")
            raise
        try:
            self.ready = envelopes.SessionReady.from_json(json.loads(ready_line))
        except (ValueError, envelopes.EnvelopeError) as exc:
            self._poison("the ready line did not parse")
            raise MorphologError(f"malformed session ready line: {exc}") from None
        if self.ready.protocol != PROTOCOL:
            self._poison("protocol mismatch")
            raise MorphologError(
                f"session speaks protocol {self.ready.protocol}, this client {PROTOCOL}"
            )
        if expected_model_hash is not None and self.ready.model_hash != expected_model_hash:
            self._poison("model hash mismatch")
            raise MorphologError(
                f"session pinned model {self.ready.model_hash}, expected {expected_model_hash}"
            )

    @property
    def model_hash(self) -> str:
        """The canonical rules-identity hash the session is pinned at."""
        return self.ready.model_hash

    # ------------------------------------------------------------
    # Lifecycle.
    # ------------------------------------------------------------

    def __enter__(self) -> Session:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()

    def close(self) -> None:
        """End the session: EOF on stdin, then reap the child,
        escalating terminate -> kill if it does not exit."""
        with self._lock:
            if self._poisoned is None:
                self._poisoned = "the session is closed"
            self._shutdown()

    def _shutdown(self) -> None:
        deadline = time.monotonic() + _SHUTDOWN_BUDGET

        def budget(share: float) -> float:
            # A share of what is LEFT, so an unresponsive stage cannot
            # spend the next one's time: the whole sequence stays
            # inside one ceiling instead of stacking waits.
            return max(0.0, (deadline - time.monotonic()) * share)

        if self._child.stdin is not None:
            try:
                self._child.stdin.close()
            except OSError:
                pass
        try:
            self._child.wait(timeout=budget(0.5))
        except subprocess.TimeoutExpired:
            self._child.terminate()
            try:
                self._child.wait(timeout=budget(0.5))
            except subprocess.TimeoutExpired:
                # SIGKILL cannot be caught, so this wait returns.
                self._child.kill()
                self._child.wait()
        # The child is gone: let the drain threads hit EOF, then close
        # the parent-side pipe wrappers they were reading.
        for thread in self._drains:
            thread.join(timeout=budget(0.5))
        for pipe in (self._child.stdout, self._child.stderr):
            if pipe is not None:
                try:
                    pipe.close()
                except OSError:
                    pass

    def _poison(self, reason: str) -> None:
        """Mark the session unusable and reap the child. A poisoned
        session refuses every later call: a late response line must
        never be read as the answer to a newer request."""
        if self._poisoned is None:
            self._poisoned = reason
        self._shutdown()

    def _drain_stdout(self) -> None:
        stdout = self._child.stdout
        assert stdout is not None
        try:
            for line in stdout:
                self._responses.put(line)
        except (OSError, ValueError):
            # A stream that cannot be read further - a decode error
            # under strict UTF-8, a pipe closed under the reader - is
            # the end of the conversation as far as lockstep is
            # concerned; the sentinel below routes the waiting caller
            # onto the process-ended path instead of a silent hang.
            pass
        finally:
            self._responses.put(None)

    def _drain_stderr(self) -> None:
        stderr = self._child.stderr
        assert stderr is not None
        try:
            for line in stderr:
                with self._stderr_lock:
                    self._stderr_tail.append(line.rstrip("\n"))
        except (OSError, ValueError):
            pass

    def _stderr_text(self) -> str:
        # Snapshot under the lock: joining the deque while the drain
        # thread appends raises, turning the diagnostic into a crash
        # that hides the failure it was written to explain.
        with self._stderr_lock:
            lines = list(self._stderr_tail)
        text = "\n".join(lines).strip()
        if self.database_url:
            text = text.replace(self.database_url, "<redacted>")
        return text

    def _next_line(self, timeout: float | None) -> str:
        """One line from the child, or an operational failure: EOF
        (the child died) and a timeout both break the lockstep."""
        try:
            line = self._responses.get(timeout=timeout)
        except queue.Empty:
            raise MorphologError(f"the session did not answer within {timeout}s") from None
        if line is None:
            raise MorphologError(
                f"the session process ended unexpectedly:\n{self._stderr_text()}"
            )
        return line.rstrip("\n")

    # ------------------------------------------------------------
    # The one exchange seam.
    # ------------------------------------------------------------

    def _exchange(self, body: dict[str, object], *, commitful: bool, decode) -> object:
        """Write one request line, wait for its one response line, and
        decode it - all in lockstep under the lock. Any break after the
        request has been flushed poisons the session; for a commitful
        request it raises outcome-unknown, because the database may
        have committed.

        ``decode(payload, expected_row)`` runs BEFORE the lock is
        released, and that placement is the contract: a decoder that
        ran afterwards would be judging this caller's response against
        a session another caller has already moved on."""
        with self._lock:
            if self._poisoned is not None:
                raise MorphologError(f"this session is unusable: {self._poisoned}")
            wire = json.dumps(body, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
            stdin = self._child.stdin
            assert stdin is not None
            try:
                stdin.write(wire + "\n")
                stdin.flush()
            except (OSError, ValueError):
                # A failed write or flush does NOT prove nothing
                # crossed the process boundary: the line may have
                # reached the pipe, and the proposal may have
                # committed, before the failure surfaced here.
                self._poison("the request could not be written reliably")
                detail = self._stderr_text()
                if commitful:
                    raise MorphologOutcomeUnknown(
                        "the proposal may have reached the session before the "
                        "write failed; the commit outcome is unknown - read the "
                        f"record before re-submitting.\n{detail}"
                    ) from None
                raise MorphologError(
                    f"the session process ended unexpectedly:\n{detail}"
                ) from None
            self._row += 1
            expected_row = self._row
            try:
                line = self._next_line(self.timeout)
            except MorphologError as exc:
                self._poison(str(exc))
                if commitful:
                    raise MorphologOutcomeUnknown(
                        f"request {expected_row} was submitted but no response arrived; "
                        f"the commit outcome is unknown - read the record before "
                        f"re-submitting. ({exc})"
                    ) from None
                raise
            try:
                payload = json.loads(line)
            except ValueError:
                self._poison("a response line did not parse as JSON")
                message = f"malformed session response: {line[:200]!r}"
                if commitful:
                    raise MorphologOutcomeUnknown(message) from None
                raise MorphologError(message) from None
            if isinstance(payload, dict) and payload.get("status") == "error":
                try:
                    receipt = envelopes.SessionErrorReceipt.from_json(payload)
                except Exception as exc:
                    # An error without the stable code is drift, and a
                    # drifted stream cannot be trusted to stay in step.
                    self._poison("an error response did not match the receipt contract")
                    message = f"uncoded session error: {exc}"
                    if commitful:
                        raise MorphologOutcomeUnknown(message) from None
                    raise MorphologError(message) from None
                if receipt.row != expected_row:
                    self._poison("response row does not match the request")
                    message = (
                        f"session answered row {receipt.row} to request {expected_row}"
                    )
                    if commitful:
                        raise MorphologOutcomeUnknown(message)
                    raise MorphologError(message)
                raise MorphologRequestError(receipt.code, receipt.error, receipt.row)
            try:
                return decode(payload, expected_row)
            except _ResponseContract as exc:
                self._poison(exc.poison)
                if commitful:
                    raise MorphologOutcomeUnknown(exc.detail) from None
                raise MorphologError(exc.detail) from None
            except Exception as exc:
                # A decoder reaches value codecs that raise their own
                # types, so the contract cannot be recognised by one
                # exception class. Anything a decoder throws means the
                # response was not understood, and a submitted proposal
                # whose response was not understood is UNDECIDED - the
                # one thing this client must never report as an
                # ordinary error. BaseException is deliberately not
                # caught: an interrupt still means what it says.
                self._poison("a response did not match the pinned client contract")
                detail = f"response decoder failed: {exc}"
                if commitful:
                    raise MorphologOutcomeUnknown(detail) from None
                raise MorphologError(detail) from None

    # ------------------------------------------------------------
    # The operations.
    # ------------------------------------------------------------

    def propose(
        self,
        transformation: str,
        actor: str,
        args_named: dict[str, object],
        explain_on_reject: bool = False,
    ) -> envelopes.Committed | envelopes.Rejected:
        """Propose a change through the session: it commits only if
        every rule holds; a refusal is a lawful outcome, returned as
        ``Rejected``."""
        body: dict[str, object] = {
            "op": "propose",
            "transformation": transformation,
            "actor": actor,
            "args_named": dict(args_named),
        }
        if explain_on_reject:
            body["explain_on_reject"] = True
        return self._exchange(body, commitful=True, decode=_decode_receipt)

    def submit(
        self, request: object, actor: str, explain_on_reject: bool = False
    ) -> envelopes.Committed | envelopes.Rejected:
        """Commit a generated request model through the session: its
        class names the transformation, its fields encode themselves."""
        return self.propose(
            request.TRANSFORMATION,  # type: ignore[attr-defined]
            actor,
            request.to_args_named(),  # type: ignore[attr-defined]
            explain_on_reject=explain_on_reject,
        )

    def claims(
        self, *predicates: str, as_of: str | None = None
    ) -> list[envelopes.ClaimInstance]:
        """The bare claims read, as on the one-shot client: the claims
        table is the authority, an unknown predicate matches nothing."""
        body = self._claims_body(predicates, named=False, as_of=as_of)
        return self._read_rows(body, envelopes.ClaimInstance)

    def claims_named(
        self,
        *predicates: str,
        as_of: str | None = None,
        where: dict[str, str] | None = None,
    ) -> list[envelopes.NamedClaim]:
        """The named claims read: the programme the session pinned is
        the authority; skew is a hard session error, not a request
        error."""
        body = self._claims_body(predicates, named=True, as_of=as_of)
        if where:
            body["where"] = dict(where)
        return self._read_rows(body, envelopes.NamedClaim)

    def derived(self, name: str, *, as_of: str | None = None) -> list[envelopes.ClaimInstance]:
        """Compute a read-side view through the session - always live,
        never the refresh cache, as on the one-shot client."""
        body = self._derived_body(name, named=False, as_of=as_of)
        return self._read_rows(body, envelopes.ClaimInstance)

    def derived_named(
        self, name: str, *, as_of: str | None = None, where: dict[str, str] | None = None
    ) -> list[envelopes.NamedClaim]:
        """``derived`` with each row's arguments decoded by declared
        field name."""
        body = self._derived_body(name, named=True, as_of=as_of)
        if where:
            body["where"] = dict(where)
        return self._read_rows(body, envelopes.NamedClaim)

    def _claims_body(
        self, predicates: tuple[str, ...], named: bool, as_of: str | None
    ) -> dict[str, object]:
        body: dict[str, object] = {"op": "claims"}
        if predicates:
            body["predicates"] = list(predicates)
        if named:
            body["named"] = True
        if as_of is not None:
            body["as_of"] = as_of
        return body

    def _derived_body(self, name: str, named: bool, as_of: str | None) -> dict[str, object]:
        body: dict[str, object] = {"name": name, "op": "derived"}
        if named:
            body["named"] = True
        if as_of is not None:
            body["as_of"] = as_of
        return body

    def _read_rows(self, body: dict[str, object], cls: type) -> list[object]:
        """One read exchange, decoded into `cls` rows under the lock. A
        row that does not match the pinned contract poisons the
        session: the framing is intact, but a binary/client contract
        mismatch will not heal on the next call."""
        return self._exchange(body, commitful=False, decode=_decode_rows(cls))
