"""Typed models for every machine-readable envelope the CLI prints.

One frozen dataclass per `$defs` entry of `morpholog schema --result`,
each with a strict ``from_json``: unknown keys raise, missing required
keys raise. That strictness is the drift tripwire - if a future binary
grows an envelope field this client does not know, the parse fails
loudly instead of silently dropping data.

Tagged values inside envelopes are decoded to bare Python values
(``decimal.Decimal``, ``datetime.date``, aware ``datetime``); named
claims keep their wire-true bare values, and the generated read models
in ``models.py`` parse those by declared kind.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime

from . import values


class EnvelopeError(ValueError):
    """An envelope that does not match the pinned contract."""


def _strict(name: str, payload: object, required: set, optional: set = frozenset()) -> dict:
    if not isinstance(payload, dict):
        raise EnvelopeError(f"{name}: expected an object, got {payload!r}")
    keys = set(payload)
    missing = required - keys
    unknown = keys - required - optional
    if missing:
        raise EnvelopeError(f"{name}: missing key(s) {sorted(missing)} in {payload!r}")
    if unknown:
        raise EnvelopeError(
            f"{name}: unknown key(s) {sorted(unknown)} - the binary's contract "
            f"has drifted past this generated client; regenerate it"
        )
    return payload


def _optional_timestamp(text: object) -> datetime | None:
    return None if text is None else values.parse_timestamp(str(text))


@dataclass(frozen=True)
class ClaimInstance:
    predicate: str
    args: list = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> "ClaimInstance":
        data = _strict("claim", payload, {"predicate", "args"})
        return cls(
            predicate=data["predicate"],
            args=[values.decode_tagged(a) for a in data["args"]],
        )


@dataclass(frozen=True)
class IntentInstance:
    name: str
    args: list = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> "IntentInstance":
        data = _strict("intent", payload, {"name", "args"})
        return cls(
            name=data["name"],
            args=[values.decode_tagged(a) for a in data["args"]],
        )


@dataclass(frozen=True)
class NamedClaim:
    """One row of the named read: bare values keyed by declared field."""

    predicate: str
    args: dict

    @classmethod
    def from_json(cls, payload: object) -> "NamedClaim":
        data = _strict("named claim", payload, {"predicate", "args"})
        return cls(predicate=data["predicate"], args=dict(data["args"]))


@dataclass(frozen=True)
class Committed:
    transition_id: str
    actor: str
    asserted_claims: list
    retracted_claims: list
    emitted_intents: list

    @classmethod
    def from_json(cls, payload: object) -> "Committed":
        data = _strict(
            "committed outcome",
            payload,
            {
                "status",
                "transition_id",
                "actor",
                "asserted_claims",
                "retracted_claims",
                "emitted_intents",
            },
        )
        return cls(
            transition_id=data["transition_id"],
            actor=str(values.decode_tagged(data["actor"])),
            asserted_claims=[ClaimInstance.from_json(c) for c in data["asserted_claims"]],
            retracted_claims=[ClaimInstance.from_json(c) for c in data["retracted_claims"]],
            emitted_intents=[IntentInstance.from_json(i) for i in data["emitted_intents"]],
        )


@dataclass(frozen=True)
class Rejected:
    reason: str
    explanation: "Explanation | None" = None

    @classmethod
    def from_json(cls, payload: object) -> "Rejected":
        data = _strict("rejected outcome", payload, {"status", "reason"}, {"explanation"})
        explanation = data.get("explanation")
        return cls(
            reason=data["reason"],
            explanation=None if explanation is None else Explanation.from_json(explanation),
        )


@dataclass(frozen=True)
class Errored:
    error: str

    @classmethod
    def from_json(cls, payload: object) -> "Errored":
        data = _strict("errored result", payload, {"status", "error"})
        return cls(error=data["error"])


def parse_run_outcome(payload: object) -> "Committed | Rejected":
    """The ``propose`` outcome envelope: a lawful business outcome
    either way. (The wire keeps the historical ``run_outcome`` name in
    the pinned schema; only the command verb changed.)"""
    status = payload.get("status") if isinstance(payload, dict) else None
    match status:
        case "committed":
            return Committed.from_json(payload)
        case "rejected":
            return Rejected.from_json(payload)
        case _:
            raise EnvelopeError(f"not a propose outcome: {payload!r}")


@dataclass(frozen=True)
class TracedEnvelope:
    result: "Committed | Rejected | Errored"
    trace: list

    @classmethod
    def from_json(cls, payload: object) -> "TracedEnvelope":
        data = _strict("traced envelope", payload, {"result", "trace"})
        result = data["result"]
        status = result.get("status") if isinstance(result, dict) else None
        parsed: Committed | Rejected | Errored
        if status == "errored":
            parsed = Errored.from_json(result)
        else:
            parsed = parse_run_outcome(result)
        # Trace entries are reserved internals; carried verbatim.
        return cls(result=parsed, trace=list(data["trace"]))


@dataclass(frozen=True)
class TransitionRef:
    transformation: str
    args: list
    actor: str

    @classmethod
    def from_json(cls, payload: object) -> "TransitionRef":
        data = _strict("transition ref", payload, {"transformation", "args", "actor"})
        return cls(
            transformation=data["transformation"],
            args=list(data["args"]),
            actor=data["actor"],
        )


@dataclass(frozen=True)
class MissingClaim:
    predicate: str
    rendered: str
    candidate_supplier_transformations: list

    @classmethod
    def from_json(cls, payload: object) -> "MissingClaim":
        data = _strict(
            "missing claim",
            payload,
            {"predicate", "rendered", "candidate_supplier_transformations"},
        )
        return cls(
            predicate=data["predicate"],
            rendered=data["rendered"],
            candidate_supplier_transformations=list(data["candidate_supplier_transformations"]),
        )


@dataclass(frozen=True)
class GateRejection:
    gate: str
    statement_kind: str
    directly_missing_claims: list

    @classmethod
    def from_json(cls, payload: object) -> "GateRejection":
        data = _strict(
            "gate rejection",
            payload,
            {"kind", "gate", "statement_kind", "directly_missing_claims"},
        )
        return cls(
            gate=data["gate"],
            statement_kind=data["statement_kind"],
            directly_missing_claims=[
                MissingClaim.from_json(m) for m in data["directly_missing_claims"]
            ],
        )


@dataclass(frozen=True)
class InvariantRejection:
    name: str
    rule: str

    @classmethod
    def from_json(cls, payload: object) -> "InvariantRejection":
        data = _strict("invariant rejection", payload, {"kind", "name", "rule"})
        return cls(name=data["name"], rule=data["rule"])


@dataclass(frozen=True)
class ErrorRejection:
    message: str

    @classmethod
    def from_json(cls, payload: object) -> "ErrorRejection":
        data = _strict("error rejection", payload, {"kind", "message"})
        return cls(message=data["message"])


def _parse_rejection(payload: object) -> "GateRejection | InvariantRejection | ErrorRejection":
    kind = payload.get("kind") if isinstance(payload, dict) else None
    match kind:
        case "gate":
            return GateRejection.from_json(payload)
        case "invariant":
            return InvariantRejection.from_json(payload)
        case "error":
            return ErrorRejection.from_json(payload)
        case _:
            raise EnvelopeError(f"unknown rejection kind in {payload!r}")


@dataclass(frozen=True)
class Explanation:
    transition: TransitionRef
    rejection: "GateRejection | InvariantRejection | ErrorRejection | None"

    @property
    def admissible(self) -> bool:
        return self.rejection is None

    @classmethod
    def from_json(cls, payload: object) -> "Explanation":
        data = _strict("explanation", payload, {"transition", "verdict"})
        verdict = data["verdict"]
        if verdict == "admissible":
            rejection = None
        else:
            wrapped = _strict("verdict", verdict, {"rejected"})
            rejection = _parse_rejection(wrapped["rejected"])
        return cls(transition=TransitionRef.from_json(data["transition"]), rejection=rejection)


@dataclass(frozen=True)
class BatchError:
    error: str


@dataclass(frozen=True)
class BatchReceipt:
    row: int
    outcome: "Committed | Rejected | BatchError"

    @classmethod
    def from_json(cls, payload: object) -> "BatchReceipt":
        if not isinstance(payload, dict) or "row" not in payload:
            raise EnvelopeError(f"not a batch receipt: {payload!r}")
        row = payload["row"]
        body = {k: v for k, v in payload.items() if k != "row"}
        if body.get("status") == "error":
            data = _strict("batch error receipt", body, {"status", "error"})
            return cls(row=row, outcome=BatchError(error=data["error"]))
        return cls(row=row, outcome=parse_run_outcome(body))


@dataclass(frozen=True)
class OutboxRow:
    intent_id: str
    transition_id: str
    intent_type: str
    arguments: list
    idempotency_key: str
    status: str
    attempt_count: int
    enqueued_at: datetime
    last_attempt_at: datetime | None
    delivered_at: datetime | None
    failed_at: datetime | None
    failure_reason: str | None
    next_attempt_at: datetime | None
    compensation_transition_id: str | None
    locked_by: str | None
    lock_expires_at: datetime | None

    @classmethod
    def from_json(cls, payload: object) -> "OutboxRow":
        data = _strict(
            "outbox row",
            payload,
            {
                "intent_id",
                "transition_id",
                "intent_type",
                "arguments",
                "idempotency_key",
                "status",
                "attempt_count",
                "enqueued_at",
                "last_attempt_at",
                "delivered_at",
                "failed_at",
                "failure_reason",
                "next_attempt_at",
                "compensation_transition_id",
                "locked_by",
                "lock_expires_at",
            },
        )
        return cls(
            intent_id=data["intent_id"],
            transition_id=data["transition_id"],
            intent_type=data["intent_type"],
            arguments=[values.decode_tagged(a) for a in data["arguments"]],
            idempotency_key=data["idempotency_key"],
            status=data["status"],
            attempt_count=data["attempt_count"],
            enqueued_at=values.parse_timestamp(data["enqueued_at"]),
            last_attempt_at=_optional_timestamp(data["last_attempt_at"]),
            delivered_at=_optional_timestamp(data["delivered_at"]),
            failed_at=_optional_timestamp(data["failed_at"]),
            failure_reason=data["failure_reason"],
            next_attempt_at=_optional_timestamp(data["next_attempt_at"]),
            compensation_transition_id=data["compensation_transition_id"],
            locked_by=data["locked_by"],
            lock_expires_at=_optional_timestamp(data["lock_expires_at"]),
        )


def parse_outbox_claim(payload: object) -> "OutboxRow | None":
    data = _strict("outbox claim", payload, {"row"})
    row = data["row"]
    return None if row is None else OutboxRow.from_json(row)


@dataclass(frozen=True)
class OutboxUpdate:
    status: str

    @property
    def applied(self) -> bool:
        return self.status == "applied"

    @classmethod
    def from_json(cls, payload: object) -> "OutboxUpdate":
        data = _strict("outbox update", payload, {"status"})
        return cls(status=data["status"])


@dataclass(frozen=True)
class AuditedInvariantCheck:
    """One invariant that governed an admission: name plus the
    version active at commit time."""

    name: str
    version: int

    @classmethod
    def from_json(cls, payload: object) -> "AuditedInvariantCheck":
        data = _strict("audited invariant check", payload, {"name", "version"})
        return cls(name=data["name"], version=data["version"])


_AUDIT_ROW_KEYS = {
    "transition_id",
    "transformation_name",
    "arguments",
    "actor",
    "invariant_epoch",
    "invariants_checked",
    "asserted_claims",
    "retracted_claims",
    "emitted_intents",
    "committed_at",
}

_AUDIT_ROW_OPTIONAL_KEYS = {"attestation"}


@dataclass(frozen=True)
class Attestation:
    """How the actor identity on an audit row was established. Gateway
    mode records which PostgreSQL-authenticated role asserted the
    actor; it proves who asserted, never that the named actor
    authorised anything. Rows written before attestation existed
    carry none."""

    mode: str
    authenticated_by: str

    @classmethod
    def from_json(cls, payload: object) -> "Attestation":
        data = _strict("attestation", payload, {"mode", "authenticated_by"})
        if data["mode"] != "gateway":
            raise EnvelopeError(
                f"attestation: unknown mode {data['mode']!r} - the binary's "
                "contract has drifted past this generated client; regenerate it"
            )
        return cls(mode=data["mode"], authenticated_by=data["authenticated_by"])


def _attestation_of(data: dict) -> "Attestation | None":
    raw = data.get("attestation")
    return None if raw is None else Attestation.from_json(raw)


@dataclass(frozen=True)
class AuditRow:
    """One committed transition from the audit tail (`inspect
    audit`): who proposed what, which rules governed the admission,
    and what was asserted, retracted, and emitted. Claim and intent
    arrays carry decoded positional values; see `AuditRowNamed` for
    the field-keyed claim decode."""

    transition_id: str
    transformation_name: str
    arguments: list
    actor: str
    invariant_epoch: int
    invariants_checked: list
    asserted_claims: list
    retracted_claims: list
    emitted_intents: list
    committed_at: datetime
    attestation: "Attestation | None" = None

    @classmethod
    def from_json(cls, payload: object) -> "AuditRow":
        data = _strict("audit row", payload, _AUDIT_ROW_KEYS, optional=_AUDIT_ROW_OPTIONAL_KEYS)
        return cls(
            transition_id=data["transition_id"],
            transformation_name=data["transformation_name"],
            arguments=[values.decode_tagged(a) for a in data["arguments"]],
            actor=str(values.decode_tagged(data["actor"])),
            invariant_epoch=data["invariant_epoch"],
            invariants_checked=[
                AuditedInvariantCheck.from_json(c) for c in data["invariants_checked"]
            ],
            asserted_claims=[ClaimInstance.from_json(c) for c in data["asserted_claims"]],
            retracted_claims=[ClaimInstance.from_json(c) for c in data["retracted_claims"]],
            emitted_intents=[IntentInstance.from_json(i) for i in data["emitted_intents"]],
            committed_at=values.parse_timestamp(data["committed_at"]),
            attestation=_attestation_of(data),
        )


@dataclass(frozen=True)
class AuditRowNamed:
    """`AuditRow` with the asserted/retracted claims decoded by
    declared field name (the `--named` tail). `arguments` and
    `emitted_intents` stay positional - they belong to the
    transformation/intent vocabularies, not predicate declarations."""

    transition_id: str
    transformation_name: str
    arguments: list
    actor: str
    invariant_epoch: int
    invariants_checked: list
    asserted_claims: list
    retracted_claims: list
    emitted_intents: list
    committed_at: datetime
    attestation: "Attestation | None" = None

    @classmethod
    def from_json(cls, payload: object) -> "AuditRowNamed":
        data = _strict("named audit row", payload, _AUDIT_ROW_KEYS, optional=_AUDIT_ROW_OPTIONAL_KEYS)
        return cls(
            transition_id=data["transition_id"],
            transformation_name=data["transformation_name"],
            arguments=[values.decode_tagged(a) for a in data["arguments"]],
            actor=str(values.decode_tagged(data["actor"])),
            invariant_epoch=data["invariant_epoch"],
            invariants_checked=[
                AuditedInvariantCheck.from_json(c) for c in data["invariants_checked"]
            ],
            asserted_claims=[NamedClaim.from_json(c) for c in data["asserted_claims"]],
            retracted_claims=[NamedClaim.from_json(c) for c in data["retracted_claims"]],
            emitted_intents=[IntentInstance.from_json(i) for i in data["emitted_intents"]],
            committed_at=values.parse_timestamp(data["committed_at"]),
            attestation=_attestation_of(data),
        )


@dataclass(frozen=True)
class InvariantCoverage:
    """Coverage of one invariant: did its condition ever match, and
    did it ever refuse a real proposal? The verdicts, strongest
    first - `constrained` (refused at least one proposal, per the
    operational rejection log; a floor, not a census), `fired`,
    `never_fired` (its condition never matched anything), `always_on`
    (a prohibition with no recorded refusals yet)."""

    invariant: str
    verdict: str
    transitions_fired: int
    from_clause: str | None = None
    first_fired: str | None = None
    last_fired: str | None = None
    proposals_refused: int = 0
    first_refused: str | None = None
    last_refused: str | None = None
    not_in_programme: bool = False

    @classmethod
    def from_json(cls, payload: object) -> "InvariantCoverage":
        data = _strict(
            "invariant coverage",
            payload,
            {"invariant", "verdict", "transitions_fired"},
            {
                "from",
                "first_fired",
                "last_fired",
                "proposals_refused",
                "first_refused",
                "last_refused",
                "not_in_programme",
            },
        )
        return cls(
            invariant=data["invariant"],
            verdict=data["verdict"],
            transitions_fired=data["transitions_fired"],
            # `from` is a Python keyword; the wire name maps to
            # `from_clause` on this side only.
            from_clause=data.get("from"),
            first_fired=data.get("first_fired"),
            last_fired=data.get("last_fired"),
            proposals_refused=data.get("proposals_refused", 0),
            first_refused=data.get("first_refused"),
            last_refused=data.get("last_refused"),
            not_in_programme=data.get("not_in_programme", False),
        )


@dataclass(frozen=True)
class TransformationUsage:
    transformation: str
    transitions: int
    first: str | None = None
    last: str | None = None
    proposals_refused: int = 0
    not_in_programme: bool = False

    @classmethod
    def from_json(cls, payload: object) -> "TransformationUsage":
        data = _strict(
            "transformation usage",
            payload,
            {"transformation", "transitions"},
            {"first", "last", "proposals_refused", "not_in_programme"},
        )
        return cls(
            transformation=data["transformation"],
            transitions=data["transitions"],
            first=data.get("first"),
            last=data.get("last"),
            proposals_refused=data.get("proposals_refused", 0),
            not_in_programme=data.get("not_in_programme", False),
        )


@dataclass(frozen=True)
class CoverageReport:
    """Which rules have ever actually done work - and which have
    demonstrably refused - over replayed committed history plus the
    operational rejection log."""

    program: str
    transitions_replayed: int
    rejections_replayed: int
    invariants: list
    transformations: list

    @classmethod
    def from_json(cls, payload: object) -> "CoverageReport":
        data = _strict(
            "coverage report",
            payload,
            {
                "program",
                "transitions_replayed",
                "rejections_replayed",
                "invariants",
                "transformations",
            },
        )
        return cls(
            program=data["program"],
            transitions_replayed=data["transitions_replayed"],
            rejections_replayed=data["rejections_replayed"],
            invariants=[InvariantCoverage.from_json(i) for i in data["invariants"]],
            transformations=[
                TransformationUsage.from_json(t) for t in data["transformations"]
            ],
        )


@dataclass(frozen=True)
class Diagnostic:
    severity: str
    message: str
    start: int | None = None
    end: int | None = None
    line: int | None = None
    column: int | None = None

    @classmethod
    def from_json(cls, payload: object) -> "Diagnostic":
        data = _strict(
            "diagnostic",
            payload,
            {"severity", "message"},
            {"start", "end", "line", "column"},
        )
        return cls(
            severity=data["severity"],
            message=data["message"],
            start=data.get("start"),
            end=data.get("end"),
            line=data.get("line"),
            column=data.get("column"),
        )


@dataclass(frozen=True)
class CheckReport:
    file: str
    diagnostics: list

    @classmethod
    def from_json(cls, payload: object) -> "CheckReport":
        data = _strict("check report", payload, {"file", "diagnostics"})
        return cls(
            file=data["file"],
            diagnostics=[Diagnostic.from_json(d) for d in data["diagnostics"]],
        )


@dataclass(frozen=True)
class HashReport:
    program: str
    hash: str

    @classmethod
    def from_json(cls, payload: object) -> "HashReport":
        data = _strict("hash report", payload, {"program", "hash"})
        return cls(program=data["program"], hash=data["hash"])


@dataclass(frozen=True)
class LeastPrivilege:
    """The `--least-privilege` floor as applied: the two group roles,
    plus the membership grants only the operator can decide."""

    next_steps: tuple[str, ...]
    reader_role: str
    writer_role: str

    @classmethod
    def from_json(cls, payload: object) -> "LeastPrivilege":
        data = _strict(
            "least privilege", payload, {"next_steps", "reader_role", "writer_role"}
        )
        return cls(
            next_steps=tuple(data["next_steps"]),
            reader_role=data["reader_role"],
            writer_role=data["writer_role"],
        )


@dataclass(frozen=True)
class InitReport:
    status: str
    schema: str
    least_privilege: LeastPrivilege | None = None

    @classmethod
    def from_json(cls, payload: object) -> "InitReport":
        data = _strict(
            "init report", payload, {"status", "schema"}, optional={"least_privilege"}
        )
        floor = data.get("least_privilege")
        return cls(
            status=data["status"],
            schema=data["schema"],
            least_privilege=None if floor is None else LeastPrivilege.from_json(floor),
        )


# ------------------------------------------------------------
# Tamper-evidence: verify / checkpoint / evidence pack.
# ------------------------------------------------------------


@dataclass(frozen=True)
class ReplayConsistent:
    """Replaying the audit log reproduces the claims table exactly."""

    transitions: int
    claims: int

    @classmethod
    def from_json(cls, payload: object) -> "ReplayConsistent":
        data = _strict("consistent replay", payload, {"status", "transitions", "claims"})
        return cls(transitions=data["transitions"], claims=data["claims"])


@dataclass(frozen=True)
class ReplayDivergent:
    """The claims table and the audit log disagree - evidence one was
    edited out of band."""

    only_in_claims_table: list
    only_in_replay: list

    @classmethod
    def from_json(cls, payload: object) -> "ReplayDivergent":
        data = _strict(
            "divergent replay", payload, {"status", "only_in_claims_table", "only_in_replay"}
        )
        return cls(
            only_in_claims_table=[ClaimInstance.from_json(c) for c in data["only_in_claims_table"]],
            only_in_replay=[ClaimInstance.from_json(c) for c in data["only_in_replay"]],
        )


def parse_verify_outcome(payload: object) -> "ReplayConsistent | ReplayDivergent":
    status = payload.get("status") if isinstance(payload, dict) else None
    match status:
        case "consistent":
            return ReplayConsistent.from_json(payload)
        case "divergent":
            return ReplayDivergent.from_json(payload)
        case _:
            raise EnvelopeError(f"not a replay verdict: {payload!r}")


@dataclass(frozen=True)
class TreeIntact:
    checkpoints: int
    tree_size: int

    @classmethod
    def from_json(cls, payload: object) -> "TreeIntact":
        data = _strict("intact tree", payload, {"status", "checkpoints", "tree_size"})
        return cls(checkpoints=data["checkpoints"], tree_size=data["tree_size"])


@dataclass(frozen=True)
class TreeTampered:
    tree_size: int
    recorded_root: str
    recomputed_root: str

    @classmethod
    def from_json(cls, payload: object) -> "TreeTampered":
        data = _strict(
            "tampered tree", payload, {"status", "tree_size", "recorded_root", "recomputed_root"}
        )
        return cls(
            tree_size=data["tree_size"],
            recorded_root=data["recorded_root"],
            recomputed_root=data["recomputed_root"],
        )


@dataclass(frozen=True)
class TreeChainBroken:
    detail: str

    @classmethod
    def from_json(cls, payload: object) -> "TreeChainBroken":
        data = _strict("chain-broken tree", payload, {"status", "detail"})
        return cls(detail=data["detail"])


@dataclass(frozen=True)
class TreeAnchorMismatch:
    tree_size: int
    anchor_checkpoint_hash: str
    stored_checkpoint_hash: str | None

    @classmethod
    def from_json(cls, payload: object) -> "TreeAnchorMismatch":
        data = _strict(
            "anchor-mismatch tree",
            payload,
            {"status", "tree_size", "anchor_checkpoint_hash", "stored_checkpoint_hash"},
        )
        return cls(
            tree_size=data["tree_size"],
            anchor_checkpoint_hash=data["anchor_checkpoint_hash"],
            stored_checkpoint_hash=data["stored_checkpoint_hash"],
        )


@dataclass(frozen=True)
class TreeMalformedPack:
    """An evidence pack could not be parsed into a checkable tree
    (offline `evidence verify` only)."""

    detail: str

    @classmethod
    def from_json(cls, payload: object) -> "TreeMalformedPack":
        data = _strict("malformed pack", payload, {"status", "detail"})
        return cls(detail=data["detail"])


@dataclass(frozen=True)
class TreeSignatureInvalid:
    """A checkpoint carries a signature that does not verify over its tree
    head - corruption, or a signed checkpoint altered without re-signing."""

    tree_size: int
    key_id: str
    purpose: str
    public_key: str

    @classmethod
    def from_json(cls, payload: object) -> "TreeSignatureInvalid":
        data = _strict(
            "signature-invalid tree",
            payload,
            {"status", "tree_size", "key_id", "purpose", "public_key"},
        )
        return cls(
            tree_size=data["tree_size"],
            key_id=data["key_id"],
            purpose=data["purpose"],
            public_key=data["public_key"],
        )


@dataclass(frozen=True)
class TreeUnauthorizedKey:
    """A checkpoint carries a genuine signature, but the signing key was
    not authorised (no admitted `AuditSigningKey` for that exact triple)
    as of the checkpoint's prefix."""

    tree_size: int
    key_id: str
    purpose: str
    public_key: str

    @classmethod
    def from_json(cls, payload: object) -> "TreeUnauthorizedKey":
        data = _strict(
            "unauthorized-key tree",
            payload,
            {"status", "tree_size", "key_id", "purpose", "public_key"},
        )
        return cls(
            tree_size=data["tree_size"],
            key_id=data["key_id"],
            purpose=data["purpose"],
            public_key=data["public_key"],
        )


@dataclass(frozen=True)
class TreeSignatureRequired:
    """`--require-signatures` was asked for and this checkpoint is
    unsigned. A compliance-policy verdict, not an intrinsic tamper."""

    tree_size: int

    @classmethod
    def from_json(cls, payload: object) -> "TreeSignatureRequired":
        data = _strict("signature-required tree", payload, {"status", "tree_size"})
        return cls(tree_size=data["tree_size"])


TreeVerification = (
    TreeIntact
    | TreeTampered
    | TreeChainBroken
    | TreeAnchorMismatch
    | TreeMalformedPack
    | TreeSignatureInvalid
    | TreeUnauthorizedKey
    | TreeSignatureRequired
)


def parse_tree_verification(payload: object) -> TreeVerification:
    """The tamper-evidence verdict, the output of `evidence verify` and
    the `tree` half of `verify`."""
    status = payload.get("status") if isinstance(payload, dict) else None
    match status:
        case "intact":
            return TreeIntact.from_json(payload)
        case "tampered":
            return TreeTampered.from_json(payload)
        case "chain_broken":
            return TreeChainBroken.from_json(payload)
        case "anchor_mismatch":
            return TreeAnchorMismatch.from_json(payload)
        case "malformed_pack":
            return TreeMalformedPack.from_json(payload)
        case "signature_invalid":
            return TreeSignatureInvalid.from_json(payload)
        case "unauthorized_key":
            return TreeUnauthorizedKey.from_json(payload)
        case "signature_required":
            return TreeSignatureRequired.from_json(payload)
        case _:
            raise EnvelopeError(f"not a tree verdict: {payload!r}")


@dataclass(frozen=True)
class ViewsIntact:
    """Every catalogued view's live definition matches its seal."""

    views_checked: int

    @classmethod
    def from_json(cls, payload: object) -> "ViewsIntact":
        data = _strict("intact views", payload, {"status", "views_checked"})
        return cls(views_checked=data["views_checked"])


@dataclass(frozen=True)
class ViewsTampered:
    """The view surface disagrees with its seal: `mismatched` views were
    redefined in place, `missing` views lack a seal row or a live
    definition."""

    mismatched: list
    missing: list

    @classmethod
    def from_json(cls, payload: object) -> "ViewsTampered":
        data = _strict("tampered views", payload, {"status", "mismatched", "missing"})
        return cls(mismatched=list(data["mismatched"]), missing=list(data["missing"]))


@dataclass(frozen=True)
class ViewsNotSealed:
    """No seal table in the schema: the views predate sealing or were
    never applied. Visible, not a failure."""

    @classmethod
    def from_json(cls, payload: object) -> "ViewsNotSealed":
        _strict("unsealed views", payload, {"status"})
        return cls()


ViewsVerification = ViewsIntact | ViewsTampered | ViewsNotSealed


def parse_views_verification(payload: object) -> ViewsVerification:
    """Parse a `views` verdict by its `status` tag."""
    if not isinstance(payload, dict):
        raise EnvelopeError(f"not a views verdict: {payload!r}")
    match payload.get("status"):
        case "intact":
            return ViewsIntact.from_json(payload)
        case "tampered":
            return ViewsTampered.from_json(payload)
        case "not_sealed":
            return ViewsNotSealed.from_json(payload)
        case _:
            raise EnvelopeError(f"not a views verdict: {payload!r}")


@dataclass(frozen=True)
class VerifyReport:
    """The `verify` envelope: the replay verdict beside the
    tamper-evidence verdict, plus the generated-view-surface verdict
    when the verifier asked for it (`--views-schema`)."""

    replay: ReplayConsistent | ReplayDivergent
    tree: TreeVerification
    views: ViewsVerification | None = None

    @classmethod
    def from_json(cls, payload: object) -> "VerifyReport":
        data = _strict("verify report", payload, {"replay", "tree"}, optional={"views"})
        views = data.get("views")
        return cls(
            replay=parse_verify_outcome(data["replay"]),
            tree=parse_tree_verification(data["tree"]),
            views=None if views is None else parse_views_verification(views),
        )


@dataclass(frozen=True)
class TreeHeadSignature:
    """One Ed25519 attestation over a tree head: who signed it (`key_id`
    + `public_key`), what the key is authorised for (`purpose`), and the
    signature - the latter two rendered `ed25519-pub:`/`ed25519-sig:`."""

    key_id: str
    purpose: str
    public_key: str
    signature: str

    @classmethod
    def from_json(cls, payload: object) -> "TreeHeadSignature":
        data = _strict(
            "tree-head signature", payload, {"key_id", "purpose", "public_key", "signature"}
        )
        return cls(
            key_id=data["key_id"],
            purpose=data["purpose"],
            public_key=data["public_key"],
            signature=data["signature"],
        )


def _parse_signatures(data: dict) -> list:
    raw = data.get("signatures", [])
    if not isinstance(raw, list):
        raise EnvelopeError(f"`signatures` must be a list, got {raw!r}")
    return [TreeHeadSignature.from_json(s) for s in raw]


@dataclass(frozen=True)
class Checkpoint:
    """A signed-tree-head commitment to a prefix of the audit log; held
    externally, it is the anchor `verify`/`evidence verify` check
    against. `signatures` is empty (and omitted from JSON) when the
    checkpoint is unsigned."""

    tree_size: int
    root_hash: str
    prev_checkpoint_hash: str | None
    checkpoint_hash: str
    signatures: list = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> "Checkpoint":
        data = _strict(
            "checkpoint",
            payload,
            {"tree_size", "root_hash", "prev_checkpoint_hash", "checkpoint_hash"},
            {"signatures"},
        )
        return cls(
            tree_size=data["tree_size"],
            root_hash=data["root_hash"],
            prev_checkpoint_hash=data["prev_checkpoint_hash"],
            checkpoint_hash=data["checkpoint_hash"],
            signatures=_parse_signatures(data),
        )


def _checkpoint_from_flattened(name: str, payload: object) -> Checkpoint:
    # The `checkpoint` command flattens the checkpoint fields beside a
    # `status` tag, so the bare-checkpoint parser (which forbids
    # `status`) cannot read it directly.
    data = _strict(
        name,
        payload,
        {"status", "tree_size", "root_hash", "prev_checkpoint_hash", "checkpoint_hash"},
        {"signatures"},
    )
    return Checkpoint(
        tree_size=data["tree_size"],
        root_hash=data["root_hash"],
        prev_checkpoint_hash=data["prev_checkpoint_hash"],
        checkpoint_hash=data["checkpoint_hash"],
        signatures=_parse_signatures(data),
    )


@dataclass(frozen=True)
class CheckpointCreated:
    checkpoint: Checkpoint

    @classmethod
    def from_json(cls, payload: object) -> "CheckpointCreated":
        return cls(checkpoint=_checkpoint_from_flattened("created checkpoint", payload))


@dataclass(frozen=True)
class CheckpointNoNewRows:
    """The stable prefix had not grown; the current head, returned
    unchanged - still a usable anchor."""

    checkpoint: Checkpoint

    @classmethod
    def from_json(cls, payload: object) -> "CheckpointNoNewRows":
        return cls(checkpoint=_checkpoint_from_flattened("no-new-rows checkpoint", payload))


def parse_checkpoint_outcome(payload: object) -> "CheckpointCreated | CheckpointNoNewRows":
    status = payload.get("status") if isinstance(payload, dict) else None
    match status:
        case "created":
            return CheckpointCreated.from_json(payload)
        case "no_new_rows":
            return CheckpointNoNewRows.from_json(payload)
        case _:
            raise EnvelopeError(f"not a checkpoint outcome: {payload!r}")


@dataclass(frozen=True)
class PackManifest:
    pack_format_version: int
    tree_size: int
    root_hash: str
    checkpoint_hash: str

    @classmethod
    def from_json(cls, payload: object) -> "PackManifest":
        data = _strict(
            "pack manifest",
            payload,
            {"pack_format_version", "tree_size", "root_hash", "checkpoint_hash"},
        )
        return cls(
            pack_format_version=data["pack_format_version"],
            tree_size=data["tree_size"],
            root_hash=data["root_hash"],
            checkpoint_hash=data["checkpoint_hash"],
        )


@dataclass(frozen=True)
class EvidencePack:
    """A portable, offline-verifiable export of a checkpointed prefix of
    the audit log: the covering checkpoint chain and the covered rows."""

    manifest: PackManifest
    checkpoints: list
    rows: list

    @classmethod
    def from_json(cls, payload: object) -> "EvidencePack":
        data = _strict("evidence pack", payload, {"manifest", "checkpoints", "rows"})
        return cls(
            manifest=PackManifest.from_json(data["manifest"]),
            checkpoints=[Checkpoint.from_json(c) for c in data["checkpoints"]],
            rows=[AuditRow.from_json(r) for r in data["rows"]],
        )


@dataclass(frozen=True)
class WindowPackManifest:
    pack_format_version: int
    pack_kind: str
    from_tree_size: int
    to_tree_size: int
    from_checkpoint_hash: str
    to_checkpoint_hash: str
    from_root_hash: str
    to_root_hash: str

    @classmethod
    def from_json(cls, payload: object) -> "WindowPackManifest":
        data = _strict(
            "window pack manifest",
            payload,
            {
                "pack_format_version",
                "pack_kind",
                "from_tree_size",
                "to_tree_size",
                "from_checkpoint_hash",
                "to_checkpoint_hash",
                "from_root_hash",
                "to_root_hash",
            },
        )
        return cls(
            pack_format_version=data["pack_format_version"],
            pack_kind=data["pack_kind"],
            from_tree_size=data["from_tree_size"],
            to_tree_size=data["to_tree_size"],
            from_checkpoint_hash=data["from_checkpoint_hash"],
            to_checkpoint_hash=data["to_checkpoint_hash"],
            from_root_hash=data["from_root_hash"],
            to_root_hash=data["to_root_hash"],
        )


def _str_list(label: str, value: object) -> list:
    """A list of strings, or an `EnvelopeError`. The schema pins proof and
    consistency-proof hashes as string arrays; a bare string would otherwise
    pass `list(...)` as a list of characters."""
    if not isinstance(value, list) or not all(isinstance(x, str) for x in value):
        raise EnvelopeError(f"`{label}` must be a list of strings, got {value!r}")
    return list(value)


@dataclass(frozen=True)
class RowInclusionProof:
    """One window row's inclusion proof: the row sits at ``leaf_index`` in
    the to-checkpoint's tree, proven by ``proof`` (sibling hashes)."""

    leaf_index: int
    proof: list

    @classmethod
    def from_json(cls, payload: object) -> "RowInclusionProof":
        data = _strict("row inclusion proof", payload, {"leaf_index", "proof"})
        return cls(leaf_index=data["leaf_index"], proof=_str_list("proof", data["proof"]))


@dataclass(frozen=True)
class WindowEvidencePack:
    """A windowed evidence pack: the interval [from, to) of the audit log,
    proven a faithful append-only continuation of an earlier checkpoint by
    a consistency proof plus one inclusion proof per row."""

    manifest: WindowPackManifest
    from_checkpoint: Checkpoint
    to_checkpoint: Checkpoint
    consistency_proof: list
    rows: list
    inclusion_proofs: list

    @classmethod
    def from_json(cls, payload: object) -> "WindowEvidencePack":
        data = _strict(
            "window evidence pack",
            payload,
            {
                "manifest",
                "from_checkpoint",
                "to_checkpoint",
                "consistency_proof",
                "rows",
                "inclusion_proofs",
            },
        )
        return cls(
            manifest=WindowPackManifest.from_json(data["manifest"]),
            from_checkpoint=Checkpoint.from_json(data["from_checkpoint"]),
            to_checkpoint=Checkpoint.from_json(data["to_checkpoint"]),
            consistency_proof=_str_list("consistency_proof", data["consistency_proof"]),
            rows=[AuditRow.from_json(r) for r in data["rows"]],
            inclusion_proofs=[RowInclusionProof.from_json(p) for p in data["inclusion_proofs"]],
        )


@dataclass(frozen=True)
class WindowIntact:
    from_tree_size: int
    to_tree_size: int
    rows: int

    @classmethod
    def from_json(cls, payload: object) -> "WindowIntact":
        data = _strict(
            "intact window", payload, {"status", "from_tree_size", "to_tree_size", "rows"}
        )
        return cls(
            from_tree_size=data["from_tree_size"],
            to_tree_size=data["to_tree_size"],
            rows=data["rows"],
        )


@dataclass(frozen=True)
class WindowInconsistentExtension:
    """The later checkpoint is not an append-only extension of the earlier
    one - the prior period was altered."""

    from_tree_size: int
    to_tree_size: int

    @classmethod
    def from_json(cls, payload: object) -> "WindowInconsistentExtension":
        data = _strict(
            "inconsistent-extension window", payload, {"status", "from_tree_size", "to_tree_size"}
        )
        return cls(from_tree_size=data["from_tree_size"], to_tree_size=data["to_tree_size"])


@dataclass(frozen=True)
class WindowRowNotIncluded:
    """A window row is not included at its declared position in the later
    checkpoint - the exported rows are not the genuine suffix."""

    leaf_index: int

    @classmethod
    def from_json(cls, payload: object) -> "WindowRowNotIncluded":
        data = _strict("row-not-included window", payload, {"status", "leaf_index"})
        return cls(leaf_index=data["leaf_index"])


@dataclass(frozen=True)
class WindowAnchorMismatch:
    """An externally held anchor disagrees with the pack's from-checkpoint."""

    tree_size: int
    anchor_checkpoint_hash: str
    pack_checkpoint_hash: str

    @classmethod
    def from_json(cls, payload: object) -> "WindowAnchorMismatch":
        data = _strict(
            "anchor-mismatch window",
            payload,
            {"status", "tree_size", "anchor_checkpoint_hash", "pack_checkpoint_hash"},
        )
        return cls(
            tree_size=data["tree_size"],
            anchor_checkpoint_hash=data["anchor_checkpoint_hash"],
            pack_checkpoint_hash=data["pack_checkpoint_hash"],
        )


@dataclass(frozen=True)
class WindowSignatureInvalid:
    """The to-checkpoint carries a signature that does not verify over its
    tree head (cryptographic check only; authority is not judged here)."""

    tree_size: int
    key_id: str
    purpose: str
    public_key: str

    @classmethod
    def from_json(cls, payload: object) -> "WindowSignatureInvalid":
        data = _strict(
            "signature-invalid window",
            payload,
            {"status", "tree_size", "key_id", "purpose", "public_key"},
        )
        return cls(
            tree_size=data["tree_size"],
            key_id=data["key_id"],
            purpose=data["purpose"],
            public_key=data["public_key"],
        )


@dataclass(frozen=True)
class WindowSignatureRequired:
    """``--require-signatures`` was asked for and the to-checkpoint is
    unsigned. A compliance-policy verdict, not an intrinsic tamper."""

    tree_size: int

    @classmethod
    def from_json(cls, payload: object) -> "WindowSignatureRequired":
        data = _strict("signature-required window", payload, {"status", "tree_size"})
        return cls(tree_size=data["tree_size"])


@dataclass(frozen=True)
class WindowMalformed:
    """The window pack is not a well-formed v2 artefact - it never had a
    chance to prove anything."""

    detail: str

    @classmethod
    def from_json(cls, payload: object) -> "WindowMalformed":
        data = _strict("malformed window", payload, {"status", "detail"})
        return cls(detail=data["detail"])


WindowVerification = (
    WindowIntact
    | WindowInconsistentExtension
    | WindowRowNotIncluded
    | WindowAnchorMismatch
    | WindowSignatureInvalid
    | WindowSignatureRequired
    | WindowMalformed
)


def parse_window_verification(payload: object) -> WindowVerification:
    """The windowed-pack verdict, the output of ``evidence verify`` on a v2
    window pack."""
    status = payload.get("status") if isinstance(payload, dict) else None
    match status:
        case "intact":
            return WindowIntact.from_json(payload)
        case "inconsistent_extension":
            return WindowInconsistentExtension.from_json(payload)
        case "row_not_included":
            return WindowRowNotIncluded.from_json(payload)
        case "anchor_mismatch":
            return WindowAnchorMismatch.from_json(payload)
        case "signature_invalid":
            return WindowSignatureInvalid.from_json(payload)
        case "signature_required":
            return WindowSignatureRequired.from_json(payload)
        case "malformed":
            return WindowMalformed.from_json(payload)
        case _:
            raise EnvelopeError(f"not a window verdict: {payload!r}")


@dataclass(frozen=True)
class SelectivePackManifest:
    """The selective pack's header: the covering checkpoint's coordinates."""

    pack_format_version: int
    pack_kind: str
    tree_size: int
    root_hash: str
    checkpoint_hash: str

    @classmethod
    def from_json(cls, payload: object) -> "SelectivePackManifest":
        data = _strict(
            "selective pack manifest",
            payload,
            {"pack_format_version", "pack_kind", "tree_size", "root_hash", "checkpoint_hash"},
        )
        return cls(
            pack_format_version=data["pack_format_version"],
            pack_kind=data["pack_kind"],
            tree_size=data["tree_size"],
            root_hash=data["root_hash"],
            checkpoint_hash=data["checkpoint_hash"],
        )


@dataclass(frozen=True)
class SelectiveEvidencePack:
    """A selective evidence pack: a CHOSEN subset of audit rows, each proven
    included at its declared position under the covering checkpoint.
    Undisclosed rows are absent entirely. It proves the disclosed rows
    authentic - never that the selection is complete."""

    manifest: SelectivePackManifest
    checkpoint: Checkpoint
    rows: list
    inclusion_proofs: list

    @classmethod
    def from_json(cls, payload: object) -> "SelectiveEvidencePack":
        data = _strict(
            "selective evidence pack",
            payload,
            {"manifest", "checkpoint", "rows", "inclusion_proofs"},
        )
        return cls(
            manifest=SelectivePackManifest.from_json(data["manifest"]),
            checkpoint=Checkpoint.from_json(data["checkpoint"]),
            rows=[AuditRow.from_json(r) for r in data["rows"]],
            inclusion_proofs=[RowInclusionProof.from_json(p) for p in data["inclusion_proofs"]],
        )


@dataclass(frozen=True)
class SelectiveIntact:
    """Every disclosed row is included at its declared position.
    ``rows_disclosed`` counts what the pack chose to show - it says nothing
    about how many rows the tree holds or the selection missed."""

    tree_size: int
    rows_disclosed: int

    @classmethod
    def from_json(cls, payload: object) -> "SelectiveIntact":
        data = _strict("intact selective", payload, {"status", "tree_size", "rows_disclosed"})
        return cls(tree_size=data["tree_size"], rows_disclosed=data["rows_disclosed"])


@dataclass(frozen=True)
class SelectiveRowNotIncluded:
    """A disclosed row is not the row the checkpoint committed to at its
    declared position."""

    leaf_index: int

    @classmethod
    def from_json(cls, payload: object) -> "SelectiveRowNotIncluded":
        data = _strict("row-not-included selective", payload, {"status", "leaf_index"})
        return cls(leaf_index=data["leaf_index"])


@dataclass(frozen=True)
class SelectiveAnchorMismatch:
    """An externally held anchor disagrees with the pack's checkpoint."""

    tree_size: int
    anchor_checkpoint_hash: str
    pack_checkpoint_hash: str

    @classmethod
    def from_json(cls, payload: object) -> "SelectiveAnchorMismatch":
        data = _strict(
            "anchor-mismatch selective",
            payload,
            {"status", "tree_size", "anchor_checkpoint_hash", "pack_checkpoint_hash"},
        )
        return cls(
            tree_size=data["tree_size"],
            anchor_checkpoint_hash=data["anchor_checkpoint_hash"],
            pack_checkpoint_hash=data["pack_checkpoint_hash"],
        )


@dataclass(frozen=True)
class SelectiveSignatureInvalid:
    """The checkpoint carries a signature that does not verify over its
    tree head (cryptographic check only; authority is not judged here)."""

    tree_size: int
    key_id: str
    purpose: str
    public_key: str

    @classmethod
    def from_json(cls, payload: object) -> "SelectiveSignatureInvalid":
        data = _strict(
            "signature-invalid selective",
            payload,
            {"status", "tree_size", "key_id", "purpose", "public_key"},
        )
        return cls(
            tree_size=data["tree_size"],
            key_id=data["key_id"],
            purpose=data["purpose"],
            public_key=data["public_key"],
        )


@dataclass(frozen=True)
class SelectiveSignatureRequired:
    """``--require-signatures`` was asked for and the covering checkpoint
    carries no signature."""

    tree_size: int

    @classmethod
    def from_json(cls, payload: object) -> "SelectiveSignatureRequired":
        data = _strict("signature-required selective", payload, {"status", "tree_size"})
        return cls(tree_size=data["tree_size"])


@dataclass(frozen=True)
class SelectiveMalformed:
    """The selective pack is not a well-formed v3 artefact - it never had a
    chance to prove anything."""

    detail: str

    @classmethod
    def from_json(cls, payload: object) -> "SelectiveMalformed":
        data = _strict("malformed selective", payload, {"status", "detail"})
        return cls(detail=data["detail"])


SelectiveVerification = (
    SelectiveIntact
    | SelectiveRowNotIncluded
    | SelectiveAnchorMismatch
    | SelectiveSignatureInvalid
    | SelectiveSignatureRequired
    | SelectiveMalformed
)


def parse_selective_verification(payload: object) -> SelectiveVerification:
    """The selective-pack verdict, the output of ``evidence verify`` on a v3
    selective pack."""
    status = payload.get("status") if isinstance(payload, dict) else None
    match status:
        case "intact":
            return SelectiveIntact.from_json(payload)
        case "row_not_included":
            return SelectiveRowNotIncluded.from_json(payload)
        case "anchor_mismatch":
            return SelectiveAnchorMismatch.from_json(payload)
        case "signature_invalid":
            return SelectiveSignatureInvalid.from_json(payload)
        case "signature_required":
            return SelectiveSignatureRequired.from_json(payload)
        case "malformed":
            return SelectiveMalformed.from_json(payload)
        case _:
            raise EnvelopeError(f"not a selective verdict: {payload!r}")
