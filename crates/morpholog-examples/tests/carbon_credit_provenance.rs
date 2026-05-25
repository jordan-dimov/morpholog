//! Integration tests for the carbon-credit provenance example
//! (`examples/09_carbon_credit_provenance/`).
//!
//! The point of this example is that a green claim cannot become official
//! unless its provenance chain is admissible - and when it fails, the
//! explanation engine names the missing link and the transformation that
//! could supply it. These tests pin that payoff against the real program,
//! plus the double-counting invariant and the terminal-retirement
//! present-blockers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, dec, has_claim, must_accept, subj};
use morpholog_core::{EvalValue, GateKind, Rejection, State, Transition, Verdict, explain};
use morpholog_examples::carbon_credit_provenance as cc;

fn transition(name: &str, args: Vec<EvalValue>, actor: &str) -> Transition {
    Transition {
        transformation_name: name.to_string(),
        args,
        actor: subj(actor),
    }
}

fn issue(credit: &str, measurement: &str, verifier: &str, account: &str) -> Transition {
    transition(
        "issue_credit",
        vec![
            subj(credit),
            subj(measurement),
            subj(verifier),
            subj(account),
        ],
        "registry",
    )
}

// ============================================================
// The legitimacy chain: a green claim cannot issue without admissible
// provenance, and explain names the missing link + its supplier.
// ============================================================

#[test]
fn issue_with_full_provenance_is_admissible() {
    let state = State::from_claims(vec![
        claim_instance("Accredited", &[subj("acme_verifier")]),
        claim_instance("VerifiedMeasurement", &[subj("m1"), dec(100)]),
        claim_instance("Attestation", &[subj("m1"), subj("acme_verifier")]),
    ]);

    let explanation = explain(
        &cc::program(),
        &issue("c1", "m1", "acme_verifier", "acct1"),
        &state,
    );
    assert_eq!(explanation.verdict, Verdict::Admissible);
}

#[test]
fn issue_without_verified_measurement_names_the_missing_measurement() {
    // Accredited verifier exists, but the measurement was never verified.
    // The `bind VerifiedMeasurement(...)` gate is the first to fail.
    let state = State::from_claims(vec![claim_instance("Accredited", &[subj("acme_verifier")])]);

    let explanation = explain(
        &cc::program(),
        &issue("c1", "m1", "acme_verifier", "acct1"),
        &state,
    );

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert_eq!(gate.statement_kind, GateKind::BindOne);
    assert_eq!(gate.directly_missing_claims.len(), 1);
    let missing = &gate.directly_missing_claims[0];
    assert_eq!(missing.predicate, "VerifiedMeasurement");
    // `quantity` is unbound at the failing bind, so it renders symbolically.
    assert_eq!(missing.rendered, "VerifiedMeasurement(m1, quantity)");
    assert_eq!(
        missing.candidate_supplier_transformations,
        vec!["verify_measurement"],
    );
}

#[test]
fn issue_without_attestation_names_the_missing_attestation() {
    let state = State::from_claims(vec![
        claim_instance("Accredited", &[subj("acme_verifier")]),
        claim_instance("VerifiedMeasurement", &[subj("m1"), dec(100)]),
    ]);

    let explanation = explain(
        &cc::program(),
        &issue("c1", "m1", "acme_verifier", "acct1"),
        &state,
    );

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert_eq!(gate.statement_kind, GateKind::Require);
    assert_eq!(gate.directly_missing_claims.len(), 1);
    let missing = &gate.directly_missing_claims[0];
    assert_eq!(missing.predicate, "Attestation");
    assert_eq!(missing.rendered, "Attestation(m1, acme_verifier)");
    assert_eq!(
        missing.candidate_supplier_transformations,
        vec!["attest_measurement"],
    );
}

#[test]
fn issue_with_unaccredited_verifier_names_the_missing_accreditation() {
    // Verified and attested, but the verifier's accreditation is absent
    // (e.g. revoked after attestation) - the currentness check bites.
    let state = State::from_claims(vec![
        claim_instance("VerifiedMeasurement", &[subj("m1"), dec(100)]),
        claim_instance("Attestation", &[subj("m1"), subj("acme_verifier")]),
    ]);

    let explanation = explain(
        &cc::program(),
        &issue("c1", "m1", "acme_verifier", "acct1"),
        &state,
    );

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert_eq!(gate.directly_missing_claims.len(), 1);
    let missing = &gate.directly_missing_claims[0];
    assert_eq!(missing.predicate, "Accredited");
    assert_eq!(missing.rendered, "Accredited(acme_verifier)");
    assert_eq!(
        missing.candidate_supplier_transformations,
        vec!["grant_accreditation"],
    );
}

// ============================================================
// No double counting: a second credit for the same measurement is an
// invariant rejection, not a missing claim.
// ============================================================

#[test]
fn second_credit_for_the_same_measurement_violates_no_double_issuance() {
    let state = State::from_claims(vec![
        claim_instance("Accredited", &[subj("acme_verifier")]),
        claim_instance("VerifiedMeasurement", &[subj("m1"), dec(100)]),
        claim_instance("Attestation", &[subj("m1"), subj("acme_verifier")]),
        claim_instance("Issued", &[subj("c1"), subj("m1"), dec(100)]),
        claim_instance("HeldBy", &[subj("c1"), subj("acct1")]),
    ]);

    let explanation = explain(
        &cc::program(),
        &issue("c2", "m1", "acme_verifier", "acct2"),
        &state,
    );

    let Verdict::Rejected(Rejection::Invariant(inv)) = &explanation.verdict else {
        panic!(
            "expected an invariant rejection, got {:?}",
            explanation.verdict
        );
    };
    assert_eq!(inv.name, "no_double_issuance");
}

#[test]
fn reissuing_one_credit_against_a_second_measurement_is_an_invariant_rejection() {
    // c1 already backs m1; backing m2 with the same credit (to the same
    // account, so custody stays single-valued) violates the converse rule
    // that a credit is backed by exactly one measurement.
    let state = State::from_claims(vec![
        claim_instance("Accredited", &[subj("acme_verifier")]),
        claim_instance("VerifiedMeasurement", &[subj("m2"), dec(50)]),
        claim_instance("Attestation", &[subj("m2"), subj("acme_verifier")]),
        claim_instance("Issued", &[subj("c1"), subj("m1"), dec(100)]),
        claim_instance("HeldBy", &[subj("c1"), subj("acct1")]),
    ]);

    let explanation = explain(
        &cc::program(),
        &issue("c1", "m2", "acme_verifier", "acct1"),
        &state,
    );

    let Verdict::Rejected(Rejection::Invariant(inv)) = &explanation.verdict else {
        panic!(
            "expected an invariant rejection, got {:?}",
            explanation.verdict
        );
    };
    assert_eq!(inv.name, "credit_backed_by_one_measurement");
}

// ============================================================
// Retirement is terminal: transfer-after-retire and double-retire are
// present-blocker gate rejections with no directly-missing claim.
// ============================================================

#[test]
fn transfer_after_retirement_is_a_present_blocker() {
    let state = State::from_claims(vec![claim_instance(
        "Retired",
        &[subj("c1"), subj("acct1")],
    )]);
    let t = transition(
        "transfer_credit",
        vec![subj("c1"), subj("acct1"), subj("acct2")],
        "acct1",
    );

    let explanation = explain(&cc::program(), &t, &state);

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert!(
        gate.directly_missing_claims.is_empty(),
        "a retired-credit block is not a missing claim: {:?}",
        gate.directly_missing_claims,
    );
}

#[test]
fn double_retirement_is_a_present_blocker() {
    let state = State::from_claims(vec![claim_instance(
        "Retired",
        &[subj("c1"), subj("acct1")],
    )]);
    let t = transition("retire_credit", vec![subj("c1"), subj("acct1")], "acct1");

    let explanation = explain(&cc::program(), &t, &state);

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert!(gate.directly_missing_claims.is_empty());
}

// ============================================================
// Currentness: revoking accreditation blocks new issuance but leaves
// credits already issued untouched. End-to-end through the real
// transformations.
// ============================================================

#[test]
fn revoking_accreditation_blocks_new_issuance_but_preserves_history() {
    let inv = cc::all_invariants();
    let s = State::default();
    let s = must_accept(
        &cc::grant_accreditation(),
        vec![subj("acme_verifier")],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::verify_measurement(),
        vec![subj("m1"), dec(100)],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::attest_measurement(),
        vec![subj("m1"), subj("acme_verifier")],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::issue_credit(),
        vec![subj("c1"), subj("m1"), subj("acme_verifier"), subj("acct1")],
        s,
        &inv,
    );
    // A second measurement, attested while the verifier is still accredited.
    let s = must_accept(
        &cc::verify_measurement(),
        vec![subj("m2"), dec(50)],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::attest_measurement(),
        vec![subj("m2"), subj("acme_verifier")],
        s,
        &inv,
    );

    // Revoke the accreditation.
    let s = must_accept(
        &cc::revoke_accreditation(),
        vec![subj("acme_verifier")],
        s,
        &inv,
    );

    // History survives: the already-issued credit keeps its standing.
    assert!(
        has_claim(&s, "Issued", &[subj("c1"), subj("m1"), dec(100)]),
        "the credit issued before revocation must keep its standing",
    );

    // New issuance through the now-unaccredited verifier is rejected, and
    // explain names the missing current accreditation.
    let explanation = explain(
        &cc::program(),
        &issue("c2", "m2", "acme_verifier", "acct1"),
        &s,
    );
    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert_eq!(gate.directly_missing_claims.len(), 1);
    assert_eq!(gate.directly_missing_claims[0].predicate, "Accredited");
}

// ============================================================
// The lifecycle commits end-to-end (propose, not just explain).
// ============================================================

#[test]
fn issue_transfer_retire_commit_in_sequence() {
    let inv = cc::all_invariants();
    let s = State::default();
    let s = must_accept(
        &cc::grant_accreditation(),
        vec![subj("acme_verifier")],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::verify_measurement(),
        vec![subj("m1"), dec(100)],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::attest_measurement(),
        vec![subj("m1"), subj("acme_verifier")],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::issue_credit(),
        vec![subj("c1"), subj("m1"), subj("acme_verifier"), subj("acct1")],
        s,
        &inv,
    );
    let s = must_accept(
        &cc::transfer_credit(),
        vec![subj("c1"), subj("acct1"), subj("acct2")],
        s,
        &inv,
    );
    assert!(has_claim(&s, "HeldBy", &[subj("c1"), subj("acct2")]));
    assert!(!has_claim(&s, "HeldBy", &[subj("c1"), subj("acct1")]));

    let s = must_accept(
        &cc::retire_credit(),
        vec![subj("c1"), subj("acct2")],
        s,
        &inv,
    );
    assert!(has_claim(&s, "Retired", &[subj("c1"), subj("acct2")]));
    // Terminal: no custody remains after retirement.
    assert!(!has_claim(&s, "HeldBy", &[subj("c1"), subj("acct2")]));
}
