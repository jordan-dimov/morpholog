//! Integration tests for the verified-revenue example
//! (`examples/02_verified_revenue/`).
//!
//! Two patterns in one programme:
//!
//! - **Restatement.** A verifier corrects a figure: the
//!   `CurrentVerification` pointer moves, `Supersedes` records the
//!   lineage, standing on the old figure is withdrawn, and past
//!   decisions survive.
//!
//! - **Admissibility for purpose.** Two authorities grant standing for
//!   different decisions on the same figure. Standing can be revoked;
//!   the figure never changes, and past decisions survive.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, dec, has_claim, subj};
use morpholog_core::{Outcome, State};
use morpholog_examples::verified_revenue;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&verified_revenue::program()))
}

fn asset() -> morpholog_core::EvalValue {
    subj("asset_a")
}

fn period() -> morpholog_core::EvalValue {
    subj("p_2026_04")
}

fn admit_iv(state: State, amount: i64, ver: &str) -> State {
    ex().must_accept(
        &verified_revenue::admit_independent_verification(),
        vec![asset(), period(), dec(amount), subj(ver)],
        state,
    )
}

fn grant(state: State, ver: &str, purpose: &str, authority: &str, grant_id: &str) -> State {
    ex().must_accept(
        &verified_revenue::grant_standing(),
        vec![subj(ver), subj(purpose), subj(authority), subj(grant_id)],
        state,
    )
}

// ============================================================
// Restatement pattern
// ============================================================

#[test]
fn admit_then_correct_preserves_history_and_moves_pointer() {
    // Admit 91, then correct to 88. The original figure stays admitted;
    // the pointer moves and Supersedes records the lineage.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let post = ex().must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
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
    // The gate in correct_independent_verification and
    // supersedes_unique_by_prior_verification_id prevent a forked chain.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = ex().must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
    );
    ex().must_reject(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(85), subj("ver_003"), subj("ver_001")],
        &pre,
    );
}

#[test]
fn second_admission_against_existing_current_is_rejected() {
    // An (asset, period) has at most one current verification; replacing
    // it goes through correct_independent_verification.
    let pre = admit_iv(State::default(), 91, "ver_001");
    ex().must_reject(
        &verified_revenue::admit_independent_verification(),
        vec![asset(), period(), dec(95), subj("ver_002")],
        &pre,
    );
}

/// Corrections chain to any depth. Being current is a pointer read, not a
/// walk along the chain, so depth never matters. The one thing refused is a
/// fork: correcting a version already superseded, at any depth.
#[test]
fn correction_chains_run_to_any_depth_and_stale_versions_stay_stale() {
    let mut state = admit_iv(State::default(), 91, "ver_001");
    let chain = ["ver_001", "ver_002", "ver_003", "ver_004", "ver_005"];
    for (depth, pair) in chain.windows(2).enumerate() {
        state = ex().must_accept(
            &verified_revenue::correct_independent_verification(),
            vec![
                asset(),
                period(),
                dec(90 - depth as i64),
                subj(pair[1]),
                subj(pair[0]),
            ],
            state,
        );
    }

    // The pointer sits on the head; every earlier version is admitted
    // history, none is current.
    assert!(has_claim(
        &state,
        "CurrentVerification",
        &[asset(), period(), subj("ver_005")]
    ));
    for pair in chain.windows(2) {
        let (prior, next) = (pair[0], pair[1]);
        assert!(
            !has_claim(
                &state,
                "CurrentVerification",
                &[asset(), period(), subj(prior)]
            ),
            "{prior} must no longer be current"
        );
        assert!(has_claim(&state, "Supersedes", &[subj(next), subj(prior)]));
    }

    // Standing attaches to the head only, whatever the chain's depth.
    for stale in &chain[..4] {
        ex().must_reject(
            &verified_revenue::grant_standing(),
            vec![
                subj(stale),
                subj(verified_revenue::BANK_DEBT_SERVICE),
                subj("credit_committee"),
                subj(&format!("grant_{stale}")),
            ],
            &state,
        );
    }
    let state = grant(
        state,
        "ver_005",
        verified_revenue::BANK_DEBT_SERVICE,
        "credit_committee",
        "grant_head",
    );
    assert!(has_claim(
        &state,
        "AdmissibleFor",
        &[subj("ver_005"), subj(verified_revenue::BANK_DEBT_SERVICE)],
    ));

    // A fork anywhere in the chain is refused: correcting a version that
    // was itself already corrected, whether the first or the third.
    for forked in ["ver_001", "ver_003"] {
        ex().must_reject(
            &verified_revenue::correct_independent_verification(),
            vec![asset(), period(), dec(50), subj("ver_fork"), subj(forked)],
            &state,
        );
    }
    // Correcting the head is not a fork: the chain grows, and standing on
    // the old head is withdrawn while past decisions stand.
    let state = ex().must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(70), subj("ver_006"), subj("ver_005")],
        state,
    );
    assert!(has_claim(
        &state,
        "CurrentVerification",
        &[asset(), period(), subj("ver_006")]
    ));
    assert!(
        !has_claim(
            &state,
            "AdmissibleFor",
            &[subj("ver_005"), subj(verified_revenue::BANK_DEBT_SERVICE)],
        ),
        "standing on the superseded head is retracted by the correction"
    );
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

    // With no bank standing, admit_debt_service_revenue is rejected. The
    // trace shows why: the figure exists (first require held) but the
    // standing does not (second require rejected).
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
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        &t,
        &transition,
        &pre,
        &verified_revenue::all_invariants(),
        &verified_revenue::definitions(),
    ) else {
        panic!("expected Completed");
    };
    assert!(matches!(outcome, Outcome::Rejected { .. }));
    let require_outcomes: Vec<(&str, &RequireOutcome)> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Require {
                expression,
                outcome,
                ..
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
    let post = ex().must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        pre,
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
    ex().must_reject(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        &pre,
    );
}

#[test]
fn revoking_standing_blocks_future_but_preserves_past() {
    // A decision admitted under valid standing survives a later
    // revocation, because standing is a gate, not an invariant.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = grant(
        pre,
        "ver_001",
        verified_revenue::BANK_DEBT_SERVICE,
        "credit_committee",
        "grant_001",
    );
    let pre = ex().must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        pre,
    );
    let after_revoke = ex().must_accept(
        &verified_revenue::revoke_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("revoke_001"),
        ],
        pre,
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
    ex().must_reject(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_002"),
            subj("ver_001"),
        ],
        &after_revoke,
    );
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
    let after_revoke = ex().must_accept(
        &verified_revenue::revoke_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("revoke_001"),
        ],
        pre,
    );
    ex().must_reject(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_002"),
        ],
        &after_revoke,
    );
}

#[test]
fn cannot_grant_standing_on_nonexistent_verification() {
    // Standing must attach to a real admitted figure: a phantom
    // verification_id is refused.
    ex().must_reject(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_phantom"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_001"),
        ],
        &State::default(),
    );
}

#[test]
fn cannot_grant_standing_on_superseded_verification() {
    // After correction ver_001 is no longer current, and new standing
    // must attach to the live figure, ver_002.
    let pre = admit_iv(State::default(), 91, "ver_001");
    let pre = ex().must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
    );

    // Attempt to grant standing on the now-superseded ver_001.
    ex().must_reject(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_001"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_001"),
        ],
        &pre,
    );

    // But standing CAN be granted on ver_002 (the current figure).
    let post = ex().must_accept(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_002"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_002"),
        ],
        pre,
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
    // Correcting a figure withdraws every standing on it, while decisions
    // made under those standings survive. Authorities must re-grant on
    // the corrected figure.
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
    let pre = ex().must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_bank_001"),
            subj("ver_001"),
        ],
        pre,
    );

    // Verifier corrects to 88. Both standings on ver_001 should be
    // retracted.
    let post = ex().must_accept(
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        pre,
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
    // StandingGrantedBy survives: who granted what stays on the record.
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
    ex().must_reject(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(88),
            subj("decision_bank_002"),
            subj("ver_002"),
        ],
        &post,
    );

    // After re-granting, the decision admits.
    let after_regrant = ex().must_accept(
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_002"),
            subj(verified_revenue::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_bank_002"),
        ],
        post,
    );
    let final_state = ex().must_accept(
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(88),
            subj("decision_bank_002"),
            subj("ver_002"),
        ],
        after_regrant,
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
