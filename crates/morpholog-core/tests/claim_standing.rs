//! Integration tests for the claim standing example
//! (`examples/03_claim_standing/`).
//!
//! Proves: standing is purpose-specific; multiple parallel standings
//! can attach to the same claim; decisions are gated at admission
//! time via `require AdmissibleFor(...)`; revocation prevents new
//! decisions but does not invalidate historical ones (because no
//! invariant ties decision claims to AdmissibleFor — that gating
//! is admission-time only); the underlying claim is never mutated.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec, has_claim, must_accept, subj};
use morpholog_core::examples::claim_standing;
use morpholog_core::{Invariant, Outcome, State};

fn standing_invariants() -> Vec<Invariant> {
    claim_standing::all_invariants()
}

/// Returns the state after admitting a single IV for (asset_a, p_2026_04, 91, ver_001).
fn state_with_iv() -> State {
    must_accept(
        &claim_standing::admit_independent_verification(),
        vec![subj("asset_a"), subj("p_2026_04"), dec(91), subj("ver_001")],
        State::default(),
        &standing_invariants(),
    )
}

/// Returns the state after admitting an IV and granting bank-debt-service standing.
fn state_with_bank_standing() -> State {
    must_accept(
        &claim_standing::grant_standing(),
        vec![
            subj("ver_001"),
            subj(claim_standing::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_001"),
        ],
        state_with_iv(),
        &standing_invariants(),
    )
}

#[test]
fn standing_is_purpose_specific() {
    // Bank standing granted; investor standing not.
    let state = state_with_bank_standing();

    // Debt-service decision: accepted.
    let after_decision = must_accept(
        &claim_standing::admit_debt_service_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        state.clone(),
        &standing_invariants(),
    );
    assert!(has_claim(
        &after_decision,
        "DebtServiceRevenue",
        &[
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
    ));

    // Investor report against the same verification: rejected (no investor standing).
    let outcome = common::propose_with_test_actor(
        &claim_standing::admit_investor_reported_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("report_001"),
            subj("ver_001"),
        ],
        &state,
        &standing_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}

#[test]
fn parallel_standings_permit_corresponding_decisions() {
    // Grant both bank and investor standings on the same verification.
    let state = must_accept(
        &claim_standing::grant_standing(),
        vec![
            subj("ver_001"),
            subj(claim_standing::INVESTOR_REPORTING),
            subj("investor_relations_office"),
            subj("grant_002"),
        ],
        state_with_bank_standing(),
        &standing_invariants(),
    );

    // Both AdmissibleFor claims coexist on the same verification.
    assert!(has_claim(
        &state,
        "AdmissibleFor",
        &[subj("ver_001"), subj(claim_standing::BANK_DEBT_SERVICE)],
    ));
    assert!(has_claim(
        &state,
        "AdmissibleFor",
        &[subj("ver_001"), subj(claim_standing::INVESTOR_REPORTING)],
    ));

    // Both decision types accept against the same verification.
    let after_debt = must_accept(
        &claim_standing::admit_debt_service_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        state,
        &standing_invariants(),
    );
    let after_both = must_accept(
        &claim_standing::admit_investor_reported_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("report_001"),
            subj("ver_001"),
        ],
        after_debt,
        &standing_invariants(),
    );

    assert!(has_claim(
        &after_both,
        "DebtServiceRevenue",
        &[
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
    ));
    assert!(has_claim(
        &after_both,
        "InvestorReportedRevenue",
        &[
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("report_001"),
            subj("ver_001"),
        ],
    ));
}

#[test]
fn revocation_blocks_new_decisions_but_preserves_history() {
    // 1. Admit IV, grant bank standing, admit decision_001.
    let after_decision = must_accept(
        &claim_standing::admit_debt_service_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        state_with_bank_standing(),
        &standing_invariants(),
    );

    // 2. Revoke bank standing. AdmissibleFor retracted, StandingRevoked recorded.
    //    Crucially, the revoke transformation commits — no invariant ties
    //    the historical decision_001 to AdmissibleFor.
    let after_revoke = must_accept(
        &claim_standing::revoke_standing(),
        vec![
            subj("ver_001"),
            subj(claim_standing::BANK_DEBT_SERVICE),
            subj("revoke_001"),
        ],
        after_decision,
        &standing_invariants(),
    );

    // Historical decision survives.
    assert!(
        has_claim(
            &after_revoke,
            "DebtServiceRevenue",
            &[
                subj("asset_a"),
                subj("p_2026_04"),
                dec(91),
                subj("decision_001"),
                subj("ver_001"),
            ],
        ),
        "historical decision_001 must survive revocation"
    );

    // Underlying IV unchanged.
    assert!(has_claim(
        &after_revoke,
        "IndependentlyVerifiedRevenue",
        &[subj("asset_a"), subj("p_2026_04"), dec(91), subj("ver_001")],
    ));

    // Grant provenance preserved; revocation recorded.
    assert!(has_claim(
        &after_revoke,
        "StandingGrantedBy",
        &[
            subj("ver_001"),
            subj(claim_standing::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_001"),
        ],
    ));
    assert!(has_claim(
        &after_revoke,
        "StandingRevoked",
        &[
            subj("ver_001"),
            subj(claim_standing::BANK_DEBT_SERVICE),
            subj("revoke_001"),
        ],
    ));

    // Active admissibility gone.
    assert!(!has_claim(
        &after_revoke,
        "AdmissibleFor",
        &[subj("ver_001"), subj(claim_standing::BANK_DEBT_SERVICE)],
    ));

    // 3. A NEW debt-service decision is rejected (admission gate fails).
    let outcome = common::propose_with_test_actor(
        &claim_standing::admit_debt_service_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("decision_002"),
            subj("ver_001"),
        ],
        &after_revoke,
        &standing_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}

#[test]
fn wrong_amount_rejected_even_with_valid_standing() {
    // Verified amount is 91. Decision claims amount 92 against the same
    // verification id. The IV-match require fails: the kernel cannot
    // find an IndependentlyVerifiedRevenue claim matching all four
    // positional args.
    let state = state_with_bank_standing();
    let outcome = common::propose_with_test_actor(
        &claim_standing::admit_debt_service_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(92),
            subj("decision_001"),
            subj("ver_001"),
        ],
        &state,
        &standing_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}

#[test]
fn cannot_admit_decision_without_iv() {
    // Grant standing on a verification id that has no IV claim in state.
    // (admissibility_has_provenance is satisfied because StandingGrantedBy
    // is the provenance — it does not require the underlying claim to
    // exist as a separate predicate. That is by design: standing claims
    // and the claims they confer standing on are independent assertions.)
    let state = must_accept(
        &claim_standing::grant_standing(),
        vec![
            subj("ver_999"),
            subj(claim_standing::BANK_DEBT_SERVICE),
            subj("credit_committee"),
            subj("grant_999"),
        ],
        State::default(),
        &standing_invariants(),
    );

    // Decision against ver_999 fails because there is no
    // IndependentlyVerifiedRevenue(_, _, _, ver_999) claim.
    let outcome = common::propose_with_test_actor(
        &claim_standing::admit_debt_service_revenue(),
        vec![
            subj("asset_a"),
            subj("p_2026_04"),
            dec(91),
            subj("decision_001"),
            subj("ver_999"),
        ],
        &state,
        &standing_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}
