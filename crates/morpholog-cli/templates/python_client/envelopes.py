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

    @classmethod
    def from_json(cls, payload: object) -> "AuditRow":
        data = _strict("audit row", payload, _AUDIT_ROW_KEYS)
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

    @classmethod
    def from_json(cls, payload: object) -> "AuditRowNamed":
        data = _strict("named audit row", payload, _AUDIT_ROW_KEYS)
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
class InitReport:
    status: str
    schema: str

    @classmethod
    def from_json(cls, payload: object) -> "InitReport":
        data = _strict("init report", payload, {"status", "schema"})
        return cls(status=data["status"], schema=data["schema"])
