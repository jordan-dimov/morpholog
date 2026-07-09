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


class MorphologError(RuntimeError):
    """An operational failure from the CLI - distinct from a lawful
    business rejection, which is a decided outcome on stdout."""


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
        self, args: list[str], stdin: str | None = None, *, timeout: float | None
    ) -> "subprocess.CompletedProcess[str]":
        """Every invocation lands here. A timeout is operational, not a
        decided outcome, so it raises ``MorphologError``."""
        try:
            return subprocess.run(
                [self.binary, *args],
                capture_output=True,
                text=True,
                input=stdin,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            raise MorphologError(
                f"`{self.binary} {_redact_argv(args)}` timed out after {timeout}s"
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

    def propose_batch(
        self,
        rows: list,
        timeout: float | None = None,
        *,
        explain_on_reject: bool = False,
    ) -> list:
        """Admit many rows in one invocation (`propose --batch -`).

        Each row is a dict with ``transformation``, ``actor``, and one
        of ``args``/``args_named``. Returns one ``BatchReceipt`` per
        processed row; a non-zero exit is operational (the batch
        aborted) and raises with the receipts that did arrive named in
        the error. ``explain_on_reject`` attaches the same-snapshot why
        to every rejected row, as on ``propose``. ``timeout`` bounds
        this one call and defaults to unbounded, ignoring the
        client-wide timeout - a large import is the legitimate
        long-running case.
        """
        ndjson = "".join(json.dumps(row) + "\n" for row in rows)
        args = ["propose", self.file, "--batch", "-", "--database-url", self.database_url]
        if explain_on_reject:
            args.append("--explain-on-reject")
        proc = self._run(args, stdin=ndjson, timeout=timeout)
        receipts = [
            envelopes.BatchReceipt.from_json(json.loads(line))
            for line in proc.stdout.splitlines()
            if line.strip()
        ]
        if proc.returncode != 0:
            raise MorphologError(
                f"batch aborted after {len(receipts)} receipt(s):\n"
                f"{self._redact_stderr(proc.stderr)}"
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
        argv = ["inspect", "audit"]
        if after is not None:
            argv.extend(["--after", after])
        if named:
            argv.extend(["--named", self.file])
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

    # ------------------------------------------------------------
    # Tamper-evidence: replay, checkpoints, evidence packs.
    # ------------------------------------------------------------

    def verify(self, anchor_file: str | None = None) -> envelopes.VerifyReport:
        """Replay the audit log against the claims table and check the
        audit Merkle tree against its checkpoints (and an external
        ``anchor_file`` if given). A divergence or tamper is a decided
        verdict on stdout, not an operational error."""
        args = ["verify", "--database-url", self.database_url]
        if anchor_file is not None:
            args.extend(["--anchor-file", str(anchor_file)])
        return envelopes.VerifyReport.from_json(self._json(*args))

    def checkpoint(
        self, signing_key: str | None = None, key_id: str | None = None
    ) -> "envelopes.CheckpointCreated | envelopes.CheckpointNoNewRows":
        """Record a checkpoint over the current stable prefix, or return
        the unchanged head - either way a usable external anchor. Pass
        ``signing_key`` (a PKCS#8 PEM path) and ``key_id`` to sign the new
        tree head, so the anchor is attributable."""
        if (signing_key is None) != (key_id is None):
            raise ValueError("signing_key and key_id must be given together")
        args = ["checkpoint", "--database-url", self.database_url]
        if signing_key is not None:
            args.extend(["--signing-key", str(signing_key), "--key-id", str(key_id)])
        return envelopes.parse_checkpoint_outcome(self._json(*args))

    def evidence_export(self, tree_size: int | None = None) -> envelopes.EvidencePack:
        """Export a complete-prefix evidence pack covering the latest
        checkpoint, or the one at ``tree_size``. The pack carries the
        full audit prefix - confidential data, not selective
        disclosure."""
        args = ["evidence", "export", "--database-url", self.database_url]
        if tree_size is not None:
            args.extend(["--tree-size", str(tree_size)])
        return envelopes.EvidencePack.from_json(self._json(*args))

    def evidence_verify(
        self, pack_file: str, anchor_file: str | None = None
    ) -> envelopes.TreeVerification:
        """Verify a prefix evidence pack offline - no database. Returns the
        tamper-evidence verdict; a tamper or malformed pack is a decided
        verdict on stdout."""
        args = ["evidence", "verify", str(pack_file)]
        if anchor_file is not None:
            args.extend(["--anchor-file", str(anchor_file)])
        return envelopes.parse_tree_verification(self._json(*args))

    def evidence_export_window(
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
        args = ["evidence", "export", "--database-url", self.database_url]
        if from_anchor is not None:
            args.extend(["--from-anchor", str(from_anchor)])
        else:
            args.extend(["--from-tree-size", str(from_tree_size)])
        if to_tree_size is not None:
            args.extend(["--tree-size", str(to_tree_size)])
        return envelopes.WindowEvidencePack.from_json(self._json(*args))

    def evidence_verify_window(
        self, pack_file: str, anchor_file: str | None = None
    ) -> envelopes.WindowVerification:
        """Verify a window pack offline - no database. Returns the window
        verdict; a tamper, inconsistent extension, or malformed pack is a
        decided verdict on stdout."""
        args = ["evidence", "verify", str(pack_file)]
        if anchor_file is not None:
            args.extend(["--anchor-file", str(anchor_file)])
        return envelopes.parse_window_verification(self._json(*args))

    def evidence_export_selective(
        self,
        transitions: "list[str]",
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
        args = ["evidence", "export", "--database-url", self.database_url]
        for transition in transitions:
            args.extend(["--transition", str(transition)])
        if tree_size is not None:
            args.extend(["--tree-size", str(tree_size)])
        return envelopes.SelectiveEvidencePack.from_json(self._json(*args))

    def evidence_verify_selective(
        self, pack_file: str, anchor_file: str | None = None
    ) -> envelopes.SelectiveVerification:
        """Verify a selective pack offline - no database. Returns the
        selective verdict; a row not included, anchor mismatch, or
        malformed pack is a decided verdict on stdout."""
        args = ["evidence", "verify", str(pack_file)]
        if anchor_file is not None:
            args.extend(["--anchor-file", str(anchor_file)])
        return envelopes.parse_selective_verification(self._json(*args))

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
