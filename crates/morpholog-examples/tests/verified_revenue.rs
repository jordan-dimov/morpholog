//! Integration tests for the verified-revenue example
//! (`examples/02_verified_revenue/`).
//!
//! Two complementary patterns woven through one programme:
//!
//! - **Currentness with restatement.** Verifier admits a figure;
//!   later corrects it. Singleton `CurrentVerification` pointer
//!   moves; lineage recorded as `Supersedes`; standing on the prior
//!   verification is retracted by pattern; historical decisions
//!   survive in admitted state.
//!
//! - **Admissibility-for-purpose.** Two authorities grant standing
//!   for different decisions on the same underlying verification.
//!   Standing can be revoked; the underlying verification is never
//!   mutated; the historical record of each decision survives any
//!   later revocation or correction.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec, has_claim, must_accept, subj};
use morpholog_core::{Invariant, Outcome, State};
use morpholog_examples::verified_revenue;

fn invariants() -> Vec<Invariant> {
    verified_revenue::all_invariants()
}

fn asset() -> morpholog_core::EvalValue {
    subj("asset_a")
}

fn period() -> morpholog_core::EvalValue {
    subj("p_2026_04")
}

fn admit_iv(state: State, amount: i64, ver: &str) -> State {
    must_accept(
        &verified_revenue::admit_independent_verification(),
        vec![asset(), period(), dec(amount), subj(ver)],
        state,
        &invariants(),
    )
}

fn grant(state: State, ver: &str, purpose: &str, authority: &str, grant_id: &str) -> State {
    must_accept(
        &verified_revenue::grant_standing(),
        vec![subj(ver), subj(purpose), subj(authority), subj(grant_id)],
        state,
        &invariants(),
    )
}

// ============================================================
// Restatement pattern
// ============================================================

#[test]
fn admit_then_correct_preserves_history_and_moves_pointer() {
    // Verifier admits 91; then corrects to 88. The original
    // IndependentlyVerifiedRevenue stays admitted; the singleton
    // CurrentVerification pointer moves to the corrected figure;
    // Supersedes records the lineage.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let post = must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
        &invariants(),
    );

    // Both verifications in admitted state.
    assert!(has_claim(
        &post,
        "IndependentlyVerifiedRevenue",
        &[asset(), period(), dec(91), subj("ver_001")],
    ));
    assert!(has_claim(
        &post,
        "IndependentlyVerifiedRevenue",
        &[asset(), period(), dec(88), subj("ver_002")],
    ));
    // Lineage recorded.
    assert!(has_claim(
        &post,
        "Supersedes",
        &[subj("ver_002"), subj("ver_001")],
    ));
    // Pointer moved.
    assert!(has_claim(
        &post,
        "CurrentVerification",
        &[asset(), period(), subj("ver_002")],
    ));
    assert!(!has_claim(
        &post,
        "CurrentVerification",
        &[asset(), period(), subj("ver_001")],
    ));
}

#[test]
fn cannot_correct_already_superseded_verification() {
    // The at_most_one_direct_successor invariant + the require in
    // correct_independent_verification together prevent parallel
    // restatement chains.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
        &invariants(),
    );
    let outcome = common::propose_with_test_actor(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(85), subj("ver_003"), subj("ver_001")],
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn second_admission_against_existing_current_is_rejected() {
    // The admit_independent_verification require enforces that an
    // (asset, period) has at most one current verification at any
    // moment. To replace it, use correct_independent_verification.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let outcome = common::propose_with_test_actor(
        &verified_revenue::admit_independent_verification(),
        vec![asset(), period(), dec(95), subj("ver_002")],
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Standing pattern
// ============================================================

#[test]
fn parallel_standings_coexist_on_same_verification() {
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::BANK_DEBT_SERVICE,
        "credit_committee",
        "grant_bank_001",
    );
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::INVESTOR_REPORTING,
        "investor_relations",
        "grant_inv_001",
    );

    assert!(has_claim(
        &pre,
        "AdmissibleFor",
        &[subj("ver_001"), subj(verified_revenue::BANK_DEBT_SERVICE)],
    ));
    assert!(has_claim(
        &pre,
        "AdmissibleFor",
        &[subj("ver_001"), subj(verified_revenue::INVESTOR_REPORTING)],
    ));
}

#[test]
fn decision_admits_only_with_matching_standing() {
    let pre = admit_iv(State::default(), 91, "ver_001");

    // No standing for bank yet; admit_debt_service_revenue is
    // rejected. Asserts via the trace that the first require
    // (IndependentlyVerifiedRevenue exists) Held, while the second
    // require (AdmissibleFor on bank_debt_service) Rejected. The
    // standing-gate distinction is the load-bearing semantics of
    // this example; trace lets us pin it precisely instead of
    // settling for `matches!(outcome, Rejected { .. })`.
    use morpholog_core::{
        RequireOutcome, Subject, TraceEntry, TracedProposal, Transition, propose_with_trace,
    };
    let t = verified_revenue::admit_debt_service_revenue();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        actor: Subject::from("test_actor"),
    };
    let TracedProposal::Completed { outcome, trace } =
        propose_with_trace(&t, &transition, &pre, &invariants())
    else {
        panic!("expected Completed");
    };
    assert!(matches!(outcome, Outcome::Rejected { .. }));
    let require_outcomes: Vec<(&str, &RequireOutcome)> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Require {
                expression,
                outcome,
            } => Some((expression.as_str(), outcome)),
            _ => None,
        })
        .collect();
    let held_verification = require_outcomes.iter().any(|(expr, out)| {
        expr.contains("IndependentlyVerifiedRevenue") && matches!(out, RequireOutcome::Held { .. })
    });
    let rejected_standing = require_outcomes.iter().any(|(expr, out)| {
        expr.contains("AdmissibleFor") && matches!(out, RequireOutcome::Rejected { .. })
    });
    assert!(
        held_verification,
        "expected the IndependentlyVerifiedRevenue gate to hold; trace: {trace:#?}"
    );
    assert!(
        rejected_standing,
        "expected the AdmissibleFor gate to reject; trace: {trace:#?}"
    );

    // Bank grants standing; admit succeeds.
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::BANK_DEBT_SERVICE,
        "credit_committee",
        "grant_001",
    );
    let post = must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        pre,
        &invariants(),
    );
    assert!(has_claim(
        &post,
        "DebtServiceRevenue",
        &[
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
    ));
}

#[test]
fn investor_standing_does_not_admit_bank_decision() {
    // Bank decision requires bank_debt_service standing specifically;
    // investor_reporting standing is not enough.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::INVESTOR_REPORTING,
        "investor_relations",
        "grant_inv_001",
    );
    let outcome = common::propose_with_test_actor(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn revoking_standing_blocks_future_but_preserves_past() {
    // The require-vs-invariant payoff. A decision admitted under
    // valid standing survives a later revocation.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::BANK_DEBT_SERVICE,
        "credit_committee",
        "grant_001",
    );
    let pre = must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        pre,
        &invariants(),
    );
    let after_revoke = must_accept(
        &verified_revenue::revoke_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("revoke_001"),
        ],
        pre,
        &invariants(),
    );

    // Historical decision survives.
    assert!(has_claim(
        &after_revoke,
        "DebtServiceRevenue",
        &[
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
    ));
    // AdmissibleFor is gone.
    assert!(!has_claim(
        &after_revoke,
        "AdmissibleFor",
        &[subj("ver_001"), subj(verified_revenue::BANK_DEBT_SERVICE)],
    ));
    // StandingRevoked recorded.
    assert!(has_claim(
        &after_revoke,
        "StandingRevoked",
        &[
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("revoke_001"),
        ],
    ));
    // A new decision against the same verification is rejected.
    let outcome = common::propose_with_test_actor(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_002"),
            subj("ver_001"),
        ],
        &after_revoke,
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn cannot_regrant_after_revocation() {
    // Revocation is terminal in v0. After StandingRevoked is admitted
    // for a (verification, purpose) pair, grant_standing is rejected.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::BANK_DEBT_SERVICE,
        "credit_committee",
        "grant_001",
    );
    let after_revoke = must_accept(
        &verified_revenue::revoke_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("revoke_001"),
        ],
        pre,
        &invariants(),
    );
    let outcome = common::propose_with_test_actor(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_002"),
        ],
        &after_revoke,
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn cannot_grant_standing_on_nonexistent_verification() {
    // grant_standing's first require: the verification_id must
    // reference a real IndependentlyVerifiedRevenue claim. Phantom
    // ids are rejected at admission time. This is what attaches the
    // word "standing" to a real admitted figure rather than just a
    // shape in the database.
    let outcome = common::propose_with_test_actor(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_phantom"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_001"),
        ],
        &State::default(),
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn cannot_grant_standing_on_superseded_verification() {
    // After correction, ver_001 is admitted but no longer current.
    // grant_standing requires CurrentVerification - new standing must
    // attach to the live figure (ver_002), not the historical one.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
        &invariants(),
    );

    // Attempt to grant standing on the now-superseded ver_001.
    let outcome = common::propose_with_test_actor(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_001"),
        ],
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));

    // But standing CAN be granted on ver_002 (the current figure).
    let post = must_accept(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_002"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_002"),
        ],
        pre,
        &invariants(),
    );
    assert!(has_claim(
        &post,
        "AdmissibleFor",
        &[subj("ver_002"), subj(verified_revenue::BANK_DEBT_SERVICE)],
    ));
}

// ============================================================
// Combined: correction retracts standing
// ============================================================

#[test]
fn correction_retracts_standing_on_prior_verification() {
    // The load-bearing combined test. A verifier corrects a figure
    // that already has multiple standings granted. Both standings on
    // the prior verification are retracted by pattern-based retract;
    // any historical decision admitted under those standings
    // survives. The authorities must re-grant standing on the
    // corrected figure if they accept it.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::BANK_DEBT_SERVICE,
        "credit_committee",
        "grant_bank_001",
    );
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::INVESTOR_REPORTING,
        "investor_relations",
        "grant_inv_001",
    );
    let pre = must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_bank_001"),
            subj("ver_001"),
        ],
        pre,
        &invariants(),
    );

    // Verifier corrects to 88. Both standings on ver_001 should be
    // retracted.
    let post = must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
        &invariants(),
    );

    // Historical verification + decision still in state.
    assert!(has_claim(
        &post,
        "IndependentlyVerifiedRevenue",
        &[asset(), period(), dec(91), subj("ver_001")],
    ));
    assert!(has_claim(
        &post,
        "DebtServiceRevenue",
        &[
            asset(),
            period(),
            dec(91),
            subj("decision_bank_001"),
            subj("ver_001"),
        ],
    ));
    // Both standings on ver_001 retracted.
    assert!(!has_claim(
        &post,
        "AdmissibleFor",
        &[subj("ver_001"), subj(verified_revenue::BANK_DEBT_SERVICE)],
    ));
    assert!(!has_claim(
        &post,
        "AdmissibleFor",
        &[subj("ver_001"), subj(verified_revenue::INVESTOR_REPORTING)],
    ));
    // The StandingGrantedBy provenance survives - the historical
    // record of who granted what is preserved.
    assert!(has_claim(
        &post,
        "StandingGrantedBy",
        &[
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_bank_001"),
        ],
    ));

    // A bank decision against ver_002 is rejected until the bank
    // re-grants standing on the corrected figure.
    let outcome = common::propose_with_test_actor(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(88),
            subj("decision_bank_002"),
            subj("ver_002"),
        ],
        &post,
        &invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));

    // After re-granting, the decision admits.
    let after_regrant = must_accept(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_002"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_bank_002"),
        ],
        post,
        &invariants(),
    );
    let final_state = must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(88),
            subj("decision_bank_002"),
            subj("ver_002"),
        ],
        after_regrant,
        &invariants(),
    );
    assert!(has_claim(
        &final_state,
        "DebtServiceRevenue",
        &[
            asset(),
            period(),
            dec(88),
            subj("decision_bank_002"),
            subj("ver_002"),
        ],
    ));
    // The OLD decision under ver_001 is still in admitted state - a
    // record of what the bank decided when ver_001 was current.
    assert!(has_claim(
        &final_state,
        "DebtServiceRevenue",
        &[
            asset(),
            period(),
            dec(91),
            subj("decision_bank_001"),
            subj("ver_001"),
        ],
    ));
}
