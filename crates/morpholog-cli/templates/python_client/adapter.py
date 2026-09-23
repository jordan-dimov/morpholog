"""The subprocess adapter: one method per pinned CLI surface entry.

The load-bearing rule is output discrimination: every DECIDED result
arrives on stdout - a committed or rejected outcome (exit 1 flags the
rejection, but the receipt is still the result), a schema, an outbox
row, a check report. For a read, empty stdout is an operational failure
and raises ``MorphologError``. For a proposal, only the binary's own
statement - a decided envelope or a coded error object - settles the
outcome; anything else raises ``MorphologOutcomeUnknown``.

This module never imports the generated ``models``; ``submit`` is
duck-typed on the two class attributes every generated request model
carries (``TRANSFORMATION`` and ``to_args_named``), so the static and
generated halves of the package meet only at that seam.
"""

from __future__ import annotations

import contextlib
import json
import os
import subprocess
import tempfile
from typing import IO, Callable, TypeVar

from . import envelopes

_Verdict = TypeVar("_Verdict")
_T = TypeVar("_T")


# Flags whose VALUE is a credential. It must never appear in a raised
# message: these are structured-logged and may be reflected to a caller.
_CREDENTIAL_FLAGS = frozenset({"--database-url"})


def _redact_argv(args: list[str]) -> str:
    """Join an argv for an error message, masking the value after any
    credential-bearing flag. The rest of the argv is safe to echo."""
    parts: list[str] = []
    redact_next = False
    for arg in args:
        if redact_next:
            parts.append("<redacted>")
            redact_next = False
        else:
            parts.append(arg)
            redact_next = arg in _CREDENTIAL_FLAGS
    return " ".join(parts)


def _text(output: str | bytes | None) -> str:
    """Output captured from a killed child arrives as bytes."""
    if output is None:
        return ""
    if isinstance(output, bytes):
        return output.decode("utf-8", "replace")
    return output


class MorphologError(RuntimeError):
    """An operational failure from the CLI - distinct from a lawful
    business rejection, which is a decided outcome on stdout."""

    #: Whether re-submitting the same request is known to be safe. False
    #: for every error but a ``serialization_failure`` receipt: an
    #: unknown outcome may already have committed, and any other failure
    #: needs its cause fixed first. The one retry predicate.
    retriable: bool = False


class MorphologRequestError(MorphologError):
    """A per-request error receipt with its stable ``code``: the request
    was received, classified, and refused, and the session (or the
    one-shot binary) did nothing durable. ``serialization_failure`` is
    re-submittable as is - ``retriable`` says so; ``not_committed`` once
    its cause is fixed; every other code is the request's own fault.
    ``row`` is the session request number, or ``None`` for a one-shot
    ``propose``, ``transact`` or a batch refused before its first row."""

    def __init__(self, code: str, error: str, row: int | None = None) -> None:
        where = f"session request {row}" if row is not None else "request"
        super().__init__(f"{where} refused ({code}): {error}")
        self.code = code
        self.error = error
        self.row = row

    @property
    def retriable(self) -> bool:  # type: ignore[override]
        return self.code == "serialization_failure"


class MorphologTimeout(MorphologError):
    """The binary did not finish within the client's timeout and was
    killed. Operational for a read; for a proposal the caller must not
    assume nothing changed, since the kill can land after COMMIT was
    sent - ``propose`` re-raises it as ``MorphologOutcomeUnknown``.
    ``stdout`` and ``stderr`` hold what the binary printed before it
    was killed, which a batch needs to know which rows finished."""

    def __init__(self, message: str, stdout: str = "", stderr: str = "") -> None:
        super().__init__(message)
        self.stdout = stdout
        self.stderr = stderr


class MorphologBatchIncomplete(MorphologError):
    """A batch without a trustworthy receipt for every row. ``receipts``
    are the rows that finished, each decided as its receipt says. The
    rows in ``unknown_rows`` may have committed: read the record before
    re-submitting them. The rows in ``not_attempted`` never ran.

    If the binary stopped - killed, timed out, crashed, or aborted
    without a receipt - one row was in flight, so ``unknown_rows`` holds
    that row and ``not_attempted`` the rest. If instead a receipt could
    not be trusted - a line that does not parse, an out-of-order row, an
    unpublished code, or a clean exit short of receipts - the binary may
    have gone on, so every row from there is unknown and none is known
    not to have run. Rows are 1-based positions in the list passed to
    ``propose_batch``."""

    def __init__(
        self,
        receipts: list[envelopes.BatchReceipt],
        unknown_rows: list[int],
        not_attempted: list[int],
        detail: str,
    ) -> None:
        super().__init__(
            f"batch incomplete: {len(receipts)} row(s) finished, "
            f"{len(unknown_rows)} unknown from row {unknown_rows[0]} - read the "
            f"record before re-submitting them, {len(not_attempted)} not attempted:"
            f"\n{detail}"
        )
        self.receipts = receipts
        self.unknown_rows = unknown_rows
        self.not_attempted = not_attempted


class MorphologOutcomeUnknown(MorphologError):
    """A proposal was submitted and its commit outcome cannot be proven.
    Either no trustworthy response arrived (a session died, hung, or
    answered garbage after the request was written), or the runtime
    itself reported ``commit_outcome_unknown``: the database connection
    failed while COMMIT was in flight, after the server may already have
    made it durable. Re-submitting blindly can duplicate a business
    action - read the record first."""


class Morpholog:
    """A typed client over the ``morpholog`` CLI: arguments in, parsed
    envelope dataclasses out.

    ``binary`` resolves as: explicit argument, then the
    ``MORPHOLOG_BIN`` environment variable, then ``morpholog`` on
    ``PATH``.

    ``timeout`` bounds normal single-operation calls (read, commit,
    audit, outbox) in seconds; a call that overruns raises
    ``MorphologError`` - a stuck binary becomes an operational failure,
    never a stuck request. It defaults to unbounded. ``propose_batch``
    stays unbounded even when it is set - a large import is the
    legitimate long case - and takes a per-call ``timeout`` to bound
    one batch.
    """

    def __init__(
        self,
        file: str,
        database_url: str,
        binary: str | None = None,
        timeout: float | None = None,
    ) -> None:
        self.file = str(file)
        self.database_url = database_url
        self.binary = binary or os.environ.get("MORPHOLOG_BIN", "morpholog")
        self.timeout = timeout

    # ------------------------------------------------------------
    # The one subprocess seam.
    # ------------------------------------------------------------

    def _run(
        self,
        args: list[str],
        stdin: str | None = None,
        *,
        timeout: float | None,
        stdout: IO[bytes] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Every invocation lands here. A timeout is operational, not a
        decided outcome, so it raises ``MorphologError``. ``stdout``, when
        given, receives the output instead of memory."""
        try:
            return subprocess.run(
                [self.binary, *args],
                stdout=subprocess.PIPE if stdout is None else stdout,
                stderr=subprocess.PIPE,
                text=True,
                input=stdin,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as exc:
            raise MorphologTimeout(
                f"`{self.binary} {_redact_argv(args)}` timed out after {timeout}s",
                stdout=_text(exc.stdout),
                stderr=self._redact_stderr(_text(exc.stderr)),
            ) from None

    def _redact_stderr(self, stderr: str) -> str:
        """Mask the client's own conninfo in any stderr it surfaces - a
        PG driver error can echo the connection string verbatim. Used at
        every raised operational error, not only ``_invoke``, so a path
        that bypasses ``_invoke`` (batch, audit) cannot leak it."""
        stderr = stderr.strip()
        if self.database_url:
            stderr = stderr.replace(self.database_url, "<redacted>")
        return stderr

    def _invoke(self, *args: str, stdin: str | None = None) -> str:
        proc = self._run(list(args), stdin=stdin, timeout=self.timeout)
        if not proc.stdout.strip():
            raise MorphologError(
                f"`{_redact_argv(list(args))}`:\n{self._redact_stderr(proc.stderr)}"
            )
        return proc.stdout

    def _json(self, *args: str) -> object:
        return json.loads(self._invoke(*args))

    @staticmethod
    def _opt(flag: str, value: str | None) -> list[str]:
        return [] if value is None else [flag, value]

    @staticmethod
    def _repeat(flag: str, values: list[str] | None) -> list[str]:
        return [part for v in values or [] for part in (flag, v)]

    # ------------------------------------------------------------
    # Provisioning and model identity.
    # ------------------------------------------------------------

    def init(
        self,
        skip_if_exists: bool = False,
        least_privilege: bool = False,
        reset: bool = False,
        i_know_this_deletes_data: bool = False,
    ) -> envelopes.InitReport:
        """Provision the Morpholog tables.

        `reset` DESTROYS every claim, audit row and outbox entry in the
        target database, and needs `i_know_this_deletes_data` alongside
        it. Both flags are passed straight through, so the pairing is
        enforced in one place - the binary - rather than re-implemented
        here where it could drift.
        """
        args = ["init", "--database-url", self.database_url]
        if skip_if_exists:
            args.append("--skip-if-exists")
        if least_privilege:
            args.append("--least-privilege")
        if reset:
            args.append("--reset")
        if i_know_this_deletes_data:
            args.append("--i-know-this-deletes-data")
        return envelopes.InitReport.from_json(self._json(*args))

    def migrate(self, *, check: bool = False) -> envelopes.MigrationReport:
        """Bring the database up to the schema this binary expects.

        The migrations are embedded in the binary, so an upgrade needs
        nothing fetched from a source tree. Applies whatever the database
        has not recorded, in order, and leaves a current one alone.

        **Needs a connection that owns the schema.** Under
        `--least-privilege` the runtime writer role cannot run DDL, so pass
        an administrative URL for this call rather than the one your
        proposals use. `check` only reads, and both roles are granted
        `SELECT` on the record, so a readiness probe can run as either.

        `check` reports and changes nothing. The binary exits non-zero when
        the database is behind, but this client reads stdout rather than the
        exit code, so you get the report either way - which is the more
        useful outcome, because a raise would throw away what is pending.
        Gate on the report:

            if not client.migrate(check=True).is_current:
                ...
        """
        args = ["migrate", "--database-url", self.database_url]
        if check:
            args.append("--check")
        return envelopes.MigrationReport.from_json(self._json(*args))

    def hash(self) -> envelopes.HashReport:
        return envelopes.HashReport.from_json(self._json("hash", self.file))

    def check(self, strict: bool = False) -> envelopes.CheckReport:
        args = ["check", "--json", self.file]
        if strict:
            args.append("--strict")
        return envelopes.CheckReport.from_json(self._json(*args))

    # ------------------------------------------------------------
    # The commit path.
    # ------------------------------------------------------------

    def propose(
        self,
        transformation: str,
        actor: str,
        args_named: dict[str, object],
        explain_on_reject: bool = False,
    ) -> envelopes.Committed | envelopes.Rejected:
        """Propose a change by transformation name: it commits only if
        everything it touches still obeys every rule; a refusal is a lawful outcome, returned as
        ``Rejected``. When the binary says nothing was committed, it
        raises ``MorphologRequestError`` with the binary's code. Anything
        else - a commit whose outcome the runtime could not prove, a
        timeout, a crash, silence - raises ``MorphologOutcomeUnknown``:
        read the record before re-submitting."""
        args = [
            "propose", self.file, transformation,
            "--actor", actor,
            "--args-named", json.dumps({k: v for k, v in args_named.items()}),
            "--database-url", self.database_url,
        ]
        if explain_on_reject:
            args.append("--explain-on-reject")
        return self._commit(args, None, self.timeout, envelopes.parse_run_outcome)

    def submit(
        self, request: object, actor: str, explain_on_reject: bool = False
    ) -> envelopes.Committed | envelopes.Rejected:
        """Commit a generated request model: its class names the
        transformation, its fields encode themselves."""
        return self.propose(
            request.TRANSFORMATION,  # type: ignore[attr-defined]
            actor,
            request.to_args_named(),  # type: ignore[attr-defined]
            explain_on_reject=explain_on_reject,
        )

    def propose_batch(
        self,
        rows: list[dict[str, object]],
        timeout: float | None = None,
        *,
        explain_on_reject: bool = False,
    ) -> list[envelopes.BatchReceipt]:
        """Admit many rows in one invocation (`propose --batch -`).

        Each row is a dict with ``transformation``, ``actor``, and one
        of ``args``/``args_named``. Returns one ``BatchReceipt`` per row
        when every row has one. If the binary refused the batch before
        its first row, raises ``MorphologRequestError``: nothing ran.
        If the batch ended early for any other reason - a timeout, a
        crash, a kill - raises ``MorphologBatchIncomplete`` with the
        receipts that arrived, the row that may have committed, and the
        rows that never ran. ``explain_on_reject`` attaches the
        same-snapshot why to every rejected row, as on ``propose``.
        ``timeout`` bounds this one call and defaults to unbounded,
        ignoring the client-wide timeout - a large import is the
        legitimate long-running case.
        """
        ndjson = "".join(json.dumps(row) + "\n" for row in rows)
        args = ["propose", self.file, "--batch", "-", "--database-url", self.database_url]
        if explain_on_reject:
            args.append("--explain-on-reject")
        try:
            proc = self._run(args, stdin=ndjson, timeout=timeout)
            stdout, stderr, how = proc.stdout, proc.stderr, f"exit {proc.returncode}"
            clean = proc.returncode == 0
        except MorphologTimeout as exc:
            stdout, stderr, how, clean = exc.stdout, exc.stderr, str(exc), False
        # Only whole lines count: text after the last newline is a line
        # the binary had not finished writing.
        lines = [line for line in stdout.split("\n")[:-1] if line.strip()]
        receipts: list[envelopes.BatchReceipt] = []
        # Whether a whole line could not be trusted, as opposed to the
        # output simply ending: only an ending says the rest never ran.
        untrusted = False
        for index, line in enumerate(lines):
            try:
                payload = json.loads(line)
            except ValueError:
                untrusted = True
                break
            if index == 0 and isinstance(payload, dict) and "row" not in payload:
                self._refused_before_the_first_row(payload, stderr)
            try:
                receipt = envelopes.BatchReceipt.from_json(payload)
            except envelopes.EnvelopeError:
                untrusted = True
                break
            # Receipts arrive in row order; anything else is not one.
            if receipt.row != len(receipts) + 1:
                untrusted = True
                break
            # A code this client does not know says nothing about the row.
            outcome = receipt.outcome
            if (
                isinstance(outcome, envelopes.BatchError)
                and outcome.code not in envelopes.PROPOSE_ERROR_CODES
            ):
                untrusted = True
                break
            receipts.append(receipt)
        if not rows:
            # No rows, so no receipts - but only a clean, silent exit
            # says the empty batch ran.
            if clean and not lines:
                return receipts
            raise MorphologError(
                f"an empty batch did not complete ({how}):\n{self._redact_stderr(stderr)}"
            )
        if len(receipts) == len(rows):
            return receipts
        first = len(receipts) + 1
        rest = list(range(first, len(rows) + 1))
        stopped = not clean and not untrusted
        raise MorphologBatchIncomplete(
            receipts,
            rest[:1] if stopped else rest,
            rest[1:] if stopped else [],
            f"{how}\n{self._redact_stderr(stderr)}",
        )

    def _refused_before_the_first_row(self, payload: object, stderr: str) -> None:
        """A batch refused as a whole prints one error object with no
        ``row``: nothing ran. Only a published code other than
        ``commit_outcome_unknown`` says so; anything else is left to the
        caller to read as an incomplete batch."""
        try:
            error = envelopes.RequestError.from_json(payload)
        except envelopes.EnvelopeError:
            return
        if error.code in envelopes.NOTHING_RECORDED_CODES:
            raise MorphologRequestError(error.code, error.error)

    def transact(
        self, acts: list[dict[str, object]], timeout: float | None = None
    ) -> envelopes.AtomicCommitted | envelopes.AtomicRejected:
        """Propose several acts as one decision (`transact --acts -`):
        every act commits or none does. Each act is a dict in the batch
        row shape; they apply in order, and each sees what the acts
        before it staged. Returns ``AtomicCommitted`` (one receipt per
        act) or ``AtomicRejected`` (the refusing act, nothing written) -
        both lawful outcomes. A known error of the whole batch raises
        ``MorphologRequestError`` with its code, whose ``retriable`` is
        true only for ``serialization_failure``; a commit whose outcome
        the runtime could not prove, or a timeout after submission,
        raises ``MorphologOutcomeUnknown`` - read the record first,
        never re-submit blind. ``timeout`` bounds this one call and
        defaults to unbounded, as for a batch."""
        ndjson = "".join(json.dumps(act) + "\n" for act in acts)
        args = ["transact", self.file, "--acts", "-", "--database-url", self.database_url]
        return self._commit(args, ndjson, timeout, envelopes.parse_atomic_outcome)

    def _commit(
        self,
        args: list[str],
        stdin: str | None,
        timeout: float | None,
        parse: Callable[[object], _T],
    ) -> _T:
        """Run a proposal and return its decided outcome. Only the
        binary's own statement settles it: a decided envelope, or an
        error object carrying a published code. Everything else - a
        timeout, a crash, a signal, silence, output that does not parse
        - is unknown, whatever the exit code, because the process may
        have died after COMMIT was sent."""

        def unknown(why: str, stderr: str = "") -> MorphologOutcomeUnknown:
            detail = f":\n{self._redact_stderr(stderr)}" if stderr.strip() else ""
            return MorphologOutcomeUnknown(
                f"`{_redact_argv(args)}`: {why}; the commit outcome is unknown - "
                f"read the record before re-submitting{detail}"
            )

        try:
            proc = self._run(args, stdin=stdin, timeout=timeout)
        except MorphologTimeout as exc:
            raise unknown(str(exc), exc.stderr) from None
        try:
            payload = json.loads(proc.stdout)
        except ValueError:
            raise unknown(
                f"the binary exited {proc.returncode} without a statement on stdout",
                proc.stderr,
            ) from None
        if isinstance(payload, dict) and payload.get("status") == "error":
            try:
                error = envelopes.RequestError.from_json(payload)
            except envelopes.EnvelopeError as exc:
                raise unknown(f"an error object outside the contract ({exc})") from None
            if error.code == "commit_outcome_unknown":
                raise unknown(error.error, proc.stderr)
            if error.code not in envelopes.NOTHING_RECORDED_CODES:
                raise unknown(f"an unpublished error code {error.code!r}", proc.stderr)
            raise MorphologRequestError(error.code, error.error)
        try:
            return parse(payload)
        except Exception as exc:
            raise unknown(f"an outcome this client does not understand ({exc})") from None

    def explain(
        self, transformation: str, actor: str, args_named: dict[str, object]
    ) -> envelopes.Explanation:
        return envelopes.Explanation.from_json(
            self._json(
                "explain", self.file, transformation,
                "--actor", actor,
                "--args-named", json.dumps(args_named),
                "--json",
                "--database-url", self.database_url,
            )
        )

    # ------------------------------------------------------------
    # Reading governed state back.
    # ------------------------------------------------------------

    def claims(self, *predicates: str, as_of: str | None = None) -> list[envelopes.ClaimInstance]:
        """The bare read: the claims table is the authority, an unknown
        predicate matches nothing. Tagged args decoded to bare values.

        `as_of` reads the claims as they were at a past moment - a
        transition id, or an RFC 3339 timestamp resolved to the last
        transition committed at or before it.
        """
        return self._claims(predicates, named=False, as_of=as_of)

    def claims_named(
        self,
        *predicates: str,
        as_of: str | None = None,
        where: dict[str, str] | None = None,
    ) -> list[envelopes.NamedClaim]:
        """The named read: the programme is the authority, skew is a
        hard error on the binary side. Values stay wire-true; the
        generated read models parse them by declared kind. `as_of` as
        on ``claims``.

        `where` narrows by argument value - ``where={"invoice_id": "inv_1"}``
        - so rows that cannot match are never transferred or decoded.
        That is what it saves: the database still scans the predicate,
        because no index covers argument positions.
        Field names resolve against the programme, so exactly one
        predicate must be named and an undeclared field is an error, not
        an empty list. Filtering runs in the database except under
        `as_of`, where the state is replayed first.
        """
        return self._claims(predicates, named=True, as_of=as_of, where=where)

    def _claims(
        self,
        predicates: tuple[str, ...],
        named: bool,
        as_of: str | None,
        where: dict[str, str] | None = None,
    ) -> list[envelopes.ClaimInstance | envelopes.NamedClaim]:
        argv = ["inspect", "claims"]
        argv += self._repeat("--predicate", list(predicates))
        argv += self._opt("--as-of", as_of)
        argv += self._repeat("--where", [f"{k}={v}" for k, v in (where or {}).items()])
        cls = envelopes.NamedClaim if named else envelopes.ClaimInstance
        if named:
            argv += ["--named", self.file]
        payload = self._json(*argv, "--database-url", self.database_url)
        return [cls.from_json(c) for c in payload]

    def rejections(self, *, limit: int = 100) -> list[envelopes.RejectionRow]:
        """The most recent refusals, newest first, with the values the refused
        rule was reading where the kernel could pin them.

        Bounded: the log grows with every refusal, so raise `limit` to reach
        further back rather than expecting the whole history. A witness runs
        to a few hundred bytes, so a large limit is a large response.

        **An operational floor, not a ledger.** Writes are at-most-once and
        happen after rollback, so a storm or an insert failure can leave a
        refusal unrecorded; audit remains the only legitimacy-grade record.
        Read a row as a lead to follow, and never as proof of what did or
        did not happen.
        """
        payload = self._json(
            "inspect",
            "rejections",
            "--limit",
            str(limit),
            "--database-url",
            self.database_url,
        )
        return [envelopes.RejectionRow.from_json(r) for r in payload]

    def derived(self, name: str, *, as_of: str | None = None) -> list[envelopes.ClaimInstance]:
        """Compute a read-side view (a derived claim) directly from the
        admitted claims - the authoritative, always-live read; it never
        consults the ``refresh_derived`` cache. Rows are tagged
        ``ClaimInstance``s, the same shape ``claims`` returns.

        `as_of` computes the view over the state as it was at a past
        moment - a transition id, or an RFC 3339 timestamp resolved to
        the last transition committed at or before it. Diffing the same
        view at two coordinates is the correction blast-radius read.
        """
        return self._derived(name, named=False, as_of=as_of)

    def derived_named(
        self, name: str, *, as_of: str | None = None, where: dict[str, str] | None = None
    ) -> list[envelopes.NamedClaim]:
        """``derived`` with each row's arguments decoded by declared
        field name (the generated read models parse them by declared
        kind). Same authority and skew contract as ``claims_named``.

        `where` narrows by field, as on ``claims_named``. A derived view
        is computed from claims, so this narrows the answer rather than
        the work - unlike the claims read, which pushes the comparison
        into the database.
        """
        return self._derived(name, named=True, as_of=as_of, where=where)

    def _derived(
        self,
        name: str,
        named: bool,
        as_of: str | None,
        where: dict[str, str] | None = None,
    ) -> list[envelopes.ClaimInstance | envelopes.NamedClaim]:
        argv = ["inspect", "derived", self.file, name]
        cls = envelopes.NamedClaim if named else envelopes.ClaimInstance
        if named:
            argv.append("--named")
        argv += self._opt("--as-of", as_of)
        argv += self._repeat("--where", [f"{k}={v}" for k, v in (where or {}).items()])
        payload = self._json(*argv, "--database-url", self.database_url)
        return [cls.from_json(c) for c in payload]

    def audit(
        self, after: str | None = None, *, writer_roles: list[str] | None = None
    ) -> list[envelopes.AuditRow]:
        """The audit tail: committed transitions in commit order, one
        ``AuditRow`` per NDJSON line. ``after`` resumes strictly after
        a previously seen transition id - lossless: rows whose writers
        were still in flight are withheld until the next call, never
        skipped. An empty tail is a lawful empty list.

        ``writer_roles`` asserts the session roles that write audit,
        for managed PostgreSQL where the platform's hidden sessions
        make the default refuse. The binary verifies the assertion
        against the catalog; superuser writes are the residue the
        assertion explicitly accepts, and role grants, memberships,
        and login attributes must stay unchanged until the command
        establishes its read snapshot."""
        return [
            envelopes.AuditRow.from_json(row)
            for row in self._audit_lines(after, named=False, writer_roles=writer_roles)
        ]

    def audit_named(
        self, after: str | None = None, *, writer_roles: list[str] | None = None
    ) -> list[envelopes.AuditRowNamed]:
        """The audit tail with asserted/retracted claims decoded by
        declared field name under this programme's authority (skew is
        a hard error on the binary side). ``arguments`` and intent
        payloads stay positional - a different vocabulary.
        ``writer_roles`` as on ``audit``."""
        return [
            envelopes.AuditRowNamed.from_json(row)
            for row in self._audit_lines(after, named=True, writer_roles=writer_roles)
        ]

    def _audit_lines(
        self, after: str | None, named: bool, writer_roles: list[str] | None = None
    ) -> list[dict[str, object]]:
        # Not _invoke: an empty tail is a lawful empty stdout, not a
        # protocol violation - so the discrimination here is on the
        # exit code alone.
        argv = ["inspect", "audit"]
        if after is not None:
            argv.extend(["--after", after])
        if named:
            argv.extend(["--named", self.file])
        argv += self._repeat("--writer-role", writer_roles)
        argv.extend(["--database-url", self.database_url])
        proc = self._run(argv, timeout=self.timeout)
        if proc.returncode != 0:
            raise MorphologError(
                f"inspect audit failed (exit {proc.returncode}):\n"
                f"{self._redact_stderr(proc.stderr)}"
            )
        return [
            json.loads(line) for line in proc.stdout.splitlines() if line.strip()
        ]

    def coverage(self) -> envelopes.CoverageReport:
        """Replay the audit log and report which rules have ever
        actually done work - per invariant, whether its condition ever
        matched anything and whether it ever refused a real proposal
        (the `constrained` verdict, counted from the operational
        rejection log); per transformation, whether it was ever used
        and how often it was refused. Read-only."""
        return envelopes.CoverageReport.from_json(
            self._json(
                "inspect", "coverage", self.file,
                "--json",
                "--database-url", self.database_url,
            )
        )

    def refresh_derived(self) -> envelopes.RefreshDerivedReport:
        """Recompute every derived claim with the kernel and publish a
        new generation of the ``morpholog_read`` cache that the
        generated derived SQL views read. Operational, out of band:
        run it after an import or on a schedule. It feeds only the SQL
        views - the ``derived`` reads above compute live and never
        need it."""
        return envelopes.RefreshDerivedReport.from_json(
            self._json(
                "refresh", "derived", self.file,
                "--database-url", self.database_url,
            )
        )

    # ------------------------------------------------------------
    # Tamper-evidence: replay, checkpoints, evidence packs.
    # ------------------------------------------------------------

    def audit_verify(
        self,
        anchor_file: str | None = None,
        require_signatures: bool = False,
        views_schema: str | None = None,
        *,
        require_signatures_from: int | None = None,
        require_signing_key: str | None = None,
        trusted_tsa_file: str | None = None,
    ) -> envelopes.VerifyReport:
        """Replay the audit log against the claims table and check the
        audit Merkle tree against its checkpoints (and an external
        ``anchor_file`` if given). ``require_signatures`` is compliance
        mode: an unsigned checkpoint becomes a failing verdict;
        ``require_signatures_from`` asks only of checkpoints at or after
        that tree size, and ``require_signing_key`` (a file holding the
        ``ed25519-pub:<hex>`` key) fails a covered checkpoint with no
        signature by that key, on top of the key being authorised in
        the log, never instead of it. ``views_schema`` also verifies the generated SQL view surface
        in that schema against its recorded seals, adding the ``views``
        verdict to the report. ``trusted_tsa_file`` (a PEM file of
        timestamp-authority CA certificates) is what the checkpoints'
        external witnesses are judged against; without it a sound
        witness reports ``unverified``. A divergence, tamper, or invalid
        witness is a decided verdict on stdout, not an operational
        error."""
        args = ["audit", "verify", "--database-url", self.database_url]
        if anchor_file is not None:
            args.extend(["--anchor-file", str(anchor_file)])
        args.extend(
            self._signature_policy_args(
                require_signatures, require_signatures_from, require_signing_key
            )
        )
        if views_schema is not None:
            args.extend(["--views-schema", views_schema])
        if trusted_tsa_file is not None:
            args.extend(["--trusted-tsa-file", str(trusted_tsa_file)])
        return envelopes.VerifyReport.from_json(self._json(*args))

    def audit_checkpoint(
        self,
        signing_key: str | None = None,
        key_id: str | None = None,
        *,
        writer_roles: list[str] | None = None,
        witnesses: list[str] | None = None,
    ) -> envelopes.CheckpointCreated | envelopes.CheckpointNoNewRows:
        """Record a checkpoint over the current stable prefix, or return
        the unchanged head - either way a usable external anchor. Pass
        ``signing_key`` (a PKCS#8 PEM path) and ``key_id`` to sign the new
        tree head, so the anchor is attributable. ``writer_roles`` as on
        ``audit`` - the checkpoint's stable prefix rests on the same
        resume horizon. ``witnesses`` (each ``"rfc3161:<url>"``) has those
        timestamp authorities countersign the new head after the commit;
        the checkpoint is recorded either way, and a failed submission is
        an operational error naming ``audit_witness`` to retry."""
        if (signing_key is None) != (key_id is None):
            raise ValueError("signing_key and key_id must be given together")
        args = ["audit", "checkpoint", "--database-url", self.database_url]
        if signing_key is not None:
            args.extend(["--signing-key", str(signing_key), "--key-id", str(key_id)])
        args += self._repeat("--writer-role", writer_roles)
        args += self._repeat("--witness", witnesses)
        return envelopes.parse_checkpoint_outcome(self._json(*args))

    def audit_witness(self, tree_size: int, witnesses: list[str]) -> envelopes.Checkpoint:
        """Have timestamp authorities (each ``"rfc3161:<url>"``) witness
        the checkpoint recorded at ``tree_size``, storing each exact
        response on it as it arrives. Returns the checkpoint as now
        stored. Every authority is attempted; a response that is not over
        this head is refused and stores nothing, and if any authority
        failed the call is an operational error naming the retry, with
        the others' witnesses already stored."""
        if not witnesses:
            raise ValueError("name at least one witness")
        args = ["audit", "witness", "--database-url", self.database_url]
        args.extend(["--tree-size", str(tree_size)])
        args += self._repeat("--witness", witnesses)
        return envelopes.Checkpoint.from_json(self._json(*args))

    def audit_export(
        self, path: str, tree_size: int | None = None, timeout: float | None = None
    ) -> envelopes.PrefixPackManifest:
        """Write a complete-prefix evidence pack covering the latest
        checkpoint, or the one at ``tree_size``, to ``path``, and return its
        manifest. The pack goes to a file beside ``path`` and replaces
        ``path`` only once the export has succeeded, so a failed export
        leaves no partial pack. It carries the full audit prefix -
        confidential data, not selective disclosure. It compresses well
        with gzip, and ``audit_verify_pack`` reads it either way.
        ``timeout`` bounds this one call and defaults to unbounded: a long
        history is the legitimate long-running case."""
        args = ["audit", "export", "--database-url", self.database_url]
        if tree_size is not None:
            args.extend(["--tree-size", str(tree_size)])
        directory = os.path.dirname(os.path.abspath(path))
        fd, partial = tempfile.mkstemp(dir=directory, prefix=".morpholog-export-")
        try:
            with os.fdopen(fd, "wb") as out:
                proc = self._run(args, timeout=timeout, stdout=out)
            if proc.returncode != 0:
                raise MorphologError(
                    f"`{_redact_argv(args)}`:\n{self._redact_stderr(proc.stderr)}"
                )
            with open(partial, "rb") as written:
                manifest = envelopes.PrefixPackManifest.from_json(json.loads(written.readline()))
            os.replace(partial, path)
            return manifest
        except BaseException:
            with contextlib.suppress(FileNotFoundError):
                os.unlink(partial)
            raise

    def audit_verify_pack(
        self,
        pack_file: str,
        anchor_file: str | None = None,
        require_signatures: bool = False,
        *,
        require_signatures_from: int | None = None,
        require_signing_key: str | None = None,
        witnesses: bool = False,
        trusted_tsa_file: str | None = None,
    ) -> envelopes.PackVerificationReport[envelopes.TreeVerification]:
        """Verify a prefix evidence pack offline - no database. Returns the
        tamper-evidence verdict; a tamper or malformed pack is a decided
        verdict on stdout. ``require_signatures``, ``require_signatures_from``
        and ``require_signing_key`` are the verifier's policy, as on
        ``audit_verify``; the pin needs a complete-prefix pack, and a
        window or selective pack refuses it as an operational error. The
        result is a ``PackVerificationReport``: this verdict, the login
        roles seen under a new OID among the pack's rows, and, with
        ``witnesses=True`` or a ``trusted_tsa_file``, what the pack's
        external witnesses prove."""
        return self._verify_pack(
            envelopes.parse_tree_verification,
            pack_file,
            anchor_file,
            require_signatures,
            require_signatures_from,
            require_signing_key,
            witnesses,
            trusted_tsa_file,
        )

    def audit_export_window(
        self,
        from_tree_size: int | None = None,
        to_tree_size: int | None = None,
        from_anchor: str | None = None,
    ) -> envelopes.WindowEvidencePack:
        """Export a WINDOW pack between an earlier checkpoint and the
        covering one (latest, or ``to_tree_size``): it proves the covered
        range extends that start. Give the start as ``from_anchor`` (a path
        to the prior period's checkpoint file - the trust object, and export
        refuses if the stored start has diverged from it) or the weaker
        ``from_tree_size``; exactly one. Carries the window's rows -
        confidential data, not selective disclosure."""
        if (from_anchor is None) == (from_tree_size is None):
            raise ValueError("give exactly one of from_anchor or from_tree_size")
        args = ["audit", "export", "--database-url", self.database_url]
        if from_anchor is not None:
            args.extend(["--from-anchor", str(from_anchor)])
        else:
            args.extend(["--from-tree-size", str(from_tree_size)])
        if to_tree_size is not None:
            args.extend(["--tree-size", str(to_tree_size)])
        return envelopes.WindowEvidencePack.from_json(self._json(*args))

    def audit_verify_pack_window(
        self,
        pack_file: str,
        anchor_file: str | None = None,
        require_signatures: bool = False,
        *,
        require_signatures_from: int | None = None,
        require_signing_key: str | None = None,
        witnesses: bool = False,
        trusted_tsa_file: str | None = None,
    ) -> envelopes.PackVerificationReport[envelopes.WindowVerification]:
        """Verify a window pack offline - no database. Returns the window
        verdict; a tamper, inconsistent extension, or malformed pack is a
        decided verdict on stdout. ``require_signatures`` is compliance
        mode, as on ``audit_verify_pack``. The
        result is a ``PackVerificationReport``: this verdict, the login
        roles seen under a new OID among the pack's rows, and, with
        ``witnesses=True`` or a ``trusted_tsa_file``, what the pack's
        external witnesses prove."""
        return self._verify_pack(
            envelopes.parse_window_verification,
            pack_file,
            anchor_file,
            require_signatures,
            require_signatures_from,
            require_signing_key,
            witnesses,
            trusted_tsa_file,
        )

    def audit_export_selective(
        self,
        transitions: list[str],
        tree_size: int | None = None,
    ) -> envelopes.SelectiveEvidencePack:
        """Export a SELECTIVE pack disclosing only the named transitions,
        each proven included at its position under the covering checkpoint
        (latest, or ``tree_size``). Undisclosed rows are absent entirely.
        The pack proves the disclosed rows authentic - it does NOT prove
        the selection complete, and disclosed positions and count are
        themselves visible."""
        if not transitions:
            raise ValueError("a selective pack must disclose at least one transition")
        args = ["audit", "export", "--database-url", self.database_url]
        for transition in transitions:
            args.extend(["--transition", str(transition)])
        if tree_size is not None:
            args.extend(["--tree-size", str(tree_size)])
        return envelopes.SelectiveEvidencePack.from_json(self._json(*args))

    def audit_verify_pack_selective(
        self,
        pack_file: str,
        anchor_file: str | None = None,
        require_signatures: bool = False,
        *,
        require_signatures_from: int | None = None,
        require_signing_key: str | None = None,
        witnesses: bool = False,
        trusted_tsa_file: str | None = None,
    ) -> envelopes.PackVerificationReport[envelopes.SelectiveVerification]:
        """Verify a selective pack offline - no database. Returns the
        selective verdict; a row not included, anchor mismatch, or
        malformed pack is a decided verdict on stdout.
        ``require_signatures`` is compliance mode, as on
        ``audit_verify_pack``. The
        result is a ``PackVerificationReport``: this verdict, the login
        roles seen under a new OID among the pack's rows, and, with
        ``witnesses=True`` or a ``trusted_tsa_file``, what the pack's
        external witnesses prove."""
        return self._verify_pack(
            envelopes.parse_selective_verification,
            pack_file,
            anchor_file,
            require_signatures,
            require_signatures_from,
            require_signing_key,
            witnesses,
            trusted_tsa_file,
        )

    @staticmethod
    def _signature_policy_args(
        require_signatures: bool,
        require_signatures_from: int | None,
        require_signing_key: str | None,
    ) -> list[str]:
        args: list[str] = []
        if require_signatures:
            args.append("--require-signatures")
        if require_signatures_from is not None:
            args.extend(["--require-signatures-from", str(require_signatures_from)])
        if require_signing_key is not None:
            args.extend(["--require-signing-key", str(require_signing_key)])
        return args

    def _verify_pack(
        self,
        parse_verdict: Callable[[object], _Verdict],
        pack_file: str,
        anchor_file: str | None,
        require_signatures: bool,
        require_signatures_from: int | None,
        require_signing_key: str | None,
        witnesses: bool,
        trusted_tsa_file: str | None,
    ) -> envelopes.PackVerificationReport[_Verdict]:
        args = ["audit", "verify-pack", str(pack_file)]
        if anchor_file is not None:
            args.extend(["--anchor-file", str(anchor_file)])
        args.extend(
            self._signature_policy_args(
                require_signatures, require_signatures_from, require_signing_key
            )
        )
        if witnesses:
            args.append("--witnesses")
        if trusted_tsa_file is not None:
            args.extend(["--trusted-tsa-file", str(trusted_tsa_file)])
        return envelopes.PackVerificationReport.from_json(self._json(*args), parse_verdict)

    # ------------------------------------------------------------
    # The outbox lease protocol.
    # ------------------------------------------------------------

    def outbox_claim(
        self,
        intent_type: str,
        lease_seconds: int | None = None,
        worker_id: str | None = None,
    ) -> envelopes.OutboxRow | None:
        args = [
            "outbox", "claim",
            "--intent-type", intent_type,
            "--database-url", self.database_url,
        ]
        if lease_seconds is not None:
            args.extend(["--lease-seconds", str(lease_seconds)])
        if worker_id is not None:
            args.extend(["--worker-id", worker_id])
        return envelopes.parse_outbox_claim(self._json(*args))

    def outbox_complete(
        self,
        intent_id: str,
        worker_id: str,
        outcome: str = "delivered",
        retry_after_seconds: int | None = None,
        reason: str | None = None,
    ) -> envelopes.OutboxUpdate:
        args = [
            "outbox", "complete", intent_id,
            "--worker-id", worker_id,
            "--outcome", outcome,
            "--database-url", self.database_url,
        ]
        if retry_after_seconds is not None:
            args.extend(["--retry-after-seconds", str(retry_after_seconds)])
        if reason is not None:
            args.extend(["--reason", reason])
        return envelopes.OutboxUpdate.from_json(self._json(*args))

    def outbox_release(self, intent_id: str, worker_id: str) -> envelopes.OutboxUpdate:
        return envelopes.OutboxUpdate.from_json(
            self._json(
                "outbox", "release", intent_id,
                "--worker-id", worker_id,
                "--database-url", self.database_url,
            )
        )
