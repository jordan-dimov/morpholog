//! Integration tests for as-of evaluation.
//!
//! Covers `reconstruct_state_at`, `list_claims_at` and `list_derived_at`.
//! Most tests share a ledger restatement chain: post entry_001 at 100,
//! post entry_002 at 200, restate entry_001 to 150. That leaves three
//! trial balances in one database. `list_derived` sees only the last;
//! the as-of helpers must recover the other two.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ClaimInstance, EvalValue};
use morpholog_examples::{approval_controls, double_entry_ledger, verified_revenue};
use morpholog_postgres::{
    PgError, PgPool, list_claims, list_claims_at, list_derived, list_derived_at,
    reconstruct_state_at,
};
use rust_decimal::Decimal;
use uuid::Uuid;

mod common;
use common::{dec, subj};
use common::{expect_committed, reset_db, test_pool};

// ============================================================
// Test infrastructure
// ============================================================

/// Post entry_001 at 100, post entry_002 at 200, restate entry_001 to
/// 150. Returns the three transition ids in order.
async fn three_step_ledger(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let period = subj("p_as_of");

    let tid1 = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
            &common::compiled(double_entry_ledger::program()),
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_001"),
                subj("d_2026_05_01"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(100),
            ],
        )
        .await
        .unwrap(),
    );

    let tid2 = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
            &common::compiled(double_entry_ledger::program()),
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_002"),
                subj("d_2026_05_02"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(200),
            ],
        )
        .await
        .unwrap(),
    );

    let tid3 = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
            &common::compiled(double_entry_ledger::program()),
            &double_entry_ledger::restate_entry(),
            vec![
                subj("entry_001_v2"),
                subj("entry_001"),
                subj("d_2026_05_10"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(150),
            ],
        )
        .await
        .unwrap(),
    );

    (tid1, tid2, tid3)
}

/// Find the TrialBalanceRow for `account_name` in `rows` and assert
/// its balance matches `amount`.
fn assert_balance(rows: &[ClaimInstance], account_name: &str, amount: i64) {
    let account = EvalValue::Subject(account_name.into());
    let expected = EvalValue::Decimal(Decimal::new(amount, 0));
    let row = rows
        .iter()
        .find(|r| r.predicate.as_str() == "TrialBalanceRow" && r.args.first() == Some(&account))
        .unwrap_or_else(|| panic!("no TrialBalanceRow for `{account_name}` in {rows:?}"));
    assert_eq!(
        row.args.get(1),
        Some(&expected),
        "balance for {account_name} did not match expected {amount}: row was {row:?}"
    );
}

// ============================================================
// Tests
// ============================================================

/// reconstruct_state_at recovers the pre-restatement state.
#[tokio::test]
async fn reconstruct_state_at_recovers_pre_restatement_state() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (tid1, tid2, _tid3) = three_step_ledger(&pool).await;

    // As of tid2: two entries, no restatement yet. 2 JournalEntry + 4
    // JournalLine = 6 claims.
    let state_at_tid2 = reconstruct_state_at(&pool, tid2).await.unwrap();
    assert_eq!(
        state_at_tid2.len(),
        6,
        "tid2 should have 2 entries x (1 entry header + 2 lines) = 6 claims"
    );

    // As-of tid1: only entry_001 (100). 1 entry + 2 lines = 3.
    let state_at_tid1 = reconstruct_state_at(&pool, tid1).await.unwrap();
    assert_eq!(
        state_at_tid1.len(),
        3,
        "tid1 should have 1 entry x 3 claims = 3"
    );
}

/// reconstruct_state_at at the latest transition matches current
/// `list_claims` as a set.
#[tokio::test]
async fn reconstruct_state_at_at_latest_equals_current_claims() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let state_at_tid3 = reconstruct_state_at(&pool, tid3).await.unwrap();
    let current = list_claims(&pool).await.unwrap();

    // Compare as sets: the two functions order claims differently.
    // ClaimInstance is not Hash, so use length plus mutual containment.
    assert_eq!(
        state_at_tid3.len(),
        current.len(),
        "claim counts must match"
    );
    for c in state_at_tid3.claims() {
        assert!(
            current.contains(c),
            "reconstructed claim missing from current: {c:?}"
        );
    }
}

/// reconstruct_state_at errors with TransitionNotFound for any unknown
/// id, wherever it sorts relative to known ids.
#[tokio::test]
async fn reconstruct_state_at_returns_transition_not_found_for_unknown_id() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = three_step_ledger(&pool).await;

    let unknown = Uuid::nil();
    let err = reconstruct_state_at(&pool, unknown)
        .await
        .expect_err("unknown transition_id must be an error");
    match err {
        PgError::TransitionNotFound(id) => {
            assert_eq!(id, unknown, "the error must carry the missing id");
        }
        other => panic!("expected TransitionNotFound, got {other:?}"),
    }

    // A fresh v7 UUID must be an error too, not treated as current state.
    let bogus = Uuid::now_v7();
    let err = reconstruct_state_at(&pool, bogus)
        .await
        .expect_err("a freshly-generated id that does not exist must be an error");
    assert!(matches!(err, PgError::TransitionNotFound(_)));
}

/// list_claims_at returns the claims as they were at that transition.
#[tokio::test]
async fn list_claims_at_differs_from_current_after_state_change() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, tid2, _tid3) = three_step_ledger(&pool).await;

    let at_tid2 = list_claims_at(&pool, tid2).await.unwrap();
    let current = list_claims(&pool).await.unwrap();

    assert_ne!(
        at_tid2.len(),
        current.len(),
        "historical and current claim sets should differ in size after a restatement"
    );
    // tid2 had 6 claims (2 entries x 3); current has 6 + the
    // restatement's 4 (entry + 2 lines + supersedes) = 10.
    assert_eq!(at_tid2.len(), 6);
    assert_eq!(current.len(), 10);
}

/// list_derived_at recovers the historical trial balance.
#[tokio::test]
async fn list_derived_at_recovers_historical_trial_balance() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (tid1, tid2, _tid3) = three_step_ledger(&pool).await;

    let trial_balance = double_entry_ledger::trial_balance_row();

    // At tid1: only entry_001 with amount 100. Cash 100, revenue -100.
    let at_tid1 = list_derived_at(
        &pool,
        &trial_balance,
        &double_entry_ledger::definitions(),
        tid1,
    )
    .await
    .unwrap();
    assert_balance(&at_tid1, "account_cash", 100);
    assert_balance(&at_tid1, "account_revenue", -100);

    // At tid2: entry_001 (100) + entry_002 (200). Cash 300, revenue -300.
    let at_tid2 = list_derived_at(
        &pool,
        &trial_balance,
        &double_entry_ledger::definitions(),
        tid2,
    )
    .await
    .unwrap();
    assert_balance(&at_tid2, "account_cash", 300);
    assert_balance(&at_tid2, "account_revenue", -300);
}

/// list_derived_at at the latest transition equals list_derived.
#[tokio::test]
async fn list_derived_at_at_latest_equals_list_derived() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let trial_balance = double_entry_ledger::trial_balance_row();
    let at_tid3 = list_derived_at(
        &pool,
        &trial_balance,
        &double_entry_ledger::definitions(),
        tid3,
    )
    .await
    .unwrap();
    let current = list_derived(&pool, &trial_balance, &double_entry_ledger::definitions())
        .await
        .unwrap();

    assert_eq!(
        at_tid3, current,
        "list_derived_at at the latest transition must equal current list_derived"
    );
}

/// list_derived_at ignores unrelated predicates during replay: after an
/// unrelated commit, the trial balance as of that commit is unchanged.
#[tokio::test]
async fn list_derived_at_ignores_unrelated_predicates_under_noise() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let trial_balance = double_entry_ledger::trial_balance_row();
    let baseline = list_derived_at(
        &pool,
        &trial_balance,
        &double_entry_ledger::definitions(),
        tid3,
    )
    .await
    .unwrap();

    // An IndependentlyVerifiedRevenue claim, which the trial balance
    // does not read.
    let new_tid = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &common::compiled(verified_revenue::program()),
            &verified_revenue::admit_independent_verification(),
            vec![
                subj("noise_asset"),
                subj("p_noise"),
                dec(7),
                subj("noise_ver"),
            ],
        )
        .await
        .unwrap(),
    );

    let after_noise = list_derived_at(
        &pool,
        &trial_balance,
        &double_entry_ledger::definitions(),
        new_tid,
    )
    .await
    .unwrap();
    assert_eq!(
        baseline, after_noise,
        "an unrelated transformation must not change the trial balance, \
         even when the as-of coordinate moves past it"
    );
}

/// The trial balance is correct over history that also holds
/// predicates it does not read (JournalEntry, Supersedes).
///
/// This checks output only. A scoped replay that loaded everything
/// would still pass; the bench's list_scoped timing catches that.
#[tokio::test]
async fn list_derived_at_returns_correct_output_under_mixed_predicate_history() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let full = reconstruct_state_at(&pool, tid3).await.unwrap();
    // Full state at tid3 holds JournalEntry, JournalLine and Supersedes.
    let predicates: std::collections::HashSet<&str> =
        full.claims().iter().map(|c| c.predicate.as_str()).collect();
    assert!(predicates.contains("JournalEntry"));
    assert!(predicates.contains("JournalLine"));
    assert!(predicates.contains("Supersedes"));

    // The trial balance replays only JournalLine. Skipping it would
    // make the balance wrong.
    let trial_balance = double_entry_ledger::trial_balance_row();
    let rows = list_derived_at(
        &pool,
        &trial_balance,
        &double_entry_ledger::definitions(),
        tid3,
    )
    .await
    .unwrap();
    // Current state: entry_001 (100) + entry_002 (200) + entry_001_v2 (150) = 450 on cash.
    assert_balance(&rows, "account_cash", 450);
    assert_balance(&rows, "account_revenue", -450);
}

/// Replay applies a retraction made in a later transition.
///
/// The ledger tests never retract. Here
/// `correct_independent_verification` retracts the
/// `CurrentVerification` pointer for ver_001 and admits one for
/// ver_002. As of tid1 the ver_001 pointer is present; as of tid2 it
/// is gone and ver_002's is present.
#[tokio::test]
async fn reconstruct_state_at_applies_cross_transition_retractions() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let asset = subj("asset_a");
    let period = subj("p_2026_04");

    // Step 1: admit IV at 92. Asserts IV + CurrentVerification(ver_001).
    let tid1 = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &common::compiled(verified_revenue::program()),
            &verified_revenue::admit_independent_verification(),
            vec![asset.clone(), period.clone(), dec(92), subj("ver_001")],
        )
        .await
        .unwrap(),
    );

    // Step 2: correct the verification to 91 (ver_002).
    let tid2 = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &common::compiled(verified_revenue::program()),
            &verified_revenue::correct_independent_verification(),
            vec![asset, period, dec(91), subj("ver_002"), subj("ver_001")],
        )
        .await
        .unwrap(),
    );

    // At tid1: CurrentVerification(_, _, ver_001) IS present.
    let claims_at_tid1 = list_claims_at(&pool, tid1).await.unwrap();
    let pointer_at_tid1 = claims_at_tid1
        .iter()
        .filter(|c| c.predicate.as_str() == "CurrentVerification" && c.args[2] == subj("ver_001"))
        .count();
    assert_eq!(
        pointer_at_tid1, 1,
        "CurrentVerification(ver_001) should be present as of tid1"
    );

    // At tid2 (after the retraction): the ver_001 pointer is gone and
    // the ver_002 pointer is present.
    let claims_at_tid2 = list_claims_at(&pool, tid2).await.unwrap();
    let stale_pointer_at_tid2 = claims_at_tid2
        .iter()
        .filter(|c| c.predicate.as_str() == "CurrentVerification" && c.args[2] == subj("ver_001"))
        .count();
    let new_pointer_at_tid2 = claims_at_tid2
        .iter()
        .filter(|c| c.predicate.as_str() == "CurrentVerification" && c.args[2] == subj("ver_002"))
        .count();
    assert_eq!(
        stale_pointer_at_tid2, 0,
        "CurrentVerification(ver_001) should be retracted as of tid2"
    );
    assert_eq!(
        new_pointer_at_tid2, 1,
        "CurrentVerification(ver_002) should be present as of tid2"
    );

    // The correction leaves IV1 standing: both verifications are
    // admitted at tid2.
    let iv_at_tid2 = claims_at_tid2
        .iter()
        .filter(|c| c.predicate.as_str() == "IndependentlyVerifiedRevenue")
        .count();
    assert_eq!(
        iv_at_tid2, 2,
        "both IV1 (original 92) and IV2 (corrected 91) should be admitted at tid2"
    );
}

/// With an empty audit log, any id is `TransitionNotFound`, not an
/// empty state.
#[tokio::test]
async fn reconstruct_state_at_on_empty_audit_log_is_transition_not_found() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // No commits at all. The audit table is empty.

    let err = reconstruct_state_at(&pool, Uuid::now_v7())
        .await
        .expect_err("reconstruct against an empty audit log must error");
    assert!(
        matches!(err, PgError::TransitionNotFound(_)),
        "empty audit log should still produce TransitionNotFound, not an empty state; got {err:?}"
    );
}

/// A historical read keeps claims in the order replay admitted them,
/// and a claim retracted and re-admitted moves to the end - the same
/// rule the kernel's own state follows.
#[tokio::test]
async fn list_claims_at_moves_a_readmitted_claim_to_the_tail() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(approval_controls::program());
    let grant = approval_controls::grant_approval_authority();
    let revoke = approval_controls::revoke_approval_authority();
    let may_approve = |who: &str| ClaimInstance {
        predicate: "MayApprove".into(),
        args: vec![subj(who), subj("invoice")],
    };
    for (t, who) in [(&grant, "p1"), (&grant, "p2"), (&revoke, "p1")] {
        expect_committed(
            common::propose_pg_with_test_actor(
                &pool,
                &compiled,
                t,
                vec![subj(who), subj("invoice")],
            )
            .await
            .unwrap(),
        );
    }
    let readmitted = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &compiled,
            &grant,
            vec![subj("p1"), subj("invoice")],
        )
        .await
        .unwrap(),
    );
    let claims = list_claims_at(&pool, readmitted).await.unwrap();
    assert_eq!(
        claims,
        vec![may_approve("p2"), may_approve("p1")],
        "p1 was granted first, revoked, and granted again: it sits at the tail"
    );
}
