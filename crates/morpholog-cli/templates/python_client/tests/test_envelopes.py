"""The envelope models against the SAME golden files the Rust contract
test pins byte-equal to the binary's real serialization - one sample
set holding the binary, result.json, and this client together."""

import json
import sys
import unittest
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from python_client import envelopes

GOLDEN_DIR = Path(__file__).resolve().parents[3] / "tests" / "golden" / "envelopes"


def golden(name):
    return json.loads((GOLDEN_DIR / name).read_text())


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

    def test_rejected_with_and_without_explanation(self):
        bare = envelopes.parse_run_outcome(golden("rejected.json"))
        self.assertIsInstance(bare, envelopes.Rejected)
        self.assertIsNone(bare.explanation)
        explained = envelopes.parse_run_outcome(golden("rejected_with_explanation.json"))
        self.assertIsInstance(
            explained.explanation.rejection, envelopes.InvariantRejection
        )

    def test_traced_envelopes(self):
        committed = envelopes.TracedEnvelope.from_json(golden("traced_committed.json"))
        self.assertIsInstance(committed.result, envelopes.Committed)
        errored = envelopes.TracedEnvelope.from_json(golden("traced_errored.json"))
        self.assertIsInstance(errored.result, envelopes.Errored)


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
        error = envelopes.BatchReceipt.from_json(golden("batch_error_receipt.json"))
        self.assertEqual(error.row, 3)
        self.assertIsInstance(error.outcome, envelopes.BatchError)


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


class Reports(unittest.TestCase):
    def test_check_hash_init_and_named_claim(self):
        check = envelopes.CheckReport.from_json(golden("check_report.json"))
        self.assertEqual(check.diagnostics[0].line, 19)
        hashed = envelopes.HashReport.from_json(golden("hash_report.json"))
        self.assertTrue(hashed.hash.startswith("sha256:"))
        init = envelopes.InitReport.from_json(golden("init_report.json"))
        self.assertEqual(init.schema, "morpholog")
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


if __name__ == "__main__":
    unittest.main()
