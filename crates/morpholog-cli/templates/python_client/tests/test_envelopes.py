"""The envelope models against the SAME golden files the Rust contract
test pins byte-equal to the binary's real serialization - one sample
set holding the binary, result.json, and this client together."""

import json
import sys
import unittest
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path

from _support import GOLDEN_DIR, TEMPLATES_DIR, add_client_to_path, golden

add_client_to_path()

from python_client import envelopes



class RunOutcomes(unittest.TestCase):
    def test_committed_decodes_every_value_kind(self):
        outcome = envelopes.parse_run_outcome(golden("committed.json"))
        self.assertIsInstance(outcome, envelopes.Committed)
        self.assertEqual(outcome.actor, "alex")
        self.assertEqual(
            outcome.asserted_claims[0].args,
            [
                "acct_1",
                Decimal("100.50"),
                True,
                date(2026, 6, 1),
                datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc),
                "PT6H",
                Decimal("25000"),
                ["nested"],
            ],
        )
        self.assertEqual(outcome.emitted_intents[0].name, "AccountOpened")

    def test_a_refusal_carries_the_values_the_rule_was_reading(self):
        # The point of a structured witness: read the offending value,
        # never parse it out of the reason string.
        outcome = envelopes.parse_run_outcome(golden("rejected_with_witness.json"))
        self.assertEqual([w.var for w in outcome.witness], ["account", "exposure"])
        self.assertEqual(outcome.witness[0].value, "acct_1")
        self.assertEqual(outcome.witness[1].value, Decimal("105.50"))

    def test_a_refusal_names_the_rule_that_refused(self):
        # The stable identifier, and the whole reason it exists: `reason` is
        # prose that any rewording changes, so a test that holds it breaks
        # for the wrong reason. `rule` does not move.
        outcome = envelopes.parse_run_outcome(golden("rejected.json"))
        self.assertEqual(outcome.rule, "no_flagged_accounts")

    def test_rejected_with_and_without_explanation(self):
        bare = envelopes.parse_run_outcome(golden("rejected.json"))
        self.assertIsInstance(bare, envelopes.Rejected)
        self.assertIsNone(bare.explanation)
        # The JSON omits `witness` entirely here - that is what keeps
        # pre-witness envelopes byte-identical - and the parsed model
        # presents it as an empty list, so callers never branch on absence.
        self.assertNotIn("witness", golden("rejected.json"))
        self.assertEqual(bare.witness, [])
        explained = envelopes.parse_run_outcome(golden("rejected_with_explanation.json"))
        self.assertIsInstance(
            explained.explanation.rejection, envelopes.InvariantRejection
        )
        self.assertEqual(explained.witness, [])

    def test_explanation_and_witness_arrive_together(self):
        # --explain-on-reject is the path an operator diagnosing a refusal
        # reaches for, so it must carry both: the why and the values.
        both = envelopes.parse_run_outcome(
            golden("rejected_with_explanation_and_witness.json")
        )
        self.assertIsInstance(both.explanation.rejection, envelopes.InvariantRejection)
        self.assertEqual([w.var for w in both.witness], ["account", "exposure"])
        self.assertEqual(both.witness[1].value, Decimal("105.50"))

    def test_traced_envelopes(self):
        committed = envelopes.TracedEnvelope.from_json(golden("traced_committed.json"))
        self.assertIsInstance(committed.result, envelopes.Committed)
        errored = envelopes.TracedEnvelope.from_json(golden("traced_errored.json"))
        self.assertIsInstance(errored.result, envelopes.Errored)
        # A kernel error is coded like every other known non-commit, and
        # keeps its trace.
        self.assertEqual(errored.result.code, "kernel_error")
        self.assertTrue(errored.trace)

    def test_a_one_shot_error_object_parses_and_its_codes_are_the_schema_s(self):
        error = envelopes.RequestError.from_json(golden("propose_error_not_committed.json"))
        self.assertEqual(error.code, "not_committed")
        schema = json.loads(
            (GOLDEN_DIR.parents[2] / "src" / "schemas" / "result.json").read_text()
        )
        published = set(schema["$defs"]["propose_error_code"]["enum"])
        self.assertEqual(envelopes.PROPOSE_ERROR_CODES, published)
        self.assertEqual(
            envelopes.NOTHING_RECORDED_CODES, published - {"commit_outcome_unknown"}
        )

    def test_the_developer_intro_recipe_lists_the_same_codes(self):
        # The hand-written recipe in the developer introduction carries its
        # own copy of the list; it must not drift from the client's.
        intro = (GOLDEN_DIR.parents[4] / "docs" / "developer-intro.md").read_text()
        start = intro.index("NOTHING_RECORDED = {")
        literal = intro[start + len("NOTHING_RECORDED = ") : intro.index("}", start) + 1]
        self.assertEqual(eval(literal), set(envelopes.NOTHING_RECORDED_CODES))

    def test_a_traced_error_with_another_code_is_drift(self):
        payload = dict(golden("traced_errored.json")["result"], code="not_committed")
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.Errored.from_json(payload)

    def test_a_trace_is_typed_steps_not_raw_dicts(self):
        # The trace used to arrive as list[object] - a pinned wrapper around
        # an unpinned payload, so an embedder reading a step was parsing
        # ad-hoc JSON with no floor under it.
        traced = envelopes.TracedEnvelope.from_json(golden("traced_committed.json"))
        kinds = [type(step).__name__ for step in traced.trace]
        self.assertIn("RequireStep", kinds)
        self.assertIn("ForStep", kinds)

        gate = next(s for s in traced.trace if isinstance(s, envelopes.RequireStep))
        # The stable identifier and the outcome, both read as attributes.
        self.assertEqual(gate.name, "not_yet_approved")
        self.assertIsInstance(gate.outcome, envelopes.RequireHeld)

        lookup = next(s for s in traced.trace if isinstance(s, envelopes.BindStep))
        self.assertIsInstance(lookup.outcome, envelopes.BindBound)
        # Bindings decode to values, and share the witness shape rather
        # than being positional pairs.
        self.assertEqual(lookup.outcome.bindings[0].var, "account")
        self.assertEqual(lookup.outcome.bindings[0].value, "acct_1")

        # A `for` carries a sub-trace per item, recursively typed.
        loop = next(s for s in traced.trace if isinstance(s, envelopes.ForStep))
        self.assertEqual(loop.binding, "item")
        self.assertIsInstance(loop.iterations[0].trace[0], envelopes.AssertStep)

    def test_every_traced_golden_parses_and_covers_every_arm(self):
        # The client-side mirror of the binary's arm-coverage gate. Parsing
        # only the happy-path trace would leave the parsers for the other
        # arms unproven, which is how four of them shipped unexercised.
        from _support import GOLDEN_DIR

        step_types, outcome_types = set(), set()
        for path in sorted(GOLDEN_DIR.glob("traced_*.json")):
            with path.open() as fh:
                traced = envelopes.TracedEnvelope.from_json(json.load(fh))

            def walk(steps):
                for step in steps:
                    step_types.add(type(step).__name__)
                    outcome = getattr(step, "outcome", None)
                    if outcome is not None:
                        outcome_types.add(type(outcome).__name__)
                    for it in getattr(step, "iterations", []):
                        walk(it.trace)

            walk(traced.trace)

        self.assertEqual(
            step_types,
            {
                "RequireStep",
                "BindStep",
                "LetStep",
                "LetNewSubjectStep",
                "AssertStep",
                "RetractStep",
                "EmitStep",
                "ForStep",
                "InvariantCheckStep",
            },
        )
        self.assertEqual(
            outcome_types,
            {"RequireHeld", "RequireRejected", "BindBound", "BindNoMatch", "BindMultipleMatches"},
        )

    def test_a_refusing_trace_names_the_lookup_that_failed(self):
        traced = envelopes.TracedEnvelope.from_json(golden("traced_rejected_at_bind.json"))
        step = traced.trace[-1]
        self.assertIsInstance(step, envelopes.BindStep)
        self.assertEqual(step.name, "the_account")
        self.assertIsInstance(step.outcome, envelopes.BindNoMatch)
        self.assertEqual(step.outcome.directly_missing_claims[0].predicate, "Account")

    def test_a_refused_gate_carries_its_reason_and_name(self):
        traced = envelopes.TracedEnvelope.from_json(golden("traced_rejected_at_gate.json"))
        gate = next(s for s in traced.trace if isinstance(s, envelopes.RequireStep))
        self.assertEqual(gate.name, "not_yet_approved")
        self.assertIsInstance(gate.outcome, envelopes.RequireRejected)
        self.assertIn("not_yet_approved", gate.outcome.reason)

    def test_a_failed_invariant_check_is_visible_in_the_trace(self):
        traced = envelopes.TracedEnvelope.from_json(golden("traced_rejected.json"))
        check = next(s for s in traced.trace if isinstance(s, envelopes.InvariantCheckStep))
        self.assertFalse(check.held)
        self.assertEqual(check.name, "approved_accounts_are_known")

    def test_an_unknown_trace_step_kind_is_drift(self):
        payload = golden("traced_committed.json")
        payload["trace"][0]["kind"] = "teleport"
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.TracedEnvelope.from_json(payload)


class RejectionLog(unittest.TestCase):
    def test_an_invariant_row_carries_its_witness_decoded(self):
        row = envelopes.RejectionRow.from_json(golden("rejection_row.json"))
        self.assertEqual(row.kind, "invariant")
        self.assertEqual(row.rule, "line_net_is_the_rounded_recompute")
        self.assertEqual(row.invariant_version, 1)
        # Values arrive decoded, not as tagged dicts an embedder must unpack.
        self.assertEqual([w.var for w in row.witness], ["account", "exposure"])
        self.assertEqual(row.witness[1].value, Decimal("105.50"))

    def test_a_gate_row_has_no_version_and_carries_its_witness_when_one_was_captured(self):
        row = envelopes.RejectionRow.from_json(golden("rejection_row_gate.json"))
        self.assertEqual(row.kind, "require")
        # None, not [] - absence means nothing was captured, and an empty
        # list would say the gate was judged under no bindings.
        self.assertIsNone(row.witness)
        self.assertIsNone(row.invariant_version)
        row = envelopes.RejectionRow.from_json(golden("rejection_row_gate_with_witness.json"))
        self.assertEqual(row.kind, "require")
        self.assertEqual([w.var for w in row.witness], ["doc", "limit"])
        self.assertEqual(row.witness[1].value, Decimal("5000"))

    def test_a_version_on_a_gate_row_is_drift(self):
        # A serializer regression that attached an invariant version to a
        # gate refusal used to parse happily; the row is impossible, so it
        # raises.
        payload = golden("rejection_row_gate.json")
        payload.update({"invariant_version": 4})
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.RejectionRow.from_json(payload)


class Migrations(unittest.TestCase):
    def test_a_database_behind_lists_what_is_outstanding(self):
        report = envelopes.MigrationReport.from_json(golden("migration_report_behind.json"))
        self.assertFalse(report.is_current)
        self.assertEqual(report.recorded_version_before, 23)
        self.assertEqual(report.binary_version, 25)
        self.assertEqual(
            [m.name for m in report.pending],
            ["a_migration_after_the_baseline", "another_after_it"],
        )
        self.assertEqual(report.applied, [])

    def test_a_migrated_database_reports_what_it_applied(self):
        report = envelopes.MigrationReport.from_json(golden("migration_report_applied.json"))
        self.assertTrue(report.is_current)
        self.assertEqual([m.version for m in report.applied], [24, 25])
        # The version AFTER, not the one it started at - a report saying
        # "current" and "version 23" at once would be two answers to one
        # question.
        self.assertEqual(report.recorded_version_before, 23)
        self.assertEqual(report.recorded_version_after, 25)

    def test_a_database_ahead_of_the_binary_is_not_current(self):
        # Nothing pending, and emphatically not ready: this build cannot know
        # whether a migration it has never seen still fits.
        report = envelopes.MigrationReport.from_json(golden("migration_report_ahead.json"))
        self.assertEqual(report.pending, [])
        self.assertFalse(report.is_current)
        self.assertEqual([m.version for m in report.unknown], [26])


class VersionSkew(unittest.TestCase):
    def test_the_hash_report_carries_the_binary_version(self):
        report = envelopes.HashReport.from_json(golden("hash_report.json"))
        self.assertEqual(report.morpholog_version, "0.0.0")
        self.assertIsNone(envelopes.version_skew(golden("hash_report.json"), "0.0.0"))

    def test_another_version_is_named_with_both_sides(self):
        skew = envelopes.version_skew(golden("hash_report.json"), "1.2.3")
        self.assertIn("0.0.0", skew)
        self.assertIn("1.2.3", skew)

    def test_only_the_exact_pre_versioned_report_shape_is_recognised_as_old(self):
        # The two-field report every binary before the versioned handshake
        # emitted. It states no version, so it is no version skew; it is the
        # one shape a client names as old rather than as drift.
        legacy = {"hash": "sha256:" + "0" * 64, "program": "envelopes"}
        self.assertIsNone(envelopes.version_skew(legacy, "1.2.3"))
        self.assertTrue(envelopes.predates_versioned_hash(legacy))
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.HashReport.from_json(legacy)
        for other in (
            {"hash": "x"},
            {"hash": "x", "program": "p", "novel": 1},
            golden("hash_report.json"),
            ["x"],
        ):
            self.assertFalse(envelopes.predates_versioned_hash(other), other)

    def test_an_absent_or_unreadable_version_is_no_evidence_of_a_version(self):
        self.assertIsNone(envelopes.version_skew(["x"], "1.2.3"))
        self.assertIsNone(envelopes.version_skew({}, "1.2.3"))
        self.assertIsNone(envelopes.version_skew({"morpholog_version": 7}, "1.2.3"))


class Provisioning(unittest.TestCase):
    def test_an_applied_run_and_its_dry_run_differ_only_in_what_they_did(self):
        applied = envelopes.ProvisionReport.from_json(golden("provision_report_applied.json"))
        planned = envelopes.ProvisionReport.from_json(golden("provision_report_dry_run.json"))
        self.assertTrue(applied.applied)
        self.assertFalse(applied.dry_run)
        self.assertTrue(planned.dry_run)
        self.assertFalse(planned.applied)
        self.assertEqual(applied.indexes, planned.indexes)
        self.assertEqual(
            [i.action for i in applied.indexes], ["create", "satisfied_externally"]
        )
        self.assertFalse(applied.has_conflict)
        self.assertEqual(applied.pruned, [])

    def test_current_means_nothing_is_outstanding(self):
        # The binary holds the same goldens to the same answers.
        self.assertTrue(
            envelopes.ProvisionReport.from_json(golden("provision_report_current.json")).is_current
        )
        for outstanding in ("applied", "dry_run", "conflict", "pruned"):
            report = envelopes.ProvisionReport.from_json(
                golden(f"provision_report_{outstanding}.json")
            )
            self.assertFalse(report.is_current, outstanding)
        # One outstanding action anywhere is enough.
        for field, action in (
            ("indexes", "create"),
            ("indexes", "repair_invalid"),
            ("indexes", "stale"),
            ("indexes", "conflict"),
            ("statistics", "create"),
            ("statistics", "stale"),
            ("statistics", "conflict"),
        ):
            payload = golden("provision_report_current.json")
            payload[field][0]["action"] = action
            self.assertFalse(
                envelopes.ProvisionReport.from_json(payload).is_current, (field, action)
            )

    def test_a_conflict_is_in_the_report(self):
        report = envelopes.ProvisionReport.from_json(golden("provision_report_conflict.json"))
        self.assertTrue(report.has_conflict)
        self.assertFalse(report.applied)
        self.assertFalse(report.dry_run)
        self.assertIn("arguments -> 7", report.indexes[1].detail)

    def test_a_conflict_in_the_statistics_alone_is_a_conflict(self):
        payload = golden("provision_report_conflict.json")
        payload["indexes"][1]["action"] = "keep"
        self.assertTrue(envelopes.ProvisionReport.from_json(payload).has_conflict)

    def test_a_pruning_run_names_what_it_dropped_and_what_others_protect(self):
        report = envelopes.ProvisionReport.from_json(golden("provision_report_pruned.json"))
        self.assertEqual([p.program for p in report.programs], ["billing", "ledger"])
        self.assertEqual(
            report.pruned,
            ["morpholog_ci_journalline_1_vk1_456789abcdef", "morpholog_cs_vk1_p1"],
        )
        self.assertEqual(report.positions_unknown_for, ["archive"])
        self.assertEqual(
            [(s.position, s.action, s.required_by) for s in report.statistics],
            [
                (0, "keep", ["billing", "ledger", "reporting"]),
                (1, "stale", []),
                (2, "keep", ["reporting"]),
                (3, "keep", []),
            ],
        )
        self.assertEqual(
            [(r.name, r.required_by) for r in report.required_elsewhere],
            [("morpholog_ci_period_2_vk1_cdef01234567", ["reporting"])],
        )
        self.assertEqual(report.indexes[1].required_by, ["billing", "ledger", "reporting"])

    def test_a_stale_index_is_dropped_only_by_a_run_that_applied_under_prune(self):
        for change in ({"applied": False, "dry_run": True}, {"prune": False}):
            payload = golden("provision_report_pruned.json")
            payload.update(change)
            self.assertEqual(envelopes.ProvisionReport.from_json(payload).pruned, [])

    def test_an_action_this_client_does_not_know_is_drift(self):
        for section in ("indexes", "statistics"):
            payload = golden("provision_report_pruned.json")
            payload[section][0]["action"] = "rebuild"
            with self.assertRaises(envelopes.EnvelopeError):
                envelopes.ProvisionReport.from_json(payload)


class Explanations(unittest.TestCase):
    def test_all_four_verdicts(self):
        admissible = envelopes.Explanation.from_json(golden("explanation_admissible.json"))
        self.assertTrue(admissible.admissible)
        gate = envelopes.Explanation.from_json(golden("explanation_gate.json"))
        self.assertIsInstance(gate.rejection, envelopes.GateRejection)
        self.assertEqual(
            gate.rejection.directly_missing_claims[0].candidate_supplier_transformations,
            ["flag_account"],
        )
        invariant = envelopes.Explanation.from_json(golden("explanation_invariant.json"))
        self.assertIsInstance(invariant.rejection, envelopes.InvariantRejection)
        error = envelopes.Explanation.from_json(golden("explanation_error.json"))
        self.assertIsInstance(error.rejection, envelopes.ErrorRejection)


class BatchReceipts(unittest.TestCase):
    def test_the_three_receipt_shapes(self):
        committed = envelopes.BatchReceipt.from_json(golden("batch_committed_receipt.json"))
        self.assertEqual(committed.row, 1)
        self.assertIsInstance(committed.outcome, envelopes.Committed)
        rejected = envelopes.BatchReceipt.from_json(golden("batch_rejected_receipt.json"))
        self.assertEqual(rejected.row, 2)
        self.assertIsInstance(rejected.outcome, envelopes.Rejected)
        not_committed = envelopes.BatchReceipt.from_json(
            golden("batch_error_receipt_not_committed.json")
        )
        self.assertIsInstance(not_committed.outcome, envelopes.BatchError)
        self.assertEqual(not_committed.outcome.code, "not_committed")
        error = envelopes.BatchReceipt.from_json(golden("batch_error_receipt.json"))
        self.assertEqual(error.row, 3)
        self.assertIsInstance(error.outcome, envelopes.BatchError)
        self.assertEqual(error.outcome.code, "invalid_request")


class Outbox(unittest.TestCase):
    def test_row_claim_and_updates(self):
        row = envelopes.OutboxRow.from_json(golden("outbox_row.json"))
        self.assertEqual(row.intent_type, "AccountOpened")
        self.assertEqual(row.arguments, ["acct_1"])
        self.assertEqual(
            row.enqueued_at, datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)
        )
        self.assertIsNone(row.delivered_at)
        claimed = envelopes.parse_outbox_claim(golden("outbox_claim.json"))
        self.assertEqual(claimed.locked_by, "worker-1")
        self.assertIsNone(envelopes.parse_outbox_claim(golden("outbox_claim_null.json")))
        applied = envelopes.OutboxUpdate.from_json(golden("outbox_update_applied.json"))
        self.assertTrue(applied.applied)
        lost = envelopes.OutboxUpdate.from_json(golden("outbox_update_lease_lost.json"))
        self.assertFalse(lost.applied)


class CheckRoutes(unittest.TestCase):
    def test_a_mixed_plan_names_each_invariant_in_order(self):
        report = envelopes.CheckReport.from_json(golden("check_report_routes.json"))
        self.assertEqual([i.name for i in report.invariants], ["cap", "fuel_is_known"])
        self.assertEqual(report.invariants[0].route, "compiled")
        self.assertIsNone(report.invariants[0].refusal)
        self.assertEqual(report.invariants[1].refusal.kind, "construct")
        self.assertEqual(report.route, "mixed")

    def test_no_invariants_is_an_empty_plan_and_compiled(self):
        report = envelopes.CheckReport.from_json(golden("check_report_no_invariants.json"))
        self.assertEqual(report.invariants, [])
        self.assertEqual(report.route, "compiled")

    def test_every_invariant_interpreted_is_interpreted(self):
        payload = golden("check_report_routes.json")
        payload["invariants"] = [payload["invariants"][1]]
        self.assertEqual(envelopes.CheckReport.from_json(payload).route, "interpreted")

    def test_a_kind_or_route_this_client_does_not_know_is_drift(self):
        for mutate in (
            lambda p: p["invariants"][1]["refusal"].__setitem__("kind", "novel"),
            lambda p: p["invariants"][0].__setitem__("route", "elsewhere"),
        ):
            payload = golden("check_report_routes.json")
            mutate(payload)
            with self.assertRaises(envelopes.EnvelopeError):
                envelopes.CheckReport.from_json(payload)

    def test_a_refusal_belongs_to_an_interpreted_invariant_and_only_to_one(self):
        compiled_with_refusal = golden("check_report_routes.json")
        compiled_with_refusal["invariants"][0]["refusal"] = {"kind": "construct", "message": "m"}
        interpreted_without = golden("check_report_routes.json")
        del interpreted_without["invariants"][1]["refusal"]
        interpreted_null = golden("check_report_routes.json")
        interpreted_null["invariants"][1]["refusal"] = None
        for payload in (compiled_with_refusal, interpreted_without, interpreted_null):
            with self.assertRaises(envelopes.EnvelopeError):
                envelopes.CheckReport.from_json(payload)

    def test_the_client_knows_exactly_the_published_kinds(self):
        schema = json.loads((TEMPLATES_DIR.parent / "src" / "schemas" / "result.json").read_text())
        published = set(schema["$defs"]["check_refusal"]["properties"]["kind"]["enum"])
        self.assertEqual(envelopes._REFUSAL_KINDS, published)

    def test_a_present_plan_is_a_list_never_null(self):
        payload = golden("check_report_no_invariants.json")
        payload["invariants"] = None
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.CheckReport.from_json(payload)


class Reports(unittest.TestCase):
    def test_check_hash_init_and_named_claim(self):
        check = envelopes.CheckReport.from_json(golden("check_report.json"))
        self.assertEqual(check.diagnostics[0].line, 19)
        self.assertIsNone(check.invariants, "a failed parse or validation has no plan")
        self.assertIsNone(check.route)
        hashed = envelopes.HashReport.from_json(golden("hash_report.json"))
        self.assertTrue(hashed.hash.startswith("sha256:"))
        init = envelopes.InitReport.from_json(golden("init_report.json"))
        self.assertEqual(init.schema, "morpholog")
        self.assertIsNone(init.least_privilege)
        locked = envelopes.InitReport.from_json(
            golden("init_report_least_privilege.json")
        )
        self.assertEqual(locked.least_privilege.writer_role, "morpholog_writer")
        self.assertTrue(locked.least_privilege.next_steps)
        named = envelopes.NamedClaim.from_json(golden("named_claim.json"))
        # Named-read values stay wire-true; the generated read models
        # parse them by declared kind.
        self.assertEqual(named.args["settled_qty"], "5000")
        self.assertIs(named.args["flagged"], False)


class AuditTail(unittest.TestCase):
    def test_the_audit_row_round_trips_the_golden(self):
        row = envelopes.AuditRow.from_json(golden("audit_row.json"))
        self.assertEqual(row.transformation_name, "open_account")
        self.assertEqual(row.actor, "alex")
        self.assertEqual(row.invariant_epoch, 1)
        self.assertEqual(len(row.invariants_checked), 1)
        check = row.invariants_checked[0]
        self.assertEqual(check.name, "account_unique_by_account_id")
        self.assertEqual(check.version, 1)
        # The kitchen-sink claim decodes through the same codecs the
        # run envelopes use - decimals exact, datetimes aware.
        claim = row.asserted_claims[0]
        self.assertEqual(claim.predicate, "EveryKind")
        self.assertEqual(claim.args[1], Decimal("100.50"))
        self.assertEqual(row.emitted_intents[0].name, "AccountOpened")
        self.assertEqual(row.committed_at.year, 2026)
        # A row from before attestation existed carries none.
        self.assertIsNone(row.attestation)

    def test_an_attested_audit_row_carries_its_lineage(self):
        row = envelopes.AuditRow.from_json(golden("audit_row_attested.json"))
        self.assertEqual(row.attestation.mode, "gateway")
        self.assertEqual(row.attestation.authenticated_by, "morpholog_writer")
        # Everything else is the same row.
        self.assertEqual(row.actor, "alex")

    def test_an_unknown_attestation_mode_raises(self):
        # The discriminator is part of the contract: a mode this client
        # does not know is drift, not data.
        payload = golden("audit_row_attested.json")
        payload["attestation"]["mode"] = "signature"
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.AuditRow.from_json(payload)

    def test_the_named_audit_row_round_trips_the_golden(self):
        row = envelopes.AuditRowNamed.from_json(golden("audit_row_named.json"))
        claim = row.asserted_claims[0]
        self.assertEqual(claim.predicate, "Account")
        self.assertEqual(claim.args["account_id"], "acct_1")
        self.assertEqual(row.retracted_claims, [])
        # The asymmetry the contract states: arguments stay
        # positional even in the named tail.
        self.assertEqual(row.arguments, ["acct_1"])

    def test_an_unknown_audit_key_raises(self):
        payload = golden("audit_row.json")
        payload["surprise"] = 1
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.AuditRow.from_json(payload)


class Coverage(unittest.TestCase):
    def test_the_coverage_report_round_trips_the_golden(self):
        report = envelopes.CoverageReport.from_json(golden("coverage_report.json"))
        self.assertEqual(report.transitions_replayed, 2)
        self.assertEqual(report.rejections_replayed, 3)
        fired = report.invariants[0]
        self.assertEqual(fired.verdict, "fired")
        self.assertEqual(fired.first_fired, "t1")
        self.assertEqual(fired.proposals_refused, 0)
        self.assertIsNone(fired.first_refused)
        # The wire field is `from` (the report's name); Python maps it
        # to `from_clause` because `from` is a keyword - the one
        # mapping wrinkle this golden exists to defend.
        self.assertEqual(
            fired.from_clause,
            "predicate CurrentRef, current pointer by (account_id)",
        )
        constrained = next(
            i for i in report.invariants if i.invariant == "no_flagged_accounts"
        )
        self.assertEqual(constrained.verdict, "constrained")
        self.assertEqual(constrained.proposals_refused, 1)
        self.assertEqual(constrained.first_refused, "r1")
        self.assertEqual(constrained.last_refused, "r1")
        self.assertIsNone(constrained.from_clause)
        self.assertFalse(constrained.not_in_programme)
        retired = next(
            i for i in report.invariants if i.invariant == "retired_rule"
        )
        self.assertTrue(retired.not_in_programme)
        unused = next(
            t for t in report.transformations if t.transformation == "open_account"
        )
        self.assertEqual(unused.transitions, 0)
        self.assertEqual(unused.proposals_refused, 1)
        self.assertFalse(unused.not_in_programme)
        drifted = next(
            t for t in report.transformations if t.transformation == "renamed_long_ago"
        )
        self.assertTrue(drifted.not_in_programme)


class RefreshDerived(unittest.TestCase):
    def test_the_report_round_trips_the_golden(self):
        report = envelopes.RefreshDerivedReport.from_json(
            golden("refresh_derived_report.json")
        )
        self.assertEqual(report.derived_claim_count, 4)
        self.assertEqual(report.derived_predicate_count, 1)
        self.assertEqual(report.source_claim_count, 12)
        self.assertTrue(report.model_hash.startswith("sha256:"))
        self.assertEqual(
            report.source_snapshot_transition_id,
            "01900000-0000-7000-8000-000000000001",
        )
        self.assertIsNotNone(report.source_snapshot_committed_at)
        self.assertIsNotNone(report.source_snapshot_committed_at.tzinfo)

    def test_no_transitions_omits_the_snapshot_pair_together(self):
        report = envelopes.RefreshDerivedReport.from_json(
            golden("refresh_derived_report_no_transitions.json")
        )
        self.assertIsNone(report.source_snapshot_transition_id)
        self.assertIsNone(report.source_snapshot_committed_at)

    def test_a_one_sided_snapshot_pair_raises(self):
        payload = golden("refresh_derived_report.json")
        del payload["source_snapshot_committed_at"]
        with self.assertRaises(envelopes.EnvelopeError) as caught:
            envelopes.RefreshDerivedReport.from_json(payload)
        self.assertIn("together", str(caught.exception))

    def test_an_unknown_report_key_raises(self):
        payload = golden("refresh_derived_report.json")
        payload["surprise"] = 1
        with self.assertRaises(envelopes.EnvelopeError) as caught:
            envelopes.RefreshDerivedReport.from_json(payload)
        self.assertIn("regenerate", str(caught.exception))


class DriftTripwire(unittest.TestCase):
    def test_an_unknown_envelope_key_raises(self):
        payload = golden("rejected.json")
        payload["surprise"] = 1
        with self.assertRaises(envelopes.EnvelopeError) as caught:
            envelopes.parse_run_outcome(payload)
        self.assertIn("regenerate", str(caught.exception))

    def test_a_missing_required_key_raises(self):
        payload = golden("committed.json")
        del payload["transition_id"]
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.parse_run_outcome(payload)


class TamperEvidence(unittest.TestCase):
    def test_verify_report_consistent_and_intact(self):
        report = envelopes.VerifyReport.from_json(golden("verify_report_consistent.json"))
        self.assertIsInstance(report.replay, envelopes.ReplayConsistent)
        self.assertEqual(report.replay.transitions, 2)
        self.assertIsInstance(report.tree, envelopes.TreeIntact)
        self.assertEqual(report.tree.checkpoints, 1)

    def test_verify_report_with_views_and_the_views_verdicts(self):
        report = envelopes.VerifyReport.from_json(golden("verify_report_with_views.json"))
        self.assertIsInstance(report.views, envelopes.ViewsIntact)
        self.assertEqual(report.views.views_checked, 4)
        # Without the opt-in leg, the field is simply absent.
        bare = envelopes.VerifyReport.from_json(golden("verify_report_consistent.json"))
        self.assertIsNone(bare.views)

        intact = envelopes.parse_views_verification(golden("views_verification_intact.json"))
        self.assertIsInstance(intact, envelopes.ViewsIntact)
        tampered = envelopes.parse_views_verification(golden("views_verification_tampered.json"))
        self.assertIsInstance(tampered, envelopes.ViewsTampered)
        self.assertEqual(tampered.mismatched, ["trade_captured"])
        self.assertEqual(tampered.missing, ["_morpholog_catalog"])
        unsealed = envelopes.parse_views_verification(golden("views_verification_not_sealed.json"))
        self.assertIsInstance(unsealed, envelopes.ViewsNotSealed)

    def test_a_receipt_and_each_layer_of_its_verification_parse_by_status(self):
        receipt = envelopes.EvaluationReceipt.from_json(golden("evaluation_receipt.json"))
        self.assertEqual(receipt.query, envelopes.ReceiptQuery("derived", "FacilityUtilisation"))
        self.assertEqual(receipt.answer[0].args[1], Decimal("0.4"))
        self.assertEqual(receipt.checkpoint.tree_size, 2)

        def report(name):
            return envelopes.ReceiptVerificationReport.from_json(
                golden(f"receipt_verification_report_{name}.json")
            )

        reproduced = report("reproduced")
        self.assertEqual(
            (reproduced.receipt, reproduced.completeness, reproduced.checkpoint,
             reproduced.program, reproduced.evaluation),
            ("well_formed", "complete", "matches", "matches", "reproduced"),
        )
        self.assertIsInstance(reproduced.evidence.verdict, envelopes.TreeIntact)
        self.assertEqual(report("differs").evaluation, envelopes.AnswerDiffers(1, 0))
        self.assertEqual(report("query_unknown").evaluation, envelopes.QueryUnknown("Wibble"))
        self.assertIsInstance(report("errored").evaluation, envelopes.EvaluationErrored)
        self.assertEqual(report("not_re_evaluated").evaluation, envelopes.NotReEvaluated(2, 1))
        mismatched = report("mismatched")
        self.assertEqual(mismatched.checkpoint.pack.tree_size, 3)
        self.assertIsInstance(mismatched.program, envelopes.ProgramDiffers)
        malformed = report("malformed")
        self.assertIsInstance(malformed.receipt, envelopes.ReceiptMalformed)
        self.assertIsInstance(malformed.evidence.verdict, envelopes.TreeMalformedPack)
        not_complete = report("not_complete")
        self.assertEqual((not_complete.verdict_kind, not_complete.completeness),
                         ("window", "not_complete"))
        self.assertIsInstance(not_complete.evidence.verdict, envelopes.WindowIntact)

        drifted = golden("receipt_verification_report_reproduced.json")
        drifted["evaluation"] = {"status": "certified"}
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.ReceiptVerificationReport.from_json(drifted)

    def test_the_witness_axis_on_the_live_and_pack_reports(self):
        report = envelopes.VerifyReport.from_json(golden("verify_report_witnessed.json"))
        axis = report.witnesses
        self.assertIsInstance(axis, envelopes.WitnessesReport)
        [checkpoint] = axis.checkpoints
        self.assertEqual(checkpoint.tree_size, 2)
        verified, untrusted = checkpoint.witnesses
        self.assertEqual(verified.status, "verified")
        self.assertEqual(verified.attested_at, axis.earliest_attested_at)
        self.assertIsNone(verified.detail)
        self.assertEqual(untrusted.status, "untrusted")
        self.assertIn("none of the supplied anchors", untrusted.detail)
        # Without any witness the field is simply absent.
        bare = envelopes.VerifyReport.from_json(golden("verify_report_consistent.json"))
        self.assertIsNone(bare.witnesses)

        pack = envelopes.PackVerificationReport.from_json(
            golden("pack_verification_report.json"), envelopes.parse_window_verification
        )
        self.assertIsInstance(pack.verdict, envelopes.WindowIntact)
        self.assertEqual(pack.witnesses, axis)

        for name, status in [
            ("witness_verdict_invalid.json", "invalid"),
            ("witness_verdict_unverified.json", "unverified"),
            ("witness_verdict_unsupported.json", "unsupported"),
        ]:
            verdict = envelopes.WitnessVerdict.from_json(golden(name))
            self.assertEqual(verdict.status, status)
            self.assertIsNotNone(verdict.detail)
        self.assertIsNone(
            envelopes.WitnessVerdict.from_json(golden("witness_verdict_invalid.json")).attested_at
        )

    def test_an_audit_row_names_its_own_parameters(self):
        stamped = envelopes.AuditRow.from_json(golden("audit_row_self_describing.json"))
        self.assertEqual(stamped.parameters, ["account_id"])
        self.assertEqual(len(stamped.parameters), len(stamped.arguments))
        # Rows from before names were stamped carry none.
        older = envelopes.AuditRow.from_json(golden("audit_row_attested.json"))
        self.assertIsNone(older.parameters)
        # The named tail carries every rung the bare row does.
        named = envelopes.AuditRowNamed.from_json(golden("audit_row_named.json"))
        self.assertEqual(named.parameters, ["account_id"])
        self.assertEqual(named.semantics_version, 1)
        self.assertEqual(len(named.drawn_subjects), 2)
        # Evidence is never coerced, and the row's shapes hold: names
        # are strings, only an attested row carries them, one per
        # argument.
        for tamper in (
            lambda r: r.__setitem__("parameters", [1]),
            lambda r: r.pop("attestation"),
            lambda r: r.__setitem__("parameters", ["account_id", "extra"]),
        ):
            row = golden("audit_row_self_describing.json")
            tamper(row)
            with self.assertRaises(envelopes.EnvelopeError):
                envelopes.AuditRow.from_json(row)

    def test_an_audit_row_names_the_programme_that_admitted_it(self):
        row = envelopes.AuditRow.from_json(golden("audit_row_model_hash.json"))
        self.assertEqual(row.model_hash, "sha256:" + "c" * 64)
        self.assertIsNone(
            envelopes.AuditRow.from_json(golden("audit_row_self_describing.json")).model_hash
        )
        # One shape only, and only on the top rung: never null, never
        # another spelling, never without names and an attestation.
        for tamper in (
            lambda r: r.__setitem__("model_hash", None),
            lambda r: r.__setitem__("model_hash", "sha256:" + "C" * 64),
            lambda r: r.__setitem__("model_hash", "sha256:abc"),
            lambda r: r.pop("parameters"),
            lambda r: r.pop("attestation"),
        ):
            row = golden("audit_row_model_hash.json")
            tamper(row)
            with self.assertRaises(envelopes.EnvelopeError):
                envelopes.AuditRow.from_json(row)

    def test_an_audit_row_names_the_semantics_that_decided_it(self):
        row = envelopes.AuditRow.from_json(golden("audit_row_semantics_version.json"))
        self.assertEqual(row.semantics_version, 1)
        self.assertIsNone(
            envelopes.AuditRow.from_json(golden("audit_row_model_hash.json")).semantics_version
        )
        # A version from a later Morpholog reads too: integrity is about the
        # bytes, not the evaluator they name.
        later = golden("audit_row_semantics_version.json")
        later["semantics_version"] = 0xFFFFFFFF
        self.assertEqual(envelopes.AuditRow.from_json(later).semantics_version, 0xFFFFFFFF)
        for tamper in (
            lambda r: r.__setitem__("semantics_version", 0),
            lambda r: r.__setitem__("semantics_version", 0x100000000),
            lambda r: r.__setitem__("semantics_version", "1"),
            lambda r: r.__setitem__("semantics_version", True),
            lambda r: r.pop("model_hash"),
        ):
            row = golden("audit_row_semantics_version.json")
            tamper(row)
            with self.assertRaises(envelopes.EnvelopeError):
                envelopes.AuditRow.from_json(row)

    def test_an_audit_row_records_the_subjects_its_act_drew(self):
        row = envelopes.AuditRow.from_json(golden("audit_row_drawn_subjects.json"))
        self.assertEqual(
            row.drawn_subjects,
            ["01900000-0000-7000-8000-00000000000a", "01900000-0000-7000-8000-00000000000b"],
        )
        self.assertIsNone(
            envelopes.AuditRow.from_json(golden("audit_row_semantics_version.json")).drawn_subjects
        )
        # Drew nothing is an empty list, not an absent one.
        none_drawn = golden("audit_row_drawn_subjects.json")
        none_drawn["drawn_subjects"] = []
        self.assertEqual(envelopes.AuditRow.from_json(none_drawn).drawn_subjects, [])
        for tamper in (
            lambda r: r.__setitem__("drawn_subjects", [1]),
            lambda r: r.__setitem__("drawn_subjects", "01900000"),
            lambda r: r.pop("semantics_version"),
        ):
            row = golden("audit_row_drawn_subjects.json")
            tamper(row)
            with self.assertRaises(envelopes.EnvelopeError):
                envelopes.AuditRow.from_json(row)

    def test_a_present_null_is_malformed_on_its_own_rung(self):
        # Absent is the only spelling for "not on this rung". Each case is
        # a row whose other fields are lawful, so only the null is at stake.
        for golden_name, field in (
            ("audit_row_self_describing.json", "parameters"),
            ("audit_row_attested.json", "attestation"),
            ("audit_row_model_hash.json", "model_hash"),
            ("audit_row_semantics_version.json", "semantics_version"),
            ("audit_row_drawn_subjects.json", "drawn_subjects"),
        ):
            row = golden(golden_name)
            envelopes.AuditRow.from_json(row)
            row[field] = None
            with self.assertRaises(envelopes.EnvelopeError, msg=field):
                envelopes.AuditRow.from_json(row)

    def test_transact_outcomes(self):
        committed = envelopes.parse_atomic_outcome(golden("transact_committed.json"))
        self.assertIsInstance(committed, envelopes.AtomicCommitted)
        self.assertEqual([a.row for a in committed.acts], [1, 2])
        self.assertIsInstance(committed.acts[0].outcome, envelopes.Committed)
        self.assertEqual(committed.acts[1].outcome.emitted_intents[0].name, "AccountOpened")
        self.assertNotEqual(
            committed.acts[0].outcome.transition_id, committed.acts[1].outcome.transition_id
        )
        rejected = envelopes.parse_atomic_outcome(golden("transact_rejected.json"))
        self.assertIsInstance(rejected, envelopes.AtomicRejected)
        self.assertEqual(rejected.act, 2)
        self.assertEqual(rejected.rule, "balance_unique_by_account")
        self.assertTrue(rejected.witness)
        in_session = envelopes.parse_atomic_outcome(golden("transact_rejected_session.json"))
        self.assertEqual((in_session.act, in_session.rule, in_session.witness), (1, None, []))
        # An act with a key the contract does not name is drift, even
        # one that would be overwritten.
        stray = golden("transact_committed.json")
        stray["acts"][0]["status"] = "committed"
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.parse_atomic_outcome(stray)
        # A coded error is not an outcome: the parser refuses it, the
        # adapter raises it.
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.parse_atomic_outcome(golden("transact_error.json"))

    def test_verify_report_divergent_and_tampered(self):
        report = envelopes.VerifyReport.from_json(golden("verify_report_divergent.json"))
        self.assertIsInstance(report.replay, envelopes.ReplayDivergent)
        self.assertEqual(report.replay.only_in_claims_table[0].predicate, "EveryKind")
        self.assertEqual(report.replay.only_in_replay, [])
        self.assertIsInstance(report.tree, envelopes.TreeTampered)
        self.assertNotEqual(report.tree.recorded_root, report.tree.recomputed_root)

    def test_checkpoint_outcomes(self):
        created = envelopes.parse_checkpoint_outcome(golden("checkpoint_created.json"))
        self.assertIsInstance(created, envelopes.CheckpointCreated)
        self.assertEqual(created.checkpoint.tree_size, 2)
        self.assertIsNone(created.checkpoint.prev_checkpoint_hash)
        no_new = envelopes.parse_checkpoint_outcome(golden("checkpoint_no_new_rows.json"))
        self.assertIsInstance(no_new, envelopes.CheckpointNoNewRows)
        self.assertEqual(created.checkpoint.signatures, [])

    def test_a_signed_checkpoint_carries_its_signature(self):
        signed = envelopes.parse_checkpoint_outcome(golden("checkpoint_created_signed.json"))
        self.assertIsInstance(signed, envelopes.CheckpointCreated)
        sig = signed.checkpoint.signatures[0]
        self.assertIsInstance(sig, envelopes.TreeHeadSignature)
        self.assertEqual(sig.key_id, "audit-2026-q3")
        self.assertEqual(sig.purpose, "audit_checkpoint_v1")
        self.assertTrue(sig.public_key.startswith("ed25519-pub:"))
        self.assertTrue(sig.signature.startswith("ed25519-sig:"))
        witnessed = envelopes.parse_checkpoint_outcome(golden("checkpoint_created_witnessed.json"))
        [witness] = witnessed.checkpoint.witnesses
        self.assertEqual(witness.scheme, "rfc3161")
        self.assertEqual(witness.submitted_to, "http://timestamp.example/tsr")
        self.assertTrue(witness.proof.startswith("MIIB"))
        self.assertEqual(signed.checkpoint.witnesses, [])
        bare = envelopes.Checkpoint.from_json(golden("checkpoint_witnessed.json"))
        self.assertEqual(bare.witnesses, witnessed.checkpoint.witnesses)

    def test_every_tree_verdict_parses(self):
        for name, cls in [
            ("tree_verification_chain_broken.json", envelopes.TreeChainBroken),
            ("tree_verification_anchor_mismatch.json", envelopes.TreeAnchorMismatch),
            ("tree_verification_malformed_pack.json", envelopes.TreeMalformedPack),
            ("tree_verification_unsupported_pack.json", envelopes.TreeUnsupportedPack),
            ("tree_verification_signature_invalid.json", envelopes.TreeSignatureInvalid),
            ("tree_verification_unauthorized_key.json", envelopes.TreeUnauthorizedKey),
            ("tree_verification_signature_required.json", envelopes.TreeSignatureRequired),
            ("tree_verification_signing_key_required.json", envelopes.TreeSigningKeyRequired),
        ]:
            self.assertIsInstance(envelopes.parse_tree_verification(golden(name)), cls)

    def test_prefix_pack_manifest(self):
        manifest = envelopes.PrefixPackManifest.from_json(golden("prefix_pack_manifest.json"))
        self.assertEqual(manifest.pack_format_version, 17)
        self.assertEqual(manifest.pack_kind, "prefix")
        self.assertEqual(manifest.morpholog_version, "0.0.0")
        self.assertEqual(manifest.checkpoint_count, 1)

    def test_every_window_verdict_parses(self):
        for name, cls in [
            ("window_verification_intact.json", envelopes.WindowIntact),
            (
                "window_verification_inconsistent_extension.json",
                envelopes.WindowInconsistentExtension,
            ),
            ("window_verification_row_not_included.json", envelopes.WindowRowNotIncluded),
            ("window_verification_anchor_mismatch.json", envelopes.WindowAnchorMismatch),
            ("window_verification_signature_invalid.json", envelopes.WindowSignatureInvalid),
            ("window_verification_signature_required.json", envelopes.WindowSignatureRequired),
            ("window_verification_malformed.json", envelopes.WindowMalformed),
        ]:
            self.assertIsInstance(envelopes.parse_window_verification(golden(name)), cls)

    def test_window_evidence_pack_with_proofs(self):
        pack = envelopes.WindowEvidencePack.from_json(golden("window_evidence_pack.json"))
        self.assertEqual(pack.manifest.pack_format_version, 17)
        self.assertEqual(pack.manifest.pack_kind, "window")
        self.assertIsInstance(pack.from_checkpoint, envelopes.Checkpoint)
        self.assertIsInstance(pack.to_checkpoint, envelopes.Checkpoint)
        self.assertEqual(len(pack.rows), len(pack.inclusion_proofs))
        self.assertIsInstance(pack.inclusion_proofs[0], envelopes.RowInclusionProof)
        self.assertEqual(pack.inclusion_proofs[0].leaf_index, 2)

    def test_selective_evidence_pack_and_every_verdict(self):
        pack = envelopes.SelectiveEvidencePack.from_json(golden("selective_evidence_pack.json"))
        self.assertEqual(pack.manifest.pack_format_version, 17)
        self.assertEqual(pack.manifest.pack_kind, "selective")
        self.assertIsInstance(pack.checkpoint, envelopes.Checkpoint)
        self.assertEqual(len(pack.rows), len(pack.inclusion_proofs))
        self.assertEqual(pack.inclusion_proofs[0].leaf_index, 1)

        cases = [
            ("selective_verification_intact.json", envelopes.SelectiveIntact),
            ("selective_verification_row_not_included.json", envelopes.SelectiveRowNotIncluded),
            ("selective_verification_anchor_mismatch.json", envelopes.SelectiveAnchorMismatch),
            ("selective_verification_signature_invalid.json", envelopes.SelectiveSignatureInvalid),
            (
                "selective_verification_signature_required.json",
                envelopes.SelectiveSignatureRequired,
            ),
            ("selective_verification_malformed.json", envelopes.SelectiveMalformed),
        ]
        for name, expected in cases:
            verdict = envelopes.parse_selective_verification(golden(name))
            self.assertIsInstance(verdict, expected, name)
        intact = envelopes.parse_selective_verification(
            golden("selective_verification_intact.json")
        )
        self.assertEqual(intact.rows_disclosed, 1)


class SessionEnvelopes(unittest.TestCase):
    def test_the_ready_line_carries_the_staleness_token_and_protocol(self):
        ready = envelopes.SessionReady.from_json(golden("session_ready.json"))
        self.assertEqual(ready.model_hash, "sha256:" + "0" * 64)
        self.assertEqual(ready.program, "envelopes")
        self.assertEqual(ready.protocol, 1)

    def test_the_error_receipt_carries_the_stable_code(self):
        unknown = envelopes.SessionErrorReceipt.from_json(
            golden("session_error_receipt_commit_outcome_unknown.json")
        )
        self.assertEqual(unknown.code, "commit_outcome_unknown")
        self.assertEqual(unknown.row, 18)
        receipt = envelopes.SessionErrorReceipt.from_json(golden("session_error_receipt.json"))
        self.assertEqual(receipt.code, "serialization_failure")
        self.assertEqual(receipt.row, 17)
        self.assertIn("could not be decided", receipt.error)

    def test_an_uncoded_error_is_drift_not_a_receipt(self):
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.SessionErrorReceipt.from_json(
                {"error": "prose only", "row": 1, "status": "error"}
            )


if __name__ == "__main__":
    unittest.main()
