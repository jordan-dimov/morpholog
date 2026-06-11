"""The subprocess adapter: one method per pinned CLI surface entry.

The load-bearing rule is output discrimination: every DECIDED result
arrives on stdout - a committed or rejected outcome (exit 1 flags the
rejection, but the receipt is still the result), a schema, an outbox
row, a check report. Empty stdout is the only operational failure,
and it raises ``MorphologError`` instead of returning an outcome.

This module never imports the generated ``models``; ``submit`` is
duck-typed on the two class attributes every generated request model
carries (``TRANSFORMATION`` and ``to_args_named``), so the static and
generated halves of the package meet only at that seam.
"""

from __future__ import annotations

import json
import os
import subprocess

from . import envelopes


class MorphologError(RuntimeError):
    """An operational failure from the CLI - distinct from a lawful
    business rejection, which is a decided outcome on stdout."""


class Morpholog:
    """A typed client over the ``morpholog`` CLI: arguments in, parsed
    envelope dataclasses out.

    ``binary`` resolves as: explicit argument, then the
    ``MORPHOLOG_BIN`` environment variable, then ``morpholog`` on
    ``PATH``.
    """

    def __init__(self, file: str, database_url: str, binary: str | None = None) -> None:
        self.file = str(file)
        self.database_url = database_url
        self.binary = binary or os.environ.get("MORPHOLOG_BIN", "morpholog")

    # ------------------------------------------------------------
    # The one subprocess seam.
    # ------------------------------------------------------------

    def _invoke(self, *args: str, stdin: str | None = None) -> str:
        proc = subprocess.run(
            [self.binary, *args],
            capture_output=True,
            text=True,
            input=stdin,
        )
        if not proc.stdout.strip():
            raise MorphologError(f"`{' '.join(args)}`:\n{proc.stderr.strip()}")
        return proc.stdout

    def _json(self, *args: str) -> object:
        return json.loads(self._invoke(*args))

    # ------------------------------------------------------------
    # Provisioning and model identity.
    # ------------------------------------------------------------

    def init(self, skip_if_exists: bool = False) -> envelopes.InitReport:
        args = ["init", "--database-url", self.database_url]
        if skip_if_exists:
            args.append("--skip-if-exists")
        return envelopes.InitReport.from_json(self._json(*args))

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
        args_named: dict,
        explain_on_reject: bool = False,
    ) -> "envelopes.Committed | envelopes.Rejected":
        """Propose a change by transformation name: it commits only if
        every rule holds; a refusal is a lawful outcome, returned as
        ``Rejected``."""
        args = [
            "propose", self.file, transformation,
            "--actor", actor,
            "--args-named", json.dumps({k: v for k, v in args_named.items()}),
            "--database-url", self.database_url,
        ]
        if explain_on_reject:
            args.append("--explain-on-reject")
        return envelopes.parse_run_outcome(self._json(*args))

    def submit(
        self, request: object, actor: str, explain_on_reject: bool = False
    ) -> "envelopes.Committed | envelopes.Rejected":
        """Commit a generated request model: its class names the
        transformation, its fields encode themselves."""
        return self.propose(
            request.TRANSFORMATION,  # type: ignore[attr-defined]
            actor,
            request.to_args_named(),  # type: ignore[attr-defined]
            explain_on_reject=explain_on_reject,
        )

    def propose_batch(self, rows: list) -> list:
        """Admit many rows in one invocation (`propose --batch -`).

        Each row is a dict with ``transformation``, ``actor``, and one
        of ``args``/``args_named``. Returns one ``BatchReceipt`` per
        processed row; a non-zero exit is operational (the batch
        aborted) and raises with the receipts that did arrive named in
        the error.
        """
        ndjson = "".join(json.dumps(row) + "\n" for row in rows)
        proc = subprocess.run(
            [
                self.binary,
                "propose", self.file,
                "--batch", "-",
                "--database-url", self.database_url,
            ],
            capture_output=True,
            text=True,
            input=ndjson,
        )
        receipts = [
            envelopes.BatchReceipt.from_json(json.loads(line))
            for line in proc.stdout.splitlines()
            if line.strip()
        ]
        if proc.returncode != 0:
            raise MorphologError(
                f"batch aborted after {len(receipts)} receipt(s):\n{proc.stderr.strip()}"
            )
        return receipts

    def explain(
        self, transformation: str, actor: str, args_named: dict
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

    def claims(self, *predicates: str, as_of: str | None = None) -> list:
        """The bare read: the claims table is the authority, an unknown
        predicate matches nothing. Tagged args decoded to bare values.

        `as_of` reads the claims as they were at a past moment - a
        transition id, or an RFC 3339 timestamp resolved to the last
        transition committed at or before it.
        """
        flags = [flag for p in predicates for flag in ("--predicate", p)]
        if as_of is not None:
            flags.extend(["--as-of", as_of])
        payload = self._json(
            "inspect", "claims", *flags, "--database-url", self.database_url
        )
        return [envelopes.ClaimInstance.from_json(c) for c in payload]

    def claims_named(self, *predicates: str, as_of: str | None = None) -> list:
        """The named read: the programme is the authority, skew is a
        hard error on the binary side. Values stay wire-true; the
        generated read models parse them by declared kind.

        `as_of` reads the claims as they were at a past moment - a
        transition id, or an RFC 3339 timestamp resolved to the last
        transition committed at or before it.
        """
        flags = [flag for p in predicates for flag in ("--predicate", p)]
        if as_of is not None:
            flags.extend(["--as-of", as_of])
        payload = self._json(
            "inspect", "claims", *flags,
            "--named", self.file,
            "--database-url", self.database_url,
        )
        return [envelopes.NamedClaim.from_json(c) for c in payload]

    def audit(self, after: str | None = None) -> list:
        """The audit tail: committed transitions in commit order, one
        ``AuditRow`` per NDJSON line. ``after`` resumes strictly after
        a previously seen transition id - lossless: rows whose writers
        were still in flight are withheld until the next call, never
        skipped. An empty tail is a lawful empty list."""
        return [
            envelopes.AuditRow.from_json(row)
            for row in self._audit_lines(after, named=False)
        ]

    def audit_named(self, after: str | None = None) -> list:
        """The audit tail with asserted/retracted claims decoded by
        declared field name under this programme's authority (skew is
        a hard error on the binary side). ``arguments`` and intent
        payloads stay positional - a different vocabulary."""
        return [
            envelopes.AuditRowNamed.from_json(row)
            for row in self._audit_lines(after, named=True)
        ]

    def _audit_lines(self, after: str | None, named: bool) -> list:
        # Not _invoke: an empty tail is a lawful empty stdout, not a
        # protocol violation - so the discrimination here is on the
        # exit code alone.
        argv = [self.binary, "inspect", "audit"]
        if after is not None:
            argv.extend(["--after", after])
        if named:
            argv.extend(["--named", self.file])
        argv.extend(["--database-url", self.database_url])
        proc = subprocess.run(argv, capture_output=True, text=True)
        if proc.returncode != 0:
            raise MorphologError(
                f"inspect audit failed (exit {proc.returncode}):\n{proc.stderr.strip()}"
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

    # ------------------------------------------------------------
    # The outbox lease protocol.
    # ------------------------------------------------------------

    def outbox_claim(
        self,
        intent_type: str,
        lease_seconds: int | None = None,
        worker_id: str | None = None,
    ) -> "envelopes.OutboxRow | None":
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
