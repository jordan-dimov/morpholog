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
from collections.abc import Set as AbstractSet
from typing import Callable, Generic, TypeVar

from . import values


class EnvelopeError(ValueError):
    """An envelope that does not match the pinned contract."""


def _strict(
    name: str,
    payload: object,
    required: AbstractSet[str],
    optional: AbstractSet[str] = frozenset(),
) -> dict[str, object]:
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


_T = TypeVar("_T")


def _by_status(payload: object, label: str, mapping: dict[str, Callable[[object], _T]]) -> _T:
    """Dispatch a status-discriminated union to its model. An unknown
    or missing discriminator is drift, refused with the union named.
    Generic so each parser keeps its advertised return type under a
    static checker."""
    status = payload.get("status") if isinstance(payload, dict) else None
    parse = mapping.get(status) if isinstance(status, str) else None
    if parse is None:
        raise EnvelopeError(f"not {label}: {payload!r}")
    return parse(payload)


def _optional_timestamp(text: object) -> datetime | None:
    return None if text is None else values.parse_timestamp(str(text))


@dataclass(frozen=True)
class ClaimInstance:
    predicate: str
    args: list[object] = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> ClaimInstance:
        data = _strict("claim", payload, {"predicate", "args"})
        return cls(
            predicate=data["predicate"],
            args=[values.decode_tagged(a) for a in data["args"]],
        )


@dataclass(frozen=True)
class IntentInstance:
    name: str
    args: list[object] = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> IntentInstance:
        data = _strict("intent", payload, {"name", "args"})
        return cls(
            name=data["name"],
            args=[values.decode_tagged(a) for a in data["args"]],
        )


@dataclass(frozen=True)
class NamedClaim:
    """One row of the named read: bare values keyed by declared field."""

    predicate: str
    args: dict[str, object]

    @classmethod
    def from_json(cls, payload: object) -> NamedClaim:
        data = _strict("named claim", payload, {"predicate", "args"})
        return cls(predicate=data["predicate"], args=dict(data["args"]))


@dataclass(frozen=True)
class Committed:
    transition_id: str
    actor: str
    asserted_claims: list[ClaimInstance]
    retracted_claims: list[ClaimInstance]
    emitted_intents: list[IntentInstance]

    @classmethod
    def from_json(cls, payload: object) -> Committed:
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
class WitnessBinding:
    """A variable and the value it held where the refused rule failed."""

    var: str
    value: object

    @classmethod
    def from_json(cls, payload: object) -> WitnessBinding:
        data = _strict("witness binding", payload, {"var", "value"})
        return cls(var=data["var"], value=values.decode_tagged(data["value"]))


@dataclass(frozen=True)
class Rejected:
    reason: str
    # The refused rule's stable identifier: an invariant's name, or a named
    # gate's. None when the gate has no name - never the rendered
    # expression, so this is safe to assert on where `reason` is not.
    rule: str | None = None
    explanation: Explanation | None = None
    # Empty when the runtime could not attribute the refusal to one
    # iteration; the key is then absent from the envelope entirely.
    witness: list[WitnessBinding] = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> Rejected:
        data = _strict(
            "rejected outcome", payload, {"status", "reason"}, {"explanation", "rule", "witness"}
        )
        explanation = data.get("explanation")
        return cls(
            reason=data["reason"],
            rule=data.get("rule"),
            explanation=None if explanation is None else Explanation.from_json(explanation),
            witness=[WitnessBinding.from_json(w) for w in data.get("witness", [])],
        )


@dataclass(frozen=True)
class Errored:
    """A traced proposal whose transformation raised a kernel error:
    nothing was committed, and ``code`` is always ``kernel_error``."""

    code: str
    error: str

    @classmethod
    def from_json(cls, payload: object) -> Errored:
        data = _strict("errored result", payload, {"status", "code", "error"})
        if data["code"] != "kernel_error":
            raise EnvelopeError(f"errored result: unexpected code {data['code']!r}")
        return cls(code=str(data["code"]), error=str(data["error"]))


def parse_run_outcome(payload: object) -> Committed | Rejected:
    """The ``propose`` outcome envelope: a lawful business outcome
    either way. (The wire keeps the historical ``run_outcome`` name in
    the pinned schema; only the command verb changed.)"""
    return _by_status(
        payload,
        "a propose outcome",
        {
            "committed": Committed.from_json,
            "rejected": Rejected.from_json,
        },
    )


@dataclass(frozen=True)
class AtomicAct:
    """One act's receipt inside a committed ``transact``: its 1-based
    ``row`` and the committed outcome, with its own transition id."""

    row: int
    outcome: Committed

    @classmethod
    def from_json(cls, payload: object) -> AtomicAct:
        # Strict on the wire shape first, so a stray key - `status`
        # included - is drift, never silently rewritten.
        data = _strict(
            "atomic act",
            payload,
            {
                "row",
                "transition_id",
                "actor",
                "asserted_claims",
                "retracted_claims",
                "emitted_intents",
            },
        )
        body = {k: v for k, v in data.items() if k != "row"}
        body["status"] = "committed"
        return cls(row=int(str(data["row"])), outcome=Committed.from_json(body))


@dataclass(frozen=True)
class AtomicCommitted:
    """Every act committed, in order, as one decision."""

    acts: list[AtomicAct]

    @classmethod
    def from_json(cls, payload: object) -> AtomicCommitted:
        data = _strict("atomic committed", payload, {"status", "acts"}, optional={"row"})
        raw = data["acts"]
        if not isinstance(raw, list) or not raw:
            raise EnvelopeError(f"`acts` must be a non-empty list, got {raw!r}")
        return cls(acts=[AtomicAct.from_json(a) for a in raw])


@dataclass(frozen=True)
class AtomicRejected:
    """The first refused act, by 1-based position, and nothing written:
    the acts before it were staged and rolled back, and get no receipt.
    ``rule`` and ``witness`` are as on ``Rejected``; the witness may
    name values the rolled-back prefix staged."""

    act: int
    reason: str
    rule: str | None = None
    witness: list[WitnessBinding] = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> AtomicRejected:
        data = _strict(
            "atomic rejected",
            payload,
            {"status", "act", "reason"},
            optional={"rule", "witness", "row"},
        )
        rule = data.get("rule")
        return cls(
            act=int(str(data["act"])),
            reason=str(data["reason"]),
            rule=None if rule is None else str(rule),
            witness=[WitnessBinding.from_json(w) for w in data.get("witness", [])],
        )


def parse_atomic_outcome(payload: object) -> AtomicCommitted | AtomicRejected:
    """A ``transact`` outcome: committed or rejected. A coded error is
    not an outcome and is raised by the adapter."""
    return _by_status(
        payload,
        "a transact outcome",
        {
            "committed": AtomicCommitted.from_json,
            "rejected": AtomicRejected.from_json,
        },
    )


@dataclass(frozen=True)
class RenderedClaim:
    predicate: str
    rendered: str

    @classmethod
    def from_json(cls, payload: object) -> RenderedClaim:
        data = _strict("rendered claim", payload, {"predicate", "rendered"})
        return cls(predicate=data["predicate"], rendered=data["rendered"])


@dataclass(frozen=True)
class RequireHeld:
    match_count: int

    @classmethod
    def from_json(cls, payload: object) -> RequireHeld:
        data = _strict("require held", payload, {"status", "match_count"})
        return cls(match_count=data["match_count"])


@dataclass(frozen=True)
class RequireRejected:
    reason: str
    failing_sub_expression: str | None = None
    directly_missing_claims: list[RenderedClaim] = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> RequireRejected:
        data = _strict(
            "require rejected",
            payload,
            {"status", "reason"},
            {"failing_sub_expression", "directly_missing_claims"},
        )
        return cls(
            reason=data["reason"],
            failing_sub_expression=data.get("failing_sub_expression"),
            directly_missing_claims=[
                RenderedClaim.from_json(c) for c in data.get("directly_missing_claims", [])
            ],
        )


@dataclass(frozen=True)
class BindBound:
    bindings: list[WitnessBinding]

    @classmethod
    def from_json(cls, payload: object) -> BindBound:
        data = _strict("bind bound", payload, {"status", "bindings"})
        return cls(bindings=[WitnessBinding.from_json(b) for b in data["bindings"]])


@dataclass(frozen=True)
class BindNoMatch:
    failing_sub_expression: str | None = None
    directly_missing_claims: list[RenderedClaim] = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> BindNoMatch:
        data = _strict(
            "bind no match",
            payload,
            {"status"},
            {"failing_sub_expression", "directly_missing_claims"},
        )
        return cls(
            failing_sub_expression=data.get("failing_sub_expression"),
            directly_missing_claims=[
                RenderedClaim.from_json(c) for c in data.get("directly_missing_claims", [])
            ],
        )


@dataclass(frozen=True)
class BindMultipleMatches:
    count: int

    @classmethod
    def from_json(cls, payload: object) -> BindMultipleMatches:
        data = _strict("bind multiple matches", payload, {"status", "count"})
        return cls(count=data["count"])


def _parse_require_outcome(payload: object) -> RequireHeld | RequireRejected:
    return _by_status(
        payload,
        "a require outcome",
        {"held": RequireHeld.from_json, "rejected": RequireRejected.from_json},
    )


def _parse_bind_outcome(payload: object) -> BindBound | BindNoMatch | BindMultipleMatches:
    return _by_status(
        payload,
        "a bind outcome",
        {
            "bound": BindBound.from_json,
            "no_match": BindNoMatch.from_json,
            "multiple_matches": BindMultipleMatches.from_json,
        },
    )


@dataclass(frozen=True)
class RequireStep:
    expression: str
    outcome: RequireHeld | RequireRejected
    # The gate's stable identifier, when its author gave it one. Hold this
    # rather than `expression`, which any rewording changes.
    name: str | None = None

    @classmethod
    def from_json(cls, payload: object) -> RequireStep:
        data = _strict("require step", payload, {"kind", "expression", "outcome"}, {"name"})
        return cls(
            expression=data["expression"],
            outcome=_parse_require_outcome(data["outcome"]),
            name=data.get("name"),
        )


@dataclass(frozen=True)
class BindStep:
    expression: str
    outcome: BindBound | BindNoMatch | BindMultipleMatches
    name: str | None = None

    @classmethod
    def from_json(cls, payload: object) -> BindStep:
        data = _strict("bind step", payload, {"kind", "expression", "outcome"}, {"name"})
        return cls(
            expression=data["expression"],
            outcome=_parse_bind_outcome(data["outcome"]),
            name=data.get("name"),
        )


@dataclass(frozen=True)
class LetStep:
    name: str
    value: object

    @classmethod
    def from_json(cls, payload: object) -> LetStep:
        data = _strict("let step", payload, {"kind", "name", "value"})
        return cls(name=data["name"], value=values.decode_tagged(data["value"]))


@dataclass(frozen=True)
class LetNewSubjectStep:
    name: str
    subject: object

    @classmethod
    def from_json(cls, payload: object) -> LetNewSubjectStep:
        data = _strict("let new subject step", payload, {"kind", "name", "subject"})
        return cls(name=data["name"], subject=values.decode_tagged(data["subject"]))


@dataclass(frozen=True)
class AssertStep:
    """A claim admitted. The wire kind is `assert`, which is what the
    surface language spells `admit`."""

    claim: ClaimInstance

    @classmethod
    def from_json(cls, payload: object) -> AssertStep:
        data = _strict("assert step", payload, {"kind", "claim"})
        return cls(claim=ClaimInstance.from_json(data["claim"]))


@dataclass(frozen=True)
class RetractStep:
    predicate: str
    retracted: list[ClaimInstance]

    @classmethod
    def from_json(cls, payload: object) -> RetractStep:
        data = _strict("retract step", payload, {"kind", "predicate", "retracted"})
        return cls(
            predicate=data["predicate"],
            retracted=[ClaimInstance.from_json(c) for c in data["retracted"]],
        )


@dataclass(frozen=True)
class EmitStep:
    intent: IntentInstance

    @classmethod
    def from_json(cls, payload: object) -> EmitStep:
        data = _strict("emit step", payload, {"kind", "intent"})
        return cls(intent=IntentInstance.from_json(data["intent"]))


@dataclass(frozen=True)
class ForIteration:
    item: object
    trace: list[TraceStep]

    @classmethod
    def from_json(cls, payload: object) -> ForIteration:
        data = _strict("for iteration", payload, {"item", "trace"})
        return cls(
            item=values.decode_tagged(data["item"]),
            trace=[parse_trace_step(e) for e in data["trace"]],
        )


@dataclass(frozen=True)
class ForStep:
    binding: str
    iterations: list[ForIteration]

    @classmethod
    def from_json(cls, payload: object) -> ForStep:
        data = _strict("for step", payload, {"kind", "binding", "iterations"})
        return cls(
            binding=data["binding"],
            iterations=[ForIteration.from_json(i) for i in data["iterations"]],
        )


@dataclass(frozen=True)
class InvariantCheckStep:
    name: str
    expression: str
    held: bool

    @classmethod
    def from_json(cls, payload: object) -> InvariantCheckStep:
        data = _strict("invariant check step", payload, {"kind", "name", "expression", "held"})
        return cls(name=data["name"], expression=data["expression"], held=data["held"])


TraceStep = (
    RequireStep
    | BindStep
    | LetStep
    | LetNewSubjectStep
    | AssertStep
    | RetractStep
    | EmitStep
    | ForStep
    | InvariantCheckStep
)


def parse_trace_step(payload: object) -> TraceStep:
    """One `--trace` step, by declared kind."""
    kind = payload.get("kind") if isinstance(payload, dict) else None
    by_kind = {
        "require": RequireStep.from_json,
        "bind_one": BindStep.from_json,
        "let": LetStep.from_json,
        "let_new_subject": LetNewSubjectStep.from_json,
        "assert": AssertStep.from_json,
        "retract": RetractStep.from_json,
        "emit": EmitStep.from_json,
        "for": ForStep.from_json,
        "invariant_check": InvariantCheckStep.from_json,
    }
    parse = by_kind.get(kind) if isinstance(kind, str) else None
    if parse is None:
        raise EnvelopeError(f"unknown trace step kind in {payload!r}")
    return parse(payload)


@dataclass(frozen=True)
class TracedEnvelope:
    result: Committed | Rejected | Errored
    trace: list[TraceStep]

    @classmethod
    def from_json(cls, payload: object) -> TracedEnvelope:
        data = _strict("traced envelope", payload, {"result", "trace"})
        result = data["result"]
        status = result.get("status") if isinstance(result, dict) else None
        parsed: Committed | Rejected | Errored
        if status == "errored":
            parsed = Errored.from_json(result)
        else:
            parsed = parse_run_outcome(result)
        return cls(result=parsed, trace=[parse_trace_step(e) for e in data["trace"]])


@dataclass(frozen=True)
class TransitionRef:
    transformation: str
    args: list[object]
    actor: str

    @classmethod
    def from_json(cls, payload: object) -> TransitionRef:
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
    candidate_supplier_transformations: list[str]

    @classmethod
    def from_json(cls, payload: object) -> MissingClaim:
        data = _strict(
            "missing claim",
            payload,
            {"predicate", "rendered", "candidate_supplier_transformations"},
        )
        return cls(
            predicate=data["predicate"],
            rendered=data["rendered"],
            candidate_supplier_transformations=_str_list(
                "candidate_supplier_transformations",
                data["candidate_supplier_transformations"],
            ),
        )


@dataclass(frozen=True)
class GateRejection:
    gate: str
    statement_kind: str
    directly_missing_claims: list[MissingClaim]
    # The gate's stable identifier, when its author gave it one. `gate` is
    # prose that any rewording changes; this does not move.
    rule: str | None = None

    @classmethod
    def from_json(cls, payload: object) -> GateRejection:
        data = _strict(
            "gate rejection",
            payload,
            {"kind", "gate", "statement_kind", "directly_missing_claims"},
            {"rule"},
        )
        return cls(
            gate=data["gate"],
            statement_kind=data["statement_kind"],
            directly_missing_claims=[
                MissingClaim.from_json(m) for m in data["directly_missing_claims"]
            ],
            rule=data.get("rule"),
        )


@dataclass(frozen=True)
class InvariantRejection:
    name: str
    rule: str

    @classmethod
    def from_json(cls, payload: object) -> InvariantRejection:
        data = _strict("invariant rejection", payload, {"kind", "name", "rule"})
        return cls(name=data["name"], rule=data["rule"])


@dataclass(frozen=True)
class ErrorRejection:
    message: str

    @classmethod
    def from_json(cls, payload: object) -> ErrorRejection:
        data = _strict("error rejection", payload, {"kind", "message"})
        return cls(message=data["message"])


def _parse_rejection(payload: object) -> GateRejection | InvariantRejection | ErrorRejection:
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
    rejection: GateRejection | InvariantRejection | ErrorRejection | None

    @property
    def admissible(self) -> bool:
        return self.rejection is None

    @classmethod
    def from_json(cls, payload: object) -> Explanation:
        data = _strict("explanation", payload, {"transition", "verdict"})
        verdict = data["verdict"]
        if verdict == "admissible":
            rejection = None
        else:
            wrapped = _strict("verdict", verdict, {"rejected"})
            rejection = _parse_rejection(wrapped["rejected"])
        return cls(transition=TransitionRef.from_json(data["transition"]), rejection=rejection)


@dataclass(frozen=True)
class SessionReady:
    """The first and only unprompted line a ``morpholog session``
    emits. ``model_hash`` is the canonical rules-identity hash the
    programme was pinned at; ``protocol`` is the wire's own version,
    distinct from the binary's."""

    model_hash: str
    morpholog_version: str
    program: str
    protocol: int

    @classmethod
    def from_json(cls, payload: object) -> SessionReady:
        data = _strict(
            "session ready",
            payload,
            {"model_hash", "morpholog_version", "program", "protocol", "status"},
        )
        if data["status"] != "ready":
            raise EnvelopeError(f"session ready: unexpected status {data['status']!r}")
        return cls(
            model_hash=str(data["model_hash"]),
            morpholog_version=str(data["morpholog_version"]),
            program=str(data["program"]),
            protocol=int(str(data["protocol"])),
        )


#: The codes a proposal can fail with - the schema's
#: ``propose_error_code``, which a test holds this set to. Only one of
#: these, other than ``commit_outcome_unknown``, lets a caller treat a
#: proposal as not committed; any other code is read as unknown.
PROPOSE_ERROR_CODES = frozenset(
    {
        "actor_assertion_unauthorised",
        "commit_outcome_unknown",
        "duplicate_intent",
        "invalid_arguments",
        "invalid_request",
        "kernel_error",
        "not_committed",
        "serialization_failure",
        "unknown_transformation",
    }
)


#: The codes that say nothing was recorded: every published code but
#: ``commit_outcome_unknown``. A caller that hand-rolls its own handling
#: should treat a proposal as not committed on one of these and on
#: nothing else.
NOTHING_RECORDED_CODES = PROPOSE_ERROR_CODES - {"commit_outcome_unknown"}


@dataclass(frozen=True)
class RequestError:
    """The error object a one-shot ``propose`` or ``transact`` prints when
    the request as a whole failed, and the object a batch prints when it
    was refused before its first row. It is the binary's own statement;
    the client never infers one."""

    code: str
    error: str

    @classmethod
    def from_json(cls, payload: object) -> RequestError:
        data = _strict("request error", payload, {"code", "error", "status"})
        if data["status"] != "error":
            raise EnvelopeError(f"request error: unexpected status {data['status']!r}")
        return cls(code=str(data["code"]), error=str(data["error"]))


@dataclass(frozen=True)
class SessionErrorReceipt:
    """A per-request session failure with its stable ``code`` - the
    field a caller consults to decide whether re-submitting is safe
    (``serialization_failure`` is the one re-submittable code)."""

    code: str
    error: str
    row: int

    @classmethod
    def from_json(cls, payload: object) -> SessionErrorReceipt:
        data = _strict("session error receipt", payload, {"code", "error", "row", "status"})
        if data["status"] != "error":
            raise EnvelopeError(f"session error receipt: unexpected status {data['status']!r}")
        return cls(
            code=str(data["code"]),
            error=str(data["error"]),
            row=int(str(data["row"])),
        )


@dataclass(frozen=True)
class BatchError:
    """A row that could not be proposed, with the same stable ``code``
    set a session error receipt carries."""

    code: str
    error: str


@dataclass(frozen=True)
class BatchReceipt:
    row: int
    outcome: Committed | Rejected | BatchError

    @classmethod
    def from_json(cls, payload: object) -> BatchReceipt:
        if not isinstance(payload, dict) or "row" not in payload:
            raise EnvelopeError(f"not a batch receipt: {payload!r}")
        row = payload["row"]
        body = {k: v for k, v in payload.items() if k != "row"}
        if body.get("status") == "error":
            data = _strict("batch error receipt", body, {"status", "code", "error"})
            return cls(row=row, outcome=BatchError(code=str(data["code"]), error=str(data["error"])))
        return cls(row=row, outcome=parse_run_outcome(body))


@dataclass(frozen=True)
class RejectionRow:
    """One refused proposal, from `inspect rejections`.

    An operational floor, not a ledger: this log is at-most-once and audit
    is the only legitimacy-grade record, so treat a row as a lead to follow
    rather than proof a refusal happened exactly this way.
    """

    rejection_id: str
    transformation_name: str
    arguments: list[object]
    actor: object
    kind: str
    rule: str
    reason: str
    rejected_at: datetime
    invariant_version: int | None = None
    # The values the refused rule was reading. None when the kernel could
    # not pin the failure to one iteration, and for rows written before the
    # column existed - so absence means "not captured", never "captured
    # nothing".
    witness: list[WitnessBinding] | None = None

    @classmethod
    def from_json(cls, payload: object) -> RejectionRow:
        data = _strict(
            "rejection row",
            payload,
            {
                "rejection_id",
                "transformation_name",
                "arguments",
                "actor",
                "kind",
                "rule",
                "reason",
                "rejected_at",
            },
            {"invariant_version", "witness"},
        )
        witness = data.get("witness")
        # Only an invariant refusal reports values or a version. A gate
        # refusal carrying either is a serializer regression, not a row -
        # so it raises here rather than becoming a model nobody can trust.
        if data["kind"] != "invariant" and (
            witness is not None or data.get("invariant_version") is not None
        ):
            raise EnvelopeError(
                f"rejection row: kind {data['kind']!r} cannot carry a witness or an "
                f"invariant version, got {payload!r}"
            )
        return cls(
            rejection_id=data["rejection_id"],
            transformation_name=data["transformation_name"],
            arguments=[values.decode_tagged(a) for a in data["arguments"]],
            actor=values.decode_tagged(data["actor"]),
            kind=data["kind"],
            rule=data["rule"],
            reason=data["reason"],
            rejected_at=values.parse_timestamp(data["rejected_at"]),
            invariant_version=data.get("invariant_version"),
            witness=None if witness is None else [WitnessBinding.from_json(w) for w in witness],
        )


@dataclass(frozen=True)
class OutboxRow:
    intent_id: str
    transition_id: str
    intent_type: str
    arguments: list[object]
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
    def from_json(cls, payload: object) -> OutboxRow:
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


def parse_outbox_claim(payload: object) -> OutboxRow | None:
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
    def from_json(cls, payload: object) -> OutboxUpdate:
        data = _strict("outbox update", payload, {"status"})
        return cls(status=data["status"])


@dataclass(frozen=True)
class AuditedInvariantCheck:
    """One active invariant the transition was admitted under: name
    plus the version active at commit time. Discharged because the
    change could not affect it, because every affected case satisfied
    it, or because the whole invariant held."""

    name: str
    version: int

    @classmethod
    def from_json(cls, payload: object) -> AuditedInvariantCheck:
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

_AUDIT_ROW_OPTIONAL_KEYS = {"attestation", "parameters", "model_hash"}


def _parameters_of(data: dict[str, object]) -> list[str] | None:
    """The stamped parameter names, held to the shapes a row can have:
    names are strings (leaf-covered evidence, never coerced), only an
    attested row carries them, and there is one per argument."""
    raw = data.get("parameters")
    if raw is None:
        return None
    names = _str_list("parameters", raw)
    if data.get("attestation") is None:
        raise EnvelopeError("an audit row carries parameter names but no attestation")
    arguments = data.get("arguments")
    if not isinstance(arguments, list) or len(names) != len(arguments):
        raise EnvelopeError(
            f"an audit row carries {len(names)} parameter names for "
            f"{len(arguments) if isinstance(arguments, list) else '?'} arguments"
        )
    return names


def _model_hash_of(data: dict[str, object]) -> str | None:
    """The programme hash a row names, held to its one shape: `sha256:`
    and 64 lowercase hex digits, only on a row that also carries an
    attestation and parameter names."""
    raw = data.get("model_hash", _ABSENT)
    if raw is _ABSENT:
        return None
    if (
        not isinstance(raw, str)
        or not raw.startswith("sha256:")
        or len(raw) != 71
        or any(c not in "0123456789abcdef" for c in raw[7:])
    ):
        raise EnvelopeError(f"an audit row carries a malformed model hash {raw!r}")
    if data.get("attestation") is None or data.get("parameters") is None:
        raise EnvelopeError(
            "an audit row carries a model hash without an attestation and parameter names"
        )
    return raw


_ABSENT = object()


@dataclass(frozen=True)
class Attestation:
    """How the actor identity on an audit row was established. Gateway
    mode records which PostgreSQL-authenticated role asserted the
    actor; it proves who asserted, never that the named actor
    authorised anything. Rows written before attestation existed
    carry none."""

    mode: str
    authenticated_by: str
    #: The role's OID when it asserted: which incarnation of the name, since
    #: a dropped role's name can be created again. ``None`` on rows written
    #: before it was recorded.
    authenticated_by_oid: int | None = None

    @classmethod
    def from_json(cls, payload: object) -> Attestation:
        data = _strict(
            "attestation", payload, {"mode", "authenticated_by"}, {"authenticated_by_oid"}
        )
        if data["mode"] != "gateway":
            raise EnvelopeError(
                f"attestation: unknown mode {data['mode']!r} - the binary's "
                "contract has drifted past this generated client; regenerate it"
            )
        oid = data.get("authenticated_by_oid")
        return cls(
            mode=data["mode"],
            authenticated_by=data["authenticated_by"],
            authenticated_by_oid=None if oid is None else int(str(oid)),
        )


def _attestation_of(data: dict[str, object]) -> Attestation | None:
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
    arguments: list[object]
    actor: str
    invariant_epoch: int
    invariants_checked: list[AuditedInvariantCheck]
    asserted_claims: list[ClaimInstance]
    retracted_claims: list[ClaimInstance]
    emitted_intents: list[IntentInstance]
    committed_at: datetime
    attestation: Attestation | None = None
    # The transformation's parameter names in declaration order, one
    # per argument, as the writer stamped them: the row's own signature,
    # readable after the act is retired. None on rows from before names
    # were stamped.
    parameters: list[str] | None = None
    # The canonical hash of the whole programme that admitted the row, as
    # `morpholog hash` prints it. None on rows from before it was stamped.
    model_hash: str | None = None

    @classmethod
    def from_json(cls, payload: object) -> AuditRow:
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
            parameters=_parameters_of(data),
            model_hash=_model_hash_of(data),
        )


@dataclass(frozen=True)
class AuditRowNamed:
    """`AuditRow` with the asserted/retracted claims decoded by
    declared field name (the `--named` tail). `arguments` and
    `emitted_intents` stay positional - they belong to the
    transformation/intent vocabularies, not predicate declarations."""

    transition_id: str
    transformation_name: str
    arguments: list[object]
    actor: str
    invariant_epoch: int
    invariants_checked: list[AuditedInvariantCheck]
    asserted_claims: list[NamedClaim]
    retracted_claims: list[NamedClaim]
    emitted_intents: list[IntentInstance]
    committed_at: datetime
    attestation: Attestation | None = None
    # The transformation's parameter names in declaration order, one
    # per argument, as the writer stamped them: the row's own signature,
    # readable after the act is retired. None on rows from before names
    # were stamped.
    parameters: list[str] | None = None
    # The canonical hash of the whole programme that admitted the row, as
    # `morpholog hash` prints it. None on rows from before it was stamped.
    model_hash: str | None = None

    @classmethod
    def from_json(cls, payload: object) -> AuditRowNamed:
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
            parameters=_parameters_of(data),
            model_hash=_model_hash_of(data),
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
    def from_json(cls, payload: object) -> InvariantCoverage:
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
    def from_json(cls, payload: object) -> TransformationUsage:
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
    invariants: list[InvariantCoverage]
    transformations: list[TransformationUsage]

    @classmethod
    def from_json(cls, payload: object) -> CoverageReport:
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
    def from_json(cls, payload: object) -> Diagnostic:
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


_CHECK_ROUTES = frozenset({"compiled", "interpreted"})
_REFUSAL_KINDS = frozenset(
    {
        "construct",
        "comparison_domain",
        "argument_kind",
        "literal",
        "sum_shape",
        "comparison_shape",
    }
)


def _member(label: str, value: object, known: AbstractSet[str]) -> str:
    if not isinstance(value, str) or value not in known:
        raise EnvelopeError(
            f"{label}: unknown value {value!r} - the binary's contract has "
            f"drifted past this generated client; regenerate it"
        )
    return value


@dataclass(frozen=True)
class CheckRefusal:
    """Why an invariant is interpreted: ``kind`` from a closed list, and
    the ``message`` that ``check -v`` prints."""

    kind: str
    message: str

    @classmethod
    def from_json(cls, payload: object) -> CheckRefusal:
        data = _strict("check refusal", payload, {"kind", "message"})
        return cls(
            kind=_member("check refusal kind", data["kind"], _REFUSAL_KINDS),
            message=data["message"],
        )


@dataclass(frozen=True)
class CheckedInvariant:
    """One invariant and the route the binary plans for it: ``compiled``
    (checked in SQL) or ``interpreted`` (checked by the kernel, with the
    refusal that kept it out of SQL)."""

    name: str
    route: str
    refusal: CheckRefusal | None = None

    @classmethod
    def from_json(cls, payload: object) -> CheckedInvariant:
        data = _strict("checked invariant", payload, {"name", "route"}, {"refusal"})
        route = _member("checked invariant route", data["route"], _CHECK_ROUTES)
        if (route == "interpreted") != ("refusal" in data):
            raise EnvelopeError(
                f"checked invariant: a refusal belongs to an interpreted invariant "
                f"and only to one, got {payload!r}"
            )
        return cls(
            name=data["name"],
            route=route,
            refusal=CheckRefusal.from_json(data["refusal"]) if "refusal" in data else None,
        )


@dataclass(frozen=True)
class CheckReport:
    """The findings for one file, and the route this binary plans for each
    invariant. ``invariants`` is ``None`` only when parsing or validation
    left no programme to plan; an empty list is a programme with no
    invariants. The route belongs to the programme and the binary
    together: another version may plan differently."""

    file: str
    diagnostics: list[Diagnostic]
    invariants: list[CheckedInvariant] | None = None

    @property
    def route(self) -> str | None:
        """``compiled``, ``interpreted`` or ``mixed`` over every invariant,
        as ``check -v`` says it; ``None`` when there is no plan. A
        programme with no invariants is ``compiled``, as the runtime
        treats it."""
        if self.invariants is None:
            return None
        routes = {i.route for i in self.invariants}
        if routes <= {"compiled"}:
            return "compiled"
        if routes == {"interpreted"}:
            return "interpreted"
        return "mixed"

    @classmethod
    def from_json(cls, payload: object) -> CheckReport:
        data = _strict("check report", payload, {"file", "diagnostics"}, {"invariants"})
        invariants = None
        if "invariants" in data:
            if not isinstance(data["invariants"], list):
                raise EnvelopeError(f"check report: invariants is not a list in {payload!r}")
            invariants = [CheckedInvariant.from_json(i) for i in data["invariants"]]
        return cls(
            file=data["file"],
            diagnostics=[Diagnostic.from_json(d) for d in data["diagnostics"]],
            invariants=invariants,
        )


@dataclass(frozen=True)
class HashReport:
    """The rules-identity hash of a programme, from the binary reporting
    it. A pinned client compares both against its own stamps."""

    program: str
    hash: str
    morpholog_version: str

    @classmethod
    def from_json(cls, payload: object) -> HashReport:
        data = _strict("hash report", payload, {"program", "hash", "morpholog_version"})
        return cls(
            program=data["program"],
            hash=data["hash"],
            morpholog_version=data["morpholog_version"],
        )


def version_skew(payload: object, expected: str) -> str | None:
    """The version the binary states in its envelope against the one this
    client was generated for, or ``None`` when they agree or the envelope
    states none.

    Read before the strict parser, on purpose: a binary of another version
    may speak another protocol or carry a field this client does not know,
    and the version is the reason for that, so it is the diagnosis to give.
    The strict parser then still sees the whole object when the versions
    agree; this reads one key and nothing else. An envelope that states no
    version is not evidence of any version, so it is left to the strict
    parser to refuse."""
    actual = payload.get("morpholog_version") if isinstance(payload, dict) else None
    if isinstance(actual, str) and actual != expected:
        return f"the binary is Morpholog {actual}; this client was generated for {expected}"
    return None


def predates_versioned_hash(payload: object) -> bool:
    """Whether a `hash` report is the exact shape every binary emitted
    before the report carried a version: the one legacy shape a
    generated client names as such rather than as drift."""
    return isinstance(payload, dict) and set(payload) == {"hash", "program"}


@dataclass(frozen=True)
class LeastPrivilege:
    """The `--least-privilege` floor as applied: the two group roles,
    plus the membership grants only the operator can decide."""

    next_steps: tuple[str, ...]
    reader_role: str
    writer_role: str

    @classmethod
    def from_json(cls, payload: object) -> LeastPrivilege:
        data = _strict(
            "least privilege", payload, {"next_steps", "reader_role", "writer_role"}
        )
        return cls(
            next_steps=tuple(data["next_steps"]),
            reader_role=data["reader_role"],
            writer_role=data["writer_role"],
        )


@dataclass(frozen=True)
class MigrationRef:
    version: int
    name: str

    @classmethod
    def from_json(cls, payload: object) -> MigrationRef:
        data = _strict("migration ref", payload, {"version", "name"})
        return cls(version=data["version"], name=data["name"])


@dataclass(frozen=True)
class MigrationReport:
    """What `migrate` found, and what it did about it."""

    # None when the database recorded nothing - one older than the record
    # itself. Not 0: "no record exists" and "at version zero" are different
    # claims, and such a database may well have migrations applied.
    recorded_version_before: int | None
    recorded_version_after: int | None
    binary_version: int
    applied: list[MigrationRef]
    pending: list[MigrationRef]
    # Recorded by the database and unknown to this binary: the database is
    # AHEAD, which is what a rollback to an older binary looks like.
    unknown: list[MigrationRef] = field(default_factory=list)

    @property
    def is_current(self) -> bool:
        """Nothing outstanding and nothing unrecognised. What a deploy gate
        asks - and it is false for a database AHEAD of this binary too,
        which is when a green light would be most dangerous."""
        return not self.pending and not self.unknown

    @classmethod
    def from_json(cls, payload: object) -> MigrationReport:
        data = _strict(
            "migration report",
            payload,
            {"recorded_version_before", "recorded_version_after", "binary_version",
             "applied", "pending"},
            {"unknown"},
        )
        return cls(
            recorded_version_before=data["recorded_version_before"],
            recorded_version_after=data["recorded_version_after"],
            binary_version=data["binary_version"],
            applied=[MigrationRef.from_json(m) for m in data["applied"]],
            pending=[MigrationRef.from_json(m) for m in data["pending"]],
            unknown=[MigrationRef.from_json(m) for m in data.get("unknown", [])],
        )


@dataclass(frozen=True)
class ProvisionedProgram:
    program: str
    hash: str

    @classmethod
    def from_json(cls, payload: object) -> ProvisionedProgram:
        data = _strict("provisioned program", payload, {"program", "hash"})
        return cls(program=data["program"], hash=data["hash"])


_INDEX_ACTIONS = frozenset(
    {"keep", "create", "repair_invalid", "satisfied_externally", "stale", "conflict"}
)
_STATISTICS_ACTIONS = frozenset({"keep", "create", "conflict", "stale"})


def _action(label: str, value: object, known: AbstractSet[str]) -> str:
    if not isinstance(value, str) or value not in known:
        raise EnvelopeError(
            f"{label}: unknown action {value!r} - the binary's contract has "
            f"drifted past this generated client; regenerate it"
        )
    return value


@dataclass(frozen=True)
class ProvisionedIndex:
    """One index the call reconciled against the catalogue. ``detail`` is
    for an operator to read, not to decide on."""

    action: str
    name: str
    predicate: str
    position: int
    required_by: list[str]
    detail: str = ""

    @classmethod
    def from_json(cls, payload: object) -> ProvisionedIndex:
        data = _strict(
            "provisioned index",
            payload,
            {"action", "name", "predicate", "position", "required_by"},
            {"detail"},
        )
        return cls(
            action=_action("provisioned index", data["action"], _INDEX_ACTIONS),
            name=data["name"],
            predicate=data["predicate"],
            position=data["position"],
            required_by=_str_list("required_by", data["required_by"]),
            detail=data.get("detail", ""),
        )


@dataclass(frozen=True)
class ProvisionedStatistics:
    """One statistics object Morpholog manages, the named programmes' or
    another's: an object is fully known from its position."""

    action: str
    name: str
    position: int
    required_by: list[str]
    detail: str = ""

    @classmethod
    def from_json(cls, payload: object) -> ProvisionedStatistics:
        data = _strict(
            "provisioned statistics",
            payload,
            {"action", "name", "position", "required_by"},
            {"detail"},
        )
        return cls(
            action=_action("provisioned statistics", data["action"], _STATISTICS_ACTIONS),
            name=data["name"],
            position=data["position"],
            required_by=_str_list("required_by", data["required_by"]),
            detail=data.get("detail", ""),
        )


@dataclass(frozen=True)
class RequiredElsewhere:
    """A managed index no named programme requires, protected from a prune
    by a programme outside the call. It says nothing about whether the
    index is still in the catalogue."""

    name: str
    required_by: list[str]

    @classmethod
    def from_json(cls, payload: object) -> RequiredElsewhere:
        data = _strict("required elsewhere", payload, {"name", "required_by"})
        return cls(name=data["name"], required_by=_str_list("required_by", data["required_by"]))


@dataclass(frozen=True)
class ProvisionReport:
    """What `provision indexes` planned and did, over every programme the
    call named."""

    applied: bool
    dry_run: bool
    prune: bool
    programs: list[ProvisionedProgram]
    indexes: list[ProvisionedIndex]
    statistics: list[ProvisionedStatistics]
    required_elsewhere: list[RequiredElsewhere]
    # Programmes outside the call with a recorded requirement whose
    # position is not known. While any, no statistics object is stale;
    # provisioning them again records it.
    positions_unknown_for: list[str]

    @property
    def has_conflict(self) -> bool:
        """Something under Morpholog's own name has another definition. The
        run applied nothing and an operator has to look."""
        return any(e.action == "conflict" for e in (*self.indexes, *self.statistics))

    @property
    def pruned(self) -> list[str]:
        """What this run dropped: every stale index and statistics object,
        when it applied under prune."""
        if not (self.applied and self.prune):
            return []
        return [e.name for e in (*self.indexes, *self.statistics) if e.action == "stale"]

    @classmethod
    def from_json(cls, payload: object) -> ProvisionReport:
        data = _strict(
            "provision report",
            payload,
            {"applied", "dry_run", "prune", "programs", "indexes", "statistics",
             "required_elsewhere", "positions_unknown_for"},
        )
        return cls(
            applied=data["applied"],
            dry_run=data["dry_run"],
            prune=data["prune"],
            programs=[ProvisionedProgram.from_json(p) for p in data["programs"]],
            indexes=[ProvisionedIndex.from_json(i) for i in data["indexes"]],
            statistics=[ProvisionedStatistics.from_json(s) for s in data["statistics"]],
            required_elsewhere=[
                RequiredElsewhere.from_json(r) for r in data["required_elsewhere"]
            ],
            positions_unknown_for=_str_list(
                "positions_unknown_for", data["positions_unknown_for"]
            ),
        )


@dataclass(frozen=True)
class InitReport:
    status: str
    schema: str
    least_privilege: LeastPrivilege | None = None

    @classmethod
    def from_json(cls, payload: object) -> InitReport:
        data = _strict(
            "init report", payload, {"status", "schema"}, optional={"least_privilege"}
        )
        floor = data.get("least_privilege")
        return cls(
            status=data["status"],
            schema=data["schema"],
            least_privilege=None if floor is None else LeastPrivilege.from_json(floor),
        )


@dataclass(frozen=True)
class RefreshDerivedReport:
    """The published read-model generation (`refresh derived`). The
    snapshot pair is the latest audit transition visible in the
    refresh's read snapshot - a coarse freshness marker, never a
    lossless audit-resume cursor (a writer in flight at snapshot time
    is excluded and folded in by the next refresh; lossless resume is
    the audit tail). The pair is present or absent together."""

    derived_claim_count: int
    derived_predicate_count: int
    model_hash: str
    refresh_id: str
    source_claim_count: int
    source_snapshot_committed_at: datetime | None = None
    source_snapshot_transition_id: str | None = None

    @classmethod
    def from_json(cls, payload: object) -> RefreshDerivedReport:
        data = _strict(
            "refresh derived report",
            payload,
            {
                "derived_claim_count",
                "derived_predicate_count",
                "model_hash",
                "refresh_id",
                "source_claim_count",
            },
            {"source_snapshot_committed_at", "source_snapshot_transition_id"},
        )
        tid = data.get("source_snapshot_transition_id")
        at = data.get("source_snapshot_committed_at")
        if (tid is None) != (at is None):
            raise EnvelopeError(
                "refresh derived report: the snapshot pair must be present "
                f"or absent together, got {payload!r}"
            )
        return cls(
            derived_claim_count=data["derived_claim_count"],
            derived_predicate_count=data["derived_predicate_count"],
            model_hash=data["model_hash"],
            refresh_id=data["refresh_id"],
            source_claim_count=data["source_claim_count"],
            source_snapshot_committed_at=_optional_timestamp(at),
            source_snapshot_transition_id=tid,
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
    def from_json(cls, payload: object) -> ReplayConsistent:
        data = _strict("consistent replay", payload, {"status", "transitions", "claims"})
        return cls(transitions=data["transitions"], claims=data["claims"])


@dataclass(frozen=True)
class ReplayDivergent:
    """The claims table and the audit log disagree - evidence one was
    edited out of band."""

    only_in_claims_table: list[ClaimInstance]
    only_in_replay: list[ClaimInstance]

    @classmethod
    def from_json(cls, payload: object) -> ReplayDivergent:
        data = _strict(
            "divergent replay", payload, {"status", "only_in_claims_table", "only_in_replay"}
        )
        return cls(
            only_in_claims_table=[ClaimInstance.from_json(c) for c in data["only_in_claims_table"]],
            only_in_replay=[ClaimInstance.from_json(c) for c in data["only_in_replay"]],
        )


def parse_verify_outcome(payload: object) -> ReplayConsistent | ReplayDivergent:
    return _by_status(
        payload,
        "a replay verdict",
        {
            "consistent": ReplayConsistent.from_json,
            "divergent": ReplayDivergent.from_json,
        },
    )


@dataclass(frozen=True)
class TreeIntact:
    checkpoints: int
    tree_size: int

    @classmethod
    def from_json(cls, payload: object) -> TreeIntact:
        data = _strict("intact tree", payload, {"status", "checkpoints", "tree_size"})
        return cls(checkpoints=data["checkpoints"], tree_size=data["tree_size"])


@dataclass(frozen=True)
class TreeTampered:
    tree_size: int
    recorded_root: str
    recomputed_root: str

    @classmethod
    def from_json(cls, payload: object) -> TreeTampered:
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
    def from_json(cls, payload: object) -> TreeChainBroken:
        data = _strict("chain-broken tree", payload, {"status", "detail"})
        return cls(detail=data["detail"])


@dataclass(frozen=True)
class TreeAnchorMismatch:
    tree_size: int
    anchor_checkpoint_hash: str
    stored_checkpoint_hash: str | None

    @classmethod
    def from_json(cls, payload: object) -> TreeAnchorMismatch:
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
    def from_json(cls, payload: object) -> TreeMalformedPack:
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
    def from_json(cls, payload: object) -> TreeSignatureInvalid:
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
    def from_json(cls, payload: object) -> TreeUnauthorizedKey:
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
    def from_json(cls, payload: object) -> TreeSignatureRequired:
        data = _strict("signature-required tree", payload, {"status", "tree_size"})
        return cls(tree_size=data["tree_size"])


@dataclass(frozen=True)
class TreeSigningKeyRequired:
    """`--require-signing-key` pinned a key and this checkpoint carries
    no signature by it. Policy over an otherwise intact tree, whose
    signatures are all genuine and authorised: the pin narrows which
    authorised signer the verifier accepts."""

    tree_size: int
    public_key: str

    @classmethod
    def from_json(cls, payload: object) -> TreeSigningKeyRequired:
        data = _strict("signing-key-required tree", payload, {"status", "tree_size", "public_key"})
        return cls(tree_size=data["tree_size"], public_key=data["public_key"])


TreeVerification = (
    TreeIntact
    | TreeTampered
    | TreeChainBroken
    | TreeAnchorMismatch
    | TreeMalformedPack
    | TreeSignatureInvalid
    | TreeUnauthorizedKey
    | TreeSignatureRequired
    | TreeSigningKeyRequired
)


def parse_tree_verification(payload: object) -> TreeVerification:
    """The tamper-evidence verdict, the output of `evidence verify` and
    the `tree` half of `verify`."""
    return _by_status(
        payload,
        "a tree verdict",
        {
            "intact": TreeIntact.from_json,
            "tampered": TreeTampered.from_json,
            "chain_broken": TreeChainBroken.from_json,
            "anchor_mismatch": TreeAnchorMismatch.from_json,
            "malformed_pack": TreeMalformedPack.from_json,
            "signature_invalid": TreeSignatureInvalid.from_json,
            "unauthorized_key": TreeUnauthorizedKey.from_json,
            "signature_required": TreeSignatureRequired.from_json,
            "signing_key_required": TreeSigningKeyRequired.from_json,
        },
    )


@dataclass(frozen=True)
class ViewsIntact:
    """Every catalogued view's live definition matches its seal."""

    views_checked: int

    @classmethod
    def from_json(cls, payload: object) -> ViewsIntact:
        data = _strict("intact views", payload, {"status", "views_checked"})
        return cls(views_checked=data["views_checked"])


@dataclass(frozen=True)
class ViewsTampered:
    """The view surface disagrees with its seal: `mismatched` views were
    redefined in place, `missing` views lack a seal row or a live
    definition."""

    mismatched: list[str]
    missing: list[str]

    @classmethod
    def from_json(cls, payload: object) -> ViewsTampered:
        data = _strict("tampered views", payload, {"status", "mismatched", "missing"})
        return cls(
            mismatched=_str_list("mismatched", data["mismatched"]),
            missing=_str_list("missing", data["missing"]),
        )


@dataclass(frozen=True)
class ViewsNotSealed:
    """No seal table in the schema: the views predate sealing or were
    never applied. Visible, not a failure."""

    @classmethod
    def from_json(cls, payload: object) -> ViewsNotSealed:
        _strict("unsealed views", payload, {"status"})
        return cls()


ViewsVerification = ViewsIntact | ViewsTampered | ViewsNotSealed


def parse_views_verification(payload: object) -> ViewsVerification:
    """Parse a `views` verdict by its `status` tag."""
    return _by_status(
        payload,
        "a views verdict",
        {
            "intact": ViewsIntact.from_json,
            "tampered": ViewsTampered.from_json,
            "not_sealed": ViewsNotSealed.from_json,
        },
    )


@dataclass(frozen=True)
class WitnessVerdict:
    """One stored external witness, judged: ``verified`` (its token
    chains to a supplied trust anchor), ``untrusted`` (a sound token from
    an authority you did not name), ``unverified`` (sound, no anchors
    supplied), ``unsupported`` (this verifier cannot check it), or
    ``invalid`` (it does not vouch for this checkpoint - the one standing
    that fails the command). ``attested_at`` is the authority's time,
    present whenever the token could be read."""

    scheme: str
    submitted_to: str
    status: str
    attested_at: datetime | None = None
    detail: str | None = None

    @classmethod
    def from_json(cls, payload: object) -> WitnessVerdict:
        data = _strict(
            "witness verdict",
            payload,
            {"scheme", "submitted_to", "status"},
            optional={"attested_at", "detail"},
        )
        detail = data.get("detail")
        return cls(
            scheme=data["scheme"],
            submitted_to=data["submitted_to"],
            status=data["status"],
            attested_at=_optional_timestamp(data.get("attested_at")),
            detail=None if detail is None else str(detail),
        )


@dataclass(frozen=True)
class CheckpointWitnesses:
    """A checkpoint's witnesses, judged."""

    tree_size: int
    witnesses: list[WitnessVerdict]

    @classmethod
    def from_json(cls, payload: object) -> CheckpointWitnesses:
        data = _strict("checkpoint witnesses", payload, {"tree_size", "witnesses"})
        raw = data["witnesses"]
        if not isinstance(raw, list):
            raise EnvelopeError(f"`witnesses` must be a list, got {raw!r}")
        return cls(
            tree_size=data["tree_size"],
            witnesses=[WitnessVerdict.from_json(w) for w in raw],
        )


@dataclass(frozen=True)
class WitnessesReport:
    """What the external witnesses on a chain of checkpoints prove.
    ``earliest_attested_at`` is the earliest time any VERIFIED witness
    attests - the figure a "no later than" claim can rest on; absent when
    none verified."""

    checkpoints: list[CheckpointWitnesses]
    earliest_attested_at: datetime | None = None

    @classmethod
    def from_json(cls, payload: object) -> WitnessesReport:
        data = _strict(
            "witnesses report", payload, {"checkpoints"}, optional={"earliest_attested_at"}
        )
        raw = data["checkpoints"]
        if not isinstance(raw, list):
            raise EnvelopeError(f"`checkpoints` must be a list, got {raw!r}")
        return cls(
            checkpoints=[CheckpointWitnesses.from_json(c) for c in raw],
            earliest_attested_at=_optional_timestamp(data.get("earliest_attested_at")),
        )


@dataclass(frozen=True)
class RoleRebinding:
    """A login-role name seen under a new OID: the role was dropped and
    created again. The transitions are the last and first observed in the
    rows compared, not necessarily the last and first in the log."""

    role: str
    previous_oid: int
    last_observed_transition: str
    new_oid: int
    first_observed_transition: str

    @classmethod
    def from_json(cls, payload: object) -> RoleRebinding:
        data = _strict(
            "role rebinding",
            payload,
            {"role", "previous_oid", "last_observed_transition", "new_oid",
             "first_observed_transition"},
        )
        return cls(
            role=str(data["role"]),
            previous_oid=int(str(data["previous_oid"])),
            last_observed_transition=str(data["last_observed_transition"]),
            new_oid=int(str(data["new_oid"])),
            first_observed_transition=str(data["first_observed_transition"]),
        )


@dataclass(frozen=True)
class RoleRebindingsEvaluated:
    """Role rebindings compared over rows an intact verdict established.
    ``scope`` is ``complete_prefix``, ``window`` or ``selective``; rows
    without an OID could not show a change. A change is a finding, never a
    failure."""

    scope: str
    rows_with_oid: int
    rows_without_oid: int
    changes: list[RoleRebinding]

    @classmethod
    def from_json(cls, payload: object) -> RoleRebindingsEvaluated:
        data = _strict(
            "role rebindings",
            payload,
            {"status", "scope", "rows_with_oid", "rows_without_oid", "changes"},
        )
        changes = data["changes"]
        if not isinstance(changes, list):
            raise EnvelopeError(f"role rebindings: changes is not a list: {changes!r}")
        return cls(
            scope=str(data["scope"]),
            rows_with_oid=int(str(data["rows_with_oid"])),
            rows_without_oid=int(str(data["rows_without_oid"])),
            changes=[RoleRebinding.from_json(c) for c in changes],
        )


@dataclass(frozen=True)
class RoleRebindingsNotEvaluated:
    """The verdict did not establish the rows, so nothing is reported
    from them."""

    @classmethod
    def from_json(cls, payload: object) -> RoleRebindingsNotEvaluated:
        _strict("role rebindings", payload, {"status"})
        return cls()


RoleRebindings = RoleRebindingsEvaluated | RoleRebindingsNotEvaluated


def parse_role_rebindings(payload: object) -> RoleRebindings:
    """Parse the role-rebinding finding by its `status` tag."""
    return _by_status(
        payload,
        "a role-rebinding finding",
        {
            "evaluated": RoleRebindingsEvaluated.from_json,
            "not_evaluated": RoleRebindingsNotEvaluated.from_json,
        },
    )


@dataclass(frozen=True)
class VerifyReport:
    """The `verify` envelope: the replay verdict beside the
    tamper-evidence verdict, the login roles seen under a new OID in the
    rows the tree covered, plus the generated-view-surface verdict when
    the verifier asked for it (`--views-schema`), plus what the
    checkpoints' external witnesses prove when any carries one."""

    replay: ReplayConsistent | ReplayDivergent
    tree: TreeVerification
    role_rebindings: RoleRebindings
    views: ViewsVerification | None = None
    witnesses: WitnessesReport | None = None

    @classmethod
    def from_json(cls, payload: object) -> VerifyReport:
        data = _strict(
            "verify report",
            payload,
            {"replay", "tree", "role_rebindings"},
            optional={"views", "witnesses"},
        )
        views = data.get("views")
        witnesses = data.get("witnesses")
        return cls(
            replay=parse_verify_outcome(data["replay"]),
            tree=parse_tree_verification(data["tree"]),
            role_rebindings=parse_role_rebindings(data["role_rebindings"]),
            views=None if views is None else parse_views_verification(views),
            witnesses=None if witnesses is None else WitnessesReport.from_json(witnesses),
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
    def from_json(cls, payload: object) -> TreeHeadSignature:
        data = _strict(
            "tree-head signature", payload, {"key_id", "purpose", "public_key", "signature"}
        )
        return cls(
            key_id=data["key_id"],
            purpose=data["purpose"],
            public_key=data["public_key"],
            signature=data["signature"],
        )


@dataclass(frozen=True)
class Witness:
    """One external witness to a tree head: the authority's exact
    response (`proof`, base64) and where it was obtained. The attested
    time and whether it verifies are read from the proof by the
    verifier, never stored."""

    scheme: str
    proof: str
    submitted_to: str

    @classmethod
    def from_json(cls, payload: object) -> Witness:
        data = _strict("witness", payload, {"scheme", "proof", "submitted_to"})
        return cls(scheme=data["scheme"], proof=data["proof"], submitted_to=data["submitted_to"])


def _parse_witnesses(data: dict[str, object]) -> list[Witness]:
    raw = data.get("witnesses", [])
    if not isinstance(raw, list):
        raise EnvelopeError(f"`witnesses` must be a list, got {raw!r}")
    return [Witness.from_json(w) for w in raw]


def _parse_signatures(data: dict[str, object]) -> list[TreeHeadSignature]:
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
    signatures: list[TreeHeadSignature] = field(default_factory=list)
    witnesses: list[Witness] = field(default_factory=list)

    @classmethod
    def from_json(cls, payload: object) -> Checkpoint:
        data = _strict(
            "checkpoint",
            payload,
            {"tree_size", "root_hash", "prev_checkpoint_hash", "checkpoint_hash"},
            {"signatures", "witnesses"},
        )
        return cls(
            tree_size=data["tree_size"],
            root_hash=data["root_hash"],
            prev_checkpoint_hash=data["prev_checkpoint_hash"],
            checkpoint_hash=data["checkpoint_hash"],
            signatures=_parse_signatures(data),
            witnesses=_parse_witnesses(data),
        )


def _checkpoint_from_flattened(name: str, payload: object) -> Checkpoint:
    # The `checkpoint` command flattens the checkpoint fields beside a
    # `status` tag, so the bare-checkpoint parser (which forbids
    # `status`) cannot read it directly.
    data = _strict(
        name,
        payload,
        {"status", "tree_size", "root_hash", "prev_checkpoint_hash", "checkpoint_hash"},
        {"signatures", "witnesses"},
    )
    return Checkpoint(
        tree_size=data["tree_size"],
        root_hash=data["root_hash"],
        prev_checkpoint_hash=data["prev_checkpoint_hash"],
        checkpoint_hash=data["checkpoint_hash"],
        signatures=_parse_signatures(data),
        witnesses=_parse_witnesses(data),
    )


@dataclass(frozen=True)
class CheckpointCreated:
    checkpoint: Checkpoint

    @classmethod
    def from_json(cls, payload: object) -> CheckpointCreated:
        return cls(checkpoint=_checkpoint_from_flattened("created checkpoint", payload))


@dataclass(frozen=True)
class CheckpointNoNewRows:
    """The stable prefix had not grown; the current head, returned
    unchanged - still a usable anchor."""

    checkpoint: Checkpoint

    @classmethod
    def from_json(cls, payload: object) -> CheckpointNoNewRows:
        return cls(checkpoint=_checkpoint_from_flattened("no-new-rows checkpoint", payload))


def parse_checkpoint_outcome(payload: object) -> CheckpointCreated | CheckpointNoNewRows:
    return _by_status(
        payload,
        "a checkpoint outcome",
        {
            "created": CheckpointCreated.from_json,
            "no_new_rows": CheckpointNoNewRows.from_json,
        },
    )


@dataclass(frozen=True)
class PrefixPackManifest:
    """Line 1 of a complete-prefix evidence pack. The next
    ``checkpoint_count`` lines are checkpoints, then exactly ``tree_size``
    audit rows in log order."""

    pack_format_version: int
    pack_kind: str
    tree_size: int
    root_hash: str
    checkpoint_hash: str
    checkpoint_count: int

    @classmethod
    def from_json(cls, payload: object) -> PrefixPackManifest:
        data = _strict(
            "prefix pack manifest",
            payload,
            {
                "pack_format_version",
                "pack_kind",
                "tree_size",
                "root_hash",
                "checkpoint_hash",
                "checkpoint_count",
            },
        )
        return cls(
            pack_format_version=data["pack_format_version"],
            pack_kind=data["pack_kind"],
            tree_size=data["tree_size"],
            root_hash=data["root_hash"],
            checkpoint_hash=data["checkpoint_hash"],
            checkpoint_count=data["checkpoint_count"],
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
    def from_json(cls, payload: object) -> WindowPackManifest:
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


def _str_list(label: str, value: object) -> list[str]:
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
    proof: list[str]

    @classmethod
    def from_json(cls, payload: object) -> RowInclusionProof:
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
    consistency_proof: list[str]
    rows: list[AuditRow]
    inclusion_proofs: list[RowInclusionProof]

    @classmethod
    def from_json(cls, payload: object) -> WindowEvidencePack:
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
    def from_json(cls, payload: object) -> WindowIntact:
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
    def from_json(cls, payload: object) -> WindowInconsistentExtension:
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
    def from_json(cls, payload: object) -> WindowRowNotIncluded:
        data = _strict("row-not-included window", payload, {"status", "leaf_index"})
        return cls(leaf_index=data["leaf_index"])


@dataclass(frozen=True)
class WindowAnchorMismatch:
    """An externally held anchor disagrees with the pack's from-checkpoint."""

    tree_size: int
    anchor_checkpoint_hash: str
    pack_checkpoint_hash: str

    @classmethod
    def from_json(cls, payload: object) -> WindowAnchorMismatch:
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
    def from_json(cls, payload: object) -> WindowSignatureInvalid:
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
    def from_json(cls, payload: object) -> WindowSignatureRequired:
        data = _strict("signature-required window", payload, {"status", "tree_size"})
        return cls(tree_size=data["tree_size"])


@dataclass(frozen=True)
class WindowMalformed:
    """The window pack is not a well-formed v2 artefact - it never had a
    chance to prove anything."""

    detail: str

    @classmethod
    def from_json(cls, payload: object) -> WindowMalformed:
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
    return _by_status(
        payload,
        "a window verdict",
        {
            "intact": WindowIntact.from_json,
            "inconsistent_extension": WindowInconsistentExtension.from_json,
            "row_not_included": WindowRowNotIncluded.from_json,
            "anchor_mismatch": WindowAnchorMismatch.from_json,
            "signature_invalid": WindowSignatureInvalid.from_json,
            "signature_required": WindowSignatureRequired.from_json,
            "malformed": WindowMalformed.from_json,
        },
    )


@dataclass(frozen=True)
class SelectivePackManifest:
    """The selective pack's header: the covering checkpoint's coordinates."""

    pack_format_version: int
    pack_kind: str
    tree_size: int
    root_hash: str
    checkpoint_hash: str

    @classmethod
    def from_json(cls, payload: object) -> SelectivePackManifest:
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
    rows: list[AuditRow]
    inclusion_proofs: list[RowInclusionProof]

    @classmethod
    def from_json(cls, payload: object) -> SelectiveEvidencePack:
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
    def from_json(cls, payload: object) -> SelectiveIntact:
        data = _strict("intact selective", payload, {"status", "tree_size", "rows_disclosed"})
        return cls(tree_size=data["tree_size"], rows_disclosed=data["rows_disclosed"])


@dataclass(frozen=True)
class SelectiveRowNotIncluded:
    """A disclosed row is not the row the checkpoint committed to at its
    declared position."""

    leaf_index: int

    @classmethod
    def from_json(cls, payload: object) -> SelectiveRowNotIncluded:
        data = _strict("row-not-included selective", payload, {"status", "leaf_index"})
        return cls(leaf_index=data["leaf_index"])


@dataclass(frozen=True)
class SelectiveAnchorMismatch:
    """An externally held anchor disagrees with the pack's checkpoint."""

    tree_size: int
    anchor_checkpoint_hash: str
    pack_checkpoint_hash: str

    @classmethod
    def from_json(cls, payload: object) -> SelectiveAnchorMismatch:
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
    def from_json(cls, payload: object) -> SelectiveSignatureInvalid:
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
    def from_json(cls, payload: object) -> SelectiveSignatureRequired:
        data = _strict("signature-required selective", payload, {"status", "tree_size"})
        return cls(tree_size=data["tree_size"])


@dataclass(frozen=True)
class SelectiveMalformed:
    """The selective pack is not a well-formed v3 artefact - it never had a
    chance to prove anything."""

    detail: str

    @classmethod
    def from_json(cls, payload: object) -> SelectiveMalformed:
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
    return _by_status(
        payload,
        "a selective verdict",
        {
            "intact": SelectiveIntact.from_json,
            "row_not_included": SelectiveRowNotIncluded.from_json,
            "anchor_mismatch": SelectiveAnchorMismatch.from_json,
            "signature_invalid": SelectiveSignatureInvalid.from_json,
            "signature_required": SelectiveSignatureRequired.from_json,
            "malformed": SelectiveMalformed.from_json,
        },
    )


_PackVerdict = TypeVar("_PackVerdict")


@dataclass(frozen=True)
class PackVerificationReport(Generic[_PackVerdict]):
    """The `verify-pack` envelope: the pack's own verdict, the login roles
    seen under a new OID among its rows, and, when asked for, what its
    checkpoints' witnesses prove. ``witnesses`` is absent unless requested,
    or when no checkpoint in the pack carries one."""

    verdict: _PackVerdict
    role_rebindings: RoleRebindings
    witnesses: WitnessesReport | None = None

    @classmethod
    def from_json(
        cls, payload: object, parse_verdict: Callable[[object], _PackVerdict]
    ) -> PackVerificationReport[_PackVerdict]:
        data = _strict(
            "pack verification report",
            payload,
            {"verdict", "role_rebindings"},
            optional={"witnesses"},
        )
        witnesses = data.get("witnesses")
        return cls(
            verdict=parse_verdict(data["verdict"]),
            role_rebindings=parse_role_rebindings(data["role_rebindings"]),
            witnesses=None if witnesses is None else WitnessesReport.from_json(witnesses),
        )
