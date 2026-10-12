"""Every envelope parser refuses what the published schema refuses.

The attacker modelled is a binary, or anything standing in for it on
stdout, that prints a payload with the right keys and a wrong value:
an integer where the schema says a string, a string where it pins a
constant, an empty array it says must hold one item. Such a payload
must raise `EnvelopeError` and nothing else, never parse into an
object whose annotations lie.

Each golden is mutated at every node, and a small structural walker
over `result.json` judges each mutant: types, constants, enums,
patterns, minimums, required and extra keys, through `$ref` and
`oneOf`. It reads no formats, so a mutant it accepts is not judged
valid, only not judged; the goldens themselves are the accepting side.
"""

import copy
import json
import re
import unittest
from collections import Counter
from pathlib import Path

from _support import GOLDEN_DIR, golden

from python_client import envelopes

SCHEMA = json.loads(
    (Path(__file__).resolve().parents[3] / "src" / "schemas" / "result.json").read_text()
)
DEFS = SCHEMA["$defs"]


def _list_of(parse):
    return lambda payload: envelopes.parse_list(payload, parse)


def _pack_report(parse_verdict):
    return lambda payload: envelopes.PackVerificationReport.from_json(payload, parse_verdict)


E = envelopes
# Each golden with its `$defs` entry and the parser the client reads it
# through. A union's golden is also read through its arm's own
# `from_json`, which must refuse what the union refuses.
CASES = {
    "committed.json": ("committed", E.parse_run_outcome),
    "rejected.json": ("rejected", E.parse_run_outcome),
    "rejected_with_explanation.json": ("rejected", E.parse_run_outcome),
    "rejected_with_explanation_and_witness.json": ("rejected", E.parse_run_outcome),
    "rejected_with_witness.json": ("rejected", E.parse_run_outcome),
    "rejected_gate_with_witness.json": ("rejected", E.parse_run_outcome),
    "rejected_gate_with_compared.json": ("rejected", E.parse_run_outcome),
    "traced_committed.json": ("traced_envelope", E.TracedEnvelope.from_json),
    "traced_rejected.json": ("traced_envelope", E.TracedEnvelope.from_json),
    "traced_rejected_at_gate.json": ("traced_envelope", E.TracedEnvelope.from_json),
    "traced_rejected_at_bind.json": ("traced_envelope", E.TracedEnvelope.from_json),
    "traced_errored.json": ("traced_envelope", E.TracedEnvelope.from_json),
    "batch_committed_receipt.json": ("batch_receipt", E.BatchReceipt.from_json),
    "batch_rejected_receipt.json": ("batch_receipt", E.BatchReceipt.from_json),
    "batch_rejected_receipt_with_compared.json": ("batch_receipt", E.BatchReceipt.from_json),
    "batch_error_receipt.json": ("batch_receipt", E.BatchReceipt.from_json),
    "batch_error_receipt_not_committed.json": ("batch_receipt", E.BatchReceipt.from_json),
    "transact_committed.json": ("atomic_outcome", E.parse_atomic_outcome),
    "transact_rejected.json": ("atomic_outcome", E.parse_atomic_outcome),
    "transact_rejected_session.json": ("atomic_outcome", E.parse_atomic_outcome),
    "transact_error.json": ("atomic_outcome", E.RequestError.from_json),
    "propose_error_not_committed.json": ("request_error", E.RequestError.from_json),
    "session_ready.json": ("session_ready", E.SessionReady.from_json),
    "session_error_receipt.json": ("session_error_receipt", E.SessionErrorReceipt.from_json),
    "session_error_receipt_commit_outcome_unknown.json": (
        "session_error_receipt",
        E.SessionErrorReceipt.from_json,
    ),
    "explanation_admissible.json": ("explanation", E.Explanation.from_json),
    "explanation_gate.json": ("explanation", E.Explanation.from_json),
    "explanation_gate_named.json": ("explanation", E.Explanation.from_json),
    "explanation_invariant.json": ("explanation", E.Explanation.from_json),
    "explanation_invariant_no_witness.json": ("explanation", E.Explanation.from_json),
    "explanation_error.json": ("explanation", E.Explanation.from_json),
    "outbox_row.json": ("outbox_row", E.OutboxRow.from_json),
    "outbox_claim.json": ("outbox_claim", E.parse_outbox_claim),
    "outbox_claim_null.json": ("outbox_claim", E.parse_outbox_claim),
    "outbox_update_applied.json": ("outbox_update", E.OutboxUpdate.from_json),
    "outbox_update_lease_lost.json": ("outbox_update", E.OutboxUpdate.from_json),
    "migration_report_behind.json": ("migration_report", E.MigrationReport.from_json),
    "migration_report_applied.json": ("migration_report", E.MigrationReport.from_json),
    "migration_report_ahead.json": ("migration_report", E.MigrationReport.from_json),
    "migration_report_below_baseline.json": ("migration_report", E.MigrationReport.from_json),
    "migration_report_below_baseline_no_record.json": (
        "migration_report",
        E.MigrationReport.from_json,
    ),
    "provision_report_applied.json": ("provision_report", E.ProvisionReport.from_json),
    "provision_report_dry_run.json": ("provision_report", E.ProvisionReport.from_json),
    "provision_report_conflict.json": ("provision_report", E.ProvisionReport.from_json),
    "provision_report_pruned.json": ("provision_report", E.ProvisionReport.from_json),
    "provision_report_current.json": ("provision_report", E.ProvisionReport.from_json),
    "rejection_row.json": ("rejection_row", E.RejectionRow.from_json),
    "rejection_row_gate.json": ("rejection_row", E.RejectionRow.from_json),
    "rejection_row_gate_with_witness.json": ("rejection_row", E.RejectionRow.from_json),
    "rejection_row_gate_with_compared.json": ("rejection_row", E.RejectionRow.from_json),
    "coverage_report.json": ("coverage_report", E.CoverageReport.from_json),
    "audit_row.json": ("audit_row", E.AuditRow.from_json),
    "audit_row_attested.json": ("audit_row", E.AuditRow.from_json),
    "audit_row_attested_with_role_oid.json": ("audit_row", E.AuditRow.from_json),
    "audit_row_self_describing.json": ("audit_row", E.AuditRow.from_json),
    "audit_row_model_hash.json": ("audit_row", E.AuditRow.from_json),
    "audit_row_semantics_version.json": ("audit_row", E.AuditRow.from_json),
    "audit_row_drawn_subjects.json": ("audit_row", E.AuditRow.from_json),
    "audit_row_named.json": ("audit_row_named", E.AuditRowNamed.from_json),
    "check_report.json": ("check_report", E.CheckReport.from_json),
    "check_report_routes.json": ("check_report", E.CheckReport.from_json),
    "check_report_cases.json": ("check_report", E.CheckReport.from_json),
    "check_report_no_invariants.json": ("check_report", E.CheckReport.from_json),
    "hash_report.json": ("hash_report", E.HashReport.from_json),
    "init_report.json": ("init_report", E.InitReport.from_json),
    "init_report_least_privilege.json": ("init_report", E.InitReport.from_json),
    "named_claim.json": ("named_claim", E.NamedClaim.from_json),
    "claim_instances.json": ("claim_instance_array", _list_of(E.ClaimInstance.from_json)),
    "named_claims.json": ("named_claim_array", _list_of(E.NamedClaim.from_json)),
    "claim_reads.json": ("claim_read_array", _list_of(E.ClaimRead.from_json)),
    "named_claim_reads.json": ("named_claim_read_array", _list_of(E.NamedClaimRead.from_json)),
    "refresh_derived_report.json": ("refresh_derived_report", E.RefreshDerivedReport.from_json),
    "refresh_derived_report_no_transitions.json": (
        "refresh_derived_report",
        E.RefreshDerivedReport.from_json,
    ),
    "verify_report_consistent.json": ("verify_report", E.VerifyReport.from_json),
    "verify_report_divergent.json": ("verify_report", E.VerifyReport.from_json),
    "verify_report_with_views.json": ("verify_report", E.VerifyReport.from_json),
    "verify_report_witnessed.json": ("verify_report", E.VerifyReport.from_json),
    "views_verification_intact.json": ("views_verification", E.parse_views_verification),
    "views_verification_tampered.json": ("views_verification", E.parse_views_verification),
    "views_verification_not_sealed.json": ("views_verification", E.parse_views_verification),
    "checkpoint_created.json": ("checkpoint_outcome", E.parse_checkpoint_outcome),
    "checkpoint_created_signed.json": ("checkpoint_outcome", E.parse_checkpoint_outcome),
    "checkpoint_no_new_rows.json": ("checkpoint_outcome", E.parse_checkpoint_outcome),
    "checkpoint_created_witnessed.json": ("checkpoint_outcome", E.parse_checkpoint_outcome),
    "checkpoint_witnessed.json": ("checkpoint", E.Checkpoint.from_json),
    "pack_verification_report.json": (
        "pack_verification_report",
        _pack_report(E.parse_window_verification),
    ),
    "pack_verification_report_selective_rebinding.json": (
        "pack_verification_report",
        _pack_report(E.parse_selective_verification),
    ),
    "evaluation_receipt.json": ("evaluation_receipt", E.EvaluationReceipt.from_json),
    "witness_verdict_invalid.json": ("witness_verdict", E.WitnessVerdict.from_json),
    "witness_verdict_unverified.json": ("witness_verdict", E.WitnessVerdict.from_json),
    "witness_verdict_unsupported.json": ("witness_verdict", E.WitnessVerdict.from_json),
    "prefix_pack_manifest.json": ("prefix_pack_manifest", E.PrefixPackManifest.from_json),
    "window_evidence_pack.json": ("window_evidence_pack", E.WindowEvidencePack.from_json),
    "selective_evidence_pack.json": (
        "selective_evidence_pack",
        E.SelectiveEvidencePack.from_json,
    ),
}
CASES.update(
    {
        f"receipt_verification_report_{kind}.json": (
            "receipt_verification_report",
            E.ReceiptVerificationReport.from_json,
        )
        for kind in (
            "reproduced",
            "differs",
            "mismatched",
            "query_unknown",
            "errored",
            "not_re_evaluated",
            "malformed",
            "not_complete",
        )
    }
)
CASES.update(
    {
        f"tree_verification_{kind}.json": ("tree_verification", E.parse_tree_verification)
        for kind in (
            "chain_broken",
            "anchor_mismatch",
            "malformed_pack",
            "unsupported_pack",
            "signature_invalid",
            "unauthorized_key",
            "signature_required",
            "signing_key_required",
        )
    }
)
CASES.update(
    {
        f"selective_verification_{kind}.json": (
            "selective_verification",
            E.parse_selective_verification,
        )
        for kind in (
            "intact",
            "row_not_included",
            "anchor_mismatch",
            "signature_invalid",
            "signature_required",
            "malformed",
        )
    }
)
CASES.update(
    {
        f"window_verification_{kind}.json": ("window_verification", E.parse_window_verification)
        for kind in (
            "intact",
            "inconsistent_extension",
            "row_not_included",
            "anchor_mismatch",
            "signature_invalid",
            "signature_required",
            "malformed",
        )
    }
)

# Goldens of reports the client has no parser for, each with its reason.
NOT_PARSED = {
    "roles_move_report.json": "`roles move` has no client method",
    "score_report.json": "scoring has no client method; read by the CLI's own consumers",
    "score_report_split.json": "scoring has no client method",
    "batch_score.json": "scoring has no client method",
}

# Measured counts, rounded down: below them the battery has stopped
# exercising a whole kind of value.
MIN_REFUSED_MUTANTS = 15000
MIN_REFUSED_BY_ORIGINAL_KIND = {
    "string": 7000,
    "integer": 1200,
    "boolean": 300,
    "array": 1500,
    "object": 4000,
    "null": 80,
}


def _same(a, b) -> bool:
    return json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True)


def _kind(value) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    return "object"


def conforms(value, schema) -> bool:
    """The Rust contract test's structural walker, plus patterns and
    `minItems`. Formats are not read."""
    if "$ref" in schema:
        return conforms(value, DEFS[schema["$ref"].removeprefix("#/$defs/")])
    if "oneOf" in schema:
        return any(conforms(value, branch) for branch in schema["oneOf"])
    if "const" in schema:
        return _same(value, schema["const"])
    if "enum" in schema:
        return any(_same(value, allowed) for allowed in schema["enum"])
    types = schema.get("type")
    if types is None:
        return True
    if isinstance(types, str):
        types = [types]
    if _kind(value) not in types:
        return False
    if isinstance(value, int) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            return False
        if "maximum" in schema and value > schema["maximum"]:
            return False
    if isinstance(value, str) and "pattern" in schema:
        if not re.search(schema["pattern"], value):
            return False
    if isinstance(value, list):
        if len(value) < schema.get("minItems", 0):
            return False
        if "items" in schema and not all(conforms(v, schema["items"]) for v in value):
            return False
    if isinstance(value, dict):
        if any(key not in value for key in schema.get("required", [])):
            return False
        for trigger, needed in schema.get("dependentRequired", {}).items():
            if trigger in value and any(key not in value for key in needed):
                return False
        properties = schema.get("properties", {})
        extra = schema.get("additionalProperties")
        for key, item in value.items():
            if key in properties:
                if not conforms(item, properties[key]):
                    return False
            elif extra is False:
                return False
            elif isinstance(extra, dict) and not conforms(item, extra):
                return False
    return True


def nodes(value, path=()):
    yield path, value
    if isinstance(value, dict):
        for key, item in value.items():
            yield from nodes(item, (*path, key))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            yield from nodes(item, (*path, index))


def replacements(value):
    """A value of every other JSON kind, and wrong values of its own kind:
    a string no constant or pattern matches, a negative integer, the empty
    array, the other boolean, and an object with a key added or one
    taken away."""
    out = [r for r in (None, True, 7, 1.5, "x", [], {}) if _kind(r) != _kind(value)]
    if isinstance(value, bool):
        out.append(not value)
    elif isinstance(value, int):
        out.append(-1)
    elif isinstance(value, str):
        out.append("\u0000")
    elif isinstance(value, list) and value:
        out.append([])
    elif isinstance(value, dict):
        out.append({**value, "unexpected": 1})
        out.extend({k: v for k, v in value.items() if k != key} for key in value)
    return out


def mutate(payload, path, replacement):
    if not path:
        return replacement
    mutant = copy.deepcopy(payload)
    target = mutant
    for step in path[:-1]:
        target = target[step]
    target[path[-1]] = replacement
    return mutant


def arm_parser(parse, payload):
    """The arm class's own `from_json`, when a union's golden parses
    through it unchanged."""
    from_json = getattr(type(parse(payload)), "from_json", None)
    if from_json is None or from_json == parse:
        return None
    try:
        from_json(payload)
    except Exception:
        return None
    return from_json


class EveryGoldenIsAccountedFor(unittest.TestCase):
    def test_every_golden_has_a_case_or_a_reason(self):
        goldens = {p.name for p in GOLDEN_DIR.glob("*.json")}
        self.assertEqual(sorted(goldens - set(CASES) - set(NOT_PARSED)), [])
        self.assertEqual(sorted((set(CASES) | set(NOT_PARSED)) - goldens), [])

    def test_every_golden_conforms_and_parses(self):
        for name, (definition, parse) in sorted(CASES.items()):
            payload = golden(name)
            self.assertTrue(conforms(payload, DEFS[definition]), f"{name} vs {definition}")
            parse(payload)


class MutantsTheSchemaRefusesAreRefused(unittest.TestCase):
    def test_every_refused_mutant_raises_envelope_error(self):
        failures = []
        refused = Counter()
        for name, (definition, parse) in sorted(CASES.items()):
            payload = golden(name)
            parsers = [("parser", parse)]
            arm = arm_parser(parse, payload)
            if arm is not None:
                parsers.append((f"{arm.__qualname__}", arm))
            for path, value in nodes(payload):
                for replacement in replacements(value):
                    mutant = mutate(payload, path, replacement)
                    if conforms(mutant, DEFS[definition]):
                        continue
                    refused[_kind(value)] += 1
                    for label, parser in parsers:
                        try:
                            parser(mutant)
                        except envelopes.EnvelopeError:
                            continue
                        except Exception as exc:
                            outcome = f"raised {type(exc).__name__}: {exc}"
                        else:
                            outcome = "accepted it"
                        failures.append(
                            f"{name} via {label} at {list(path)}: "
                            f"{value!r} -> {replacement!r} should raise EnvelopeError, "
                            f"{outcome}"[:400]
                        )
        self.assertEqual(
            failures[:40],
            [],
            f"{len(failures)} schema-refused mutants were not refused with EnvelopeError",
        )
        self.assertGreaterEqual(sum(refused.values()), MIN_REFUSED_MUTANTS)
        for kind, floor in MIN_REFUSED_BY_ORIGINAL_KIND.items():
            self.assertGreaterEqual(refused[kind], floor, f"too few mutants of {kind} values")


if __name__ == "__main__":
    unittest.main()
