//! Integration tests for the revenue restatement example
//! (`examples/02_revenue_restatement/`).
//!
//! Proves the runtime can model **temporal correction**: when
//! authoritative figures are revised after the fact, history is
//! preserved, the "in-force" view moves cleanly, and no metadata
//! is added to any claim.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, dec, has_claim, must_accept, subj};
use morpholog_core::examples::revenue_restatement;
use morpholog_core::{Outcome, State, propose};

#[test]
fn correct_independent_verification_retracts_dependent_current_pointer() {
    let invariants = vec![
        revenue_restatement::current_recognition_matches_current_verification(),
        revenue_restatement::at_most_one_current_recognition_per_asset_period(),
        revenue_restatement::at_most_one_direct_successor(),
    ];

    let pre = State {
        claims: vec![
            claim_instance(
                "IndependentlyVerifiedRevenue",
                &[subj("asset_a"), subj("p_2026_04"), dec(92), subj("ver_001")],
            ),
            claim_instance(
                "BankRecognisedRevenue",
                &[subj("asset_a"), subj("p_2026_04"), dec(92), subj("rec_001")],
            ),
            claim_instance(
                "CurrentBankRecognition",
                &[subj("asset_a"), subj("p_2026_04"), subj("rec_001")],
            ),
        ],
    };

    let args = vec![
        subj("asset_a"),
        subj("p_2026_04"),
        dec(91),
        subj("ver_002"),
        subj("ver_001"),
    ];

    let outcome = propose(
        &revenue_restatement::correct_independent_verification(),
        args,
        &pre,
        &invariants,
    )
    .expect("propose should not error");

    let Outcome::Accepted {
        candidate_state,
        asserted_claims,
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Accepted, got {outcome:?}");
    };

    assert_eq!(
        asserted_claims.len(),
        2,
        "should assert new IV + Supersedes"
    );
    assert_eq!(
        retracted_claims.len(),
        1,
        "should retract the current pointer"
    );
    assert_eq!(retracted_claims[0].predicate, "CurrentBankRecognition");
    assert_eq!(emitted_intents.len(), 1);
    assert_eq!(emitted_intents[0].name, "VerificationCorrected");

    // Historical BankRecognisedRevenue must still be in candidate state.
    assert!(
        candidate_state.claims.iter().any(|c| {
            c.predicate == "BankRecognisedRevenue"
                && c.args == vec![subj("asset_a"), subj("p_2026_04"), dec(92), subj("rec_001")]
        }),
        "historical BankRecognisedRevenue must be preserved"
    );

    // CurrentBankRecognition must be gone.
    assert!(
        !candidate_state
            .claims
            .iter()
            .any(|c| c.predicate == "CurrentBankRecognition"),
        "current bank recognition pointer must be retracted"
    );

    // New verification must be present.
    assert!(
        candidate_state.claims.iter().any(|c| {
            c.predicate == "IndependentlyVerifiedRevenue"
                && c.args == vec![subj("asset_a"), subj("p_2026_04"), dec(91), subj("ver_002")]
        }),
        "new IndependentlyVerifiedRevenue must be present"
    );

    // Supersession recorded.
    assert!(
        candidate_state.claims.iter().any(|c| {
            c.predicate == "Supersedes" && c.args == vec![subj("ver_002"), subj("ver_001")]
        }),
        "Supersedes(ver_002, ver_001) must be recorded"
    );
}

#[test]
fn full_restatement_chain_preserves_history_and_updates_pointer() {
    let invariants = vec![
        revenue_restatement::current_recognition_matches_current_verification(),
        revenue_restatement::at_most_one_current_recognition_per_asset_period(),
        revenue_restatement::at_most_one_direct_successor(),
    ];

    let a = subj("asset_a");
    let p = subj("p_2026_04");

    // 1. Admit initial independent verification at 92.
    let s1 = must_accept(
        &revenue_restatement::admit_independent_verification(),
        vec![a.clone(), p.clone(), dec(92), subj("ver_001")],
        State::default(),
        &invariants,
    );

    // 2. Bank recognises 92, rec_001. I1 holds against the current verification.
    let s2 = must_accept(
        &revenue_restatement::recognise_bank_revenue(),
        vec![a.clone(), p.clone(), dec(92), subj("rec_001")],
        s1,
        &invariants,
    );

    // 3. Verifier corrects to 91 (ver_002 supersedes ver_001). The
    // dependent CurrentBankRecognition is retracted as part of the
    // verifier's transformation body, so I1 is vacuously satisfied
    // (no current pointer remains).
    let s3 = must_accept(
        &revenue_restatement::correct_independent_verification(),
        vec![
            a.clone(),
            p.clone(),
            dec(91),
            subj("ver_002"),
            subj("ver_001"),
        ],
        s2,
        &invariants,
    );

    // 4. Bank restates to 91 with a new recognition_id. New current
    // pointer; new Supersedes link.
    let s4 = must_accept(
        &revenue_restatement::restate_bank_revenue(),
        vec![
            a.clone(),
            p.clone(),
            dec(91),
            subj("rec_002"),
            subj("rec_001"),
        ],
        s3,
        &invariants,
    );

    // Final state: 2 IV + 2 BR + 2 Supersedes + 1 Current = 7 claims.
    assert_eq!(s4.claims.len(), 7);

    assert!(has_claim(
        &s4,
        "IndependentlyVerifiedRevenue",
        &[a.clone(), p.clone(), dec(92), subj("ver_001")],
    ));
    assert!(has_claim(
        &s4,
        "IndependentlyVerifiedRevenue",
        &[a.clone(), p.clone(), dec(91), subj("ver_002")],
    ));
    assert!(has_claim(
        &s4,
        "Supersedes",
        &[subj("ver_002"), subj("ver_001")],
    ));

    assert!(
        has_claim(
            &s4,
            "BankRecognisedRevenue",
            &[a.clone(), p.clone(), dec(92), subj("rec_001")],
        ),
        "historical BR(92, rec_001) must be preserved"
    );
    assert!(has_claim(
        &s4,
        "BankRecognisedRevenue",
        &[a.clone(), p.clone(), dec(91), subj("rec_002")],
    ));
    assert!(has_claim(
        &s4,
        "Supersedes",
        &[subj("rec_002"), subj("rec_001")],
    ));

    assert!(
        has_claim(
            &s4,
            "CurrentBankRecognition",
            &[a.clone(), p.clone(), subj("rec_002")],
        ),
        "current pointer must be rec_002"
    );
    assert!(
        !has_claim(
            &s4,
            "CurrentBankRecognition",
            &[a.clone(), p.clone(), subj("rec_001")],
        ),
        "old current pointer must be retracted"
    );
}
