"""The envelope models against the SAME golden files the Rust contract
test pins byte-equal to the binary's real serialization - one sample
set holding the binary, result.json, and this client together."""

import json
import sys
import unittest
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path

from _support import add_client_to_path, golden

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

    def test_every_tree_verdict_parses(self):
        for name, cls in [
            ("tree_verification_chain_broken.json", envelopes.TreeChainBroken),
            ("tree_verification_anchor_mismatch.json", envelopes.TreeAnchorMismatch),
            ("tree_verification_malformed_pack.json", envelopes.TreeMalformedPack),
            ("tree_verification_signature_invalid.json", envelopes.TreeSignatureInvalid),
            ("tree_verification_unauthorized_key.json", envelopes.TreeUnauthorizedKey),
            ("tree_verification_signature_required.json", envelopes.TreeSignatureRequired),
        ]:
            self.assertIsInstance(envelopes.parse_tree_verification(golden(name)), cls)

    def test_evidence_pack_with_embedded_rows_and_checkpoints(self):
        pack = envelopes.EvidencePack.from_json(golden("evidence_pack.json"))
        self.assertEqual(pack.manifest.pack_format_version, 1)
        self.assertIsInstance(pack.checkpoints[0], envelopes.Checkpoint)
        self.assertIsInstance(pack.rows[0], envelopes.AuditRow)
        self.assertEqual(pack.rows[0].transformation_name, "open_account")

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
        self.assertEqual(pack.manifest.pack_format_version, 2)
        self.assertEqual(pack.manifest.pack_kind, "window")
        self.assertIsInstance(pack.from_checkpoint, envelopes.Checkpoint)
        self.assertIsInstance(pack.to_checkpoint, envelopes.Checkpoint)
        self.assertEqual(len(pack.rows), len(pack.inclusion_proofs))
        self.assertIsInstance(pack.inclusion_proofs[0], envelopes.RowInclusionProof)
        self.assertEqual(pack.inclusion_proofs[0].leaf_index, 2)

    def test_selective_evidence_pack_and_every_verdict(self):
        pack = envelopes.SelectiveEvidencePack.from_json(golden("selective_evidence_pack.json"))
        self.assertEqual(pack.manifest.pack_format_version, 3)
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


if __name__ == "__main__":
    unittest.main()
