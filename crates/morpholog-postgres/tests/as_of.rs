//! Integration tests for as-of evaluation.
//!
//! These tests cover the production helpers `reconstruct_state_at`,
//! `list_claims_at`, `list_derived_at`, and (indirectly via the
//! last) the internal `reconstruct_state_at_for_predicates`. The
//! scenario most tests share is the double-entry-ledger restatement
//! chain: post entry_001 at 100, post entry_002 at 200, restate
//! entry_001 to 150. Three distinct trial balances exist in the
//! same database; only the last is reachable through the
//! current-state `list_derived`. The as-of helpers must recover
//! the other two.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ClaimInstance, EvalValue};
use morpholog_examples::{double_entry_ledger, verified_revenue};
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, list_claims, list_claims_at, list_derived, list_derived_at,
    reconstruct_state_at,
};
use rust_decimal::Decimal;
use uuid::Uuid;

mod common;

// ============================================================
// Test infrastructure
// ============================================================

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

async fn reset_db(pool: &PgPool) {
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit CASCADE")
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
}

fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

/// Unwrap a `Committed` outcome and return its `transition_id`,
/// panicking if the transformation rejected. Each step of the chain
/// is required to commit; a rejection is a fixture or kernel bug,
/// not a business outcome.
fn expect_committed(outcome: PgProposalOutcome) -> Uuid {
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => {
            panic!("expected Committed; got Rejected({reason})")
        }
    }
}

/// Three-step ledger fixture: post entry_001 at 100, post entry_002
/// at 200, restate entry_001 to 150. Returns the three captured
/// `transition_id`s in order. Used by most of the tests below.
async fn three_step_ledger(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let invariants = double_entry_ledger::all_invariants();
    let period = subj("p_as_of");

    let tid1 = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_001"),
                subj("d_2026_05_01"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(100),
            ],
            &invariants,
        )
        .await
        .unwrap(),
    );

    let tid2 = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_002"),
                subj("d_2026_05_02"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(200),
            ],
            &invariants,
        )
        .await
        .unwrap(),
    );

    let tid3 = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
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
            &invariants,
        )
        .await
        .unwrap(),
    );

    (tid1, tid2, tid3)
}

/// Find the TrialBalanceRow for `account_name` in `rows` and assert
/// its balance matches `amount`.
fn assert_balance(rows: &[ClaimInstance], account_name: &str, amount: i64) {
    let account = EvalValue::Subject(account_name.to_string());
    let expected = EvalValue::Decimal(Decimal::new(amount, 0));
    let row = rows
        .iter()
        .find(|r| r.predicate == "TrialBalanceRow" && r.args.first() == Some(&account))
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

/// Test #1: reconstruct_state_at recovers the pre-restatement state.
/// Inherits the spike's headline scenario.
#[tokio::test]
async fn reconstruct_state_at_recovers_pre_restatement_state() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (tid1, tid2, _tid3) = three_step_ledger(&pool).await;

    // As-of tid2: entry_001 (100) and entry_002 (200), no
    // restatement yet. That's 2 JournalEntry + 4 JournalLine = 6
    // claims total.
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

/// Test #2: reconstruct_state_at at the latest transition matches
/// current `list_claims` as a set.
#[tokio::test]
async fn reconstruct_state_at_at_latest_equals_current_claims() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let state_at_tid3 = reconstruct_state_at(&pool, tid3).await.unwrap();
    let current = list_claims(&pool).await.unwrap();

    // Compare as sets, not as ordered lists: list_claims orders by
    // (asserted_at, predicate, args::text); reconstruct_state_at
    // returns construction (replay) order. ClaimInstance does not
    // derive Hash, so check set equality by length plus mutual
    // containment - O(N^2) but the sets are tiny here.
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

/// Test #3: reconstruct_state_at errors with TransitionNotFound for
/// an unknown UUID. Crisp contract: every unknown id is an error,
/// including ids ordered between/before/after known ids.
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

    // A fresh random v7 UUID that does not exist in audit must also
    // be an error - not silently treated as current state. This is
    // the load-bearing test for the "no magical edge cases" contract.
    let bogus = Uuid::now_v7();
    let err = reconstruct_state_at(&pool, bogus)
        .await
        .expect_err("a freshly-generated id that does not exist must be an error");
    assert!(matches!(err, PgError::TransitionNotFound(_)));
}

/// Test #4: list_claims_at returns the claim set as it was at the
/// supplied moment, differing from current when state has changed
/// since.
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

/// Test #5: list_derived_at recovers the historical trial balance.
/// The headline as-of property.
#[tokio::test]
async fn list_derived_at_recovers_historical_trial_balance() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (tid1, tid2, _tid3) = three_step_ledger(&pool).await;

    let trial_balance = double_entry_ledger::trial_balance_row();

    // At tid1: only entry_001 with amount 100. Cash 100, revenue -100.
    let at_tid1 = list_derived_at(&pool, &trial_balance, tid1).await.unwrap();
    assert_balance(&at_tid1, "account_cash", 100);
    assert_balance(&at_tid1, "account_revenue", -100);

    // At tid2: entry_001 (100) + entry_002 (200). Cash 300, revenue -300.
    let at_tid2 = list_derived_at(&pool, &trial_balance, tid2).await.unwrap();
    assert_balance(&at_tid2, "account_cash", 300);
    assert_balance(&at_tid2, "account_revenue", -300);
}

/// Test #6: list_derived_at at the latest transition equals
/// list_derived against the current state. Behavioural equivalence
/// guarantee.
#[tokio::test]
async fn list_derived_at_at_latest_equals_list_derived() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let trial_balance = double_entry_ledger::trial_balance_row();
    let at_tid3 = list_derived_at(&pool, &trial_balance, tid3).await.unwrap();
    let current = list_derived(&pool, &trial_balance).await.unwrap();

    assert_eq!(
        at_tid3, current,
        "list_derived_at at the latest transition must equal current list_derived"
    );
}

/// Test #7: list_derived_at ignores unrelated predicates during
/// replay (proves scoped reconstruction is correct under noise).
/// Commit an unrelated transformation via `propose_against_pg`
/// (which adds its own audit row), then confirm the trial balance
/// at that new transition is unchanged.
#[tokio::test]
async fn list_derived_at_ignores_unrelated_predicates_under_noise() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let trial_balance = double_entry_ledger::trial_balance_row();
    let baseline = list_derived_at(&pool, &trial_balance, tid3).await.unwrap();

    // Commit an unrelated transformation (verified_revenue's
    // admit_independent_verification). This adds an
    // IndependentlyVerifiedRevenue claim and a new audit row, none of
    // which the trial-balance derived references. Capture the new
    // tid; ask for the trial balance as of THIS tid.
    let invariants = verified_revenue::all_invariants();
    let new_tid = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &verified_revenue::admit_independent_verification(),
            vec![
                subj("noise_asset"),
                subj("p_noise"),
                dec(7),
                subj("noise_ver"),
            ],
            &invariants,
        )
        .await
        .unwrap(),
    );

    let after_noise = list_derived_at(&pool, &trial_balance, new_tid)
        .await
        .unwrap();
    assert_eq!(
        baseline, after_noise,
        "an unrelated transformation must not change the trial balance, \
         even when the as-of coordinate moves past it"
    );
}

/// Test #8: confirms the trial balance derived enumeration is
/// correct against historical state that ALSO contains predicates
/// the derived does not touch (JournalEntry, Supersedes). This is
/// the output-level check: `list_derived_at` produces the right
/// answer even when the audit log contains noise predicates.
///
/// Note: this test does NOT directly inspect what
/// `reconstruct_state_at_for_predicates` returns - that function is
/// `pub(crate)` and not reachable from integration tests. The
/// partial-state contract (only requested predicates in the
/// reconstructed state) is enforced internally by the
/// `predicate_in_scope_set` check in the replay loop and validated
/// here only indirectly via correct output. A regression that
/// accidentally loaded everything would still produce correct
/// output for the trial balance (the JournalLines would still be
/// there); the test that catches such a regression is the bench's
/// list_scoped phase timing.
#[tokio::test]
async fn list_derived_at_returns_correct_output_under_mixed_predicate_history() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_tid1, _tid2, tid3) = three_step_ledger(&pool).await;

    let full = reconstruct_state_at(&pool, tid3).await.unwrap();
    // Full state at tid3: JournalEntry, JournalLine, and Supersedes
    // (the restatement adds a Supersedes claim). All three predicates
    // should be present.
    let predicates: std::collections::HashSet<&str> =
        full.claims().iter().map(|c| c.predicate.as_str()).collect();
    assert!(predicates.contains("JournalEntry"));
    assert!(predicates.contains("JournalLine"));
    assert!(predicates.contains("Supersedes"));

    // Now ask for the trial balance at tid3 via list_derived_at,
    // which internally calls reconstruct_state_at_for_predicates
    // with footprint = {"JournalLine"}. The output is the trial
    // balance rows, which require only JournalLine to compute
    // correctly. If the scoped reconstruction were broken (e.g.,
    // accidentally loaded everything, or accidentally skipped
    // JournalLines), the trial balance would be wrong.
    let trial_balance = double_entry_ledger::trial_balance_row();
    let rows = list_derived_at(&pool, &trial_balance, tid3).await.unwrap();
    // Current state: entry_001 (100) + entry_002 (200) + entry_001_v2 (150) = 450 on cash.
    assert_balance(&rows, "account_cash", 450);
    assert_balance(&rows, "account_revenue", -450);
}

/// Test #9: cross-transition retraction.
///
/// The previous eight tests exercise additive workflows only
/// (`post_simple_entry`, `restate_entry`) - neither retracts
/// anything, so the replay loop's `claims.retain(|c| c != r)`
/// branch is never tickled by realistic data. This test uses
/// `verified_revenue::correct_independent_verification`, which
/// **retracts** the `CurrentVerification` pointer as part of its
/// body. The scenario:
///
/// 1. `admit_independent_verification` (IV1, asserts also
///    `CurrentVerification(asset, period, ver_001)`)   -> tid1
/// 2. `correct_independent_verification`               -> tid2 (asserts
///    new IV2 + `Supersedes`; **retracts** the `CurrentVerification`
///    that step 1 created and asserts a new one for ver_002)
///
/// As-of tid1: `CurrentVerification(_, _, ver_001)` IS present.
/// As-of tid2: `CurrentVerification(_, _, ver_001)` is GONE;
///             `CurrentVerification(_, _, ver_002)` is present.
///
/// A regression that broke the retraction branch of the replay loop
/// (e.g. ignored retractions, or applied them after assertions
/// rather than before, or scoped them out under
/// `reconstruct_state_at_for_predicates`) would make this test
/// fail.
#[tokio::test]
async fn reconstruct_state_at_applies_cross_transition_retractions() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = verified_revenue::all_invariants();
    let asset = subj("asset_a");
    let period = subj("p_2026_04");

    // Step 1: admit IV at 92. Asserts IV + CurrentVerification(ver_001).
    let tid1 = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &verified_revenue::admit_independent_verification(),
            vec![asset.clone(), period.clone(), dec(92), subj("ver_001")],
            &invariants,
        )
        .await
        .unwrap(),
    );

    // Step 2: correct the verification to 91 (ver_002). This
    // transformation retracts CurrentVerification(ver_001) and
    // asserts CurrentVerification(ver_002) plus Supersedes.
    let tid2 = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &verified_revenue::correct_independent_verification(),
            vec![asset, period, dec(91), subj("ver_002"), subj("ver_001")],
            &invariants,
        )
        .await
        .unwrap(),
    );

    // At tid1: CurrentVerification(_, _, ver_001) IS present.
    let claims_at_tid1 = list_claims_at(&pool, tid1).await.unwrap();
    let pointer_at_tid1 = claims_at_tid1
        .iter()
        .filter(|c| c.predicate == "CurrentVerification" && c.args[2] == subj("ver_001"))
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
        .filter(|c| c.predicate == "CurrentVerification" && c.args[2] == subj("ver_001"))
        .count();
    let new_pointer_at_tid2 = claims_at_tid2
        .iter()
        .filter(|c| c.predicate == "CurrentVerification" && c.args[2] == subj("ver_002"))
        .count();
    assert_eq!(
        stale_pointer_at_tid2, 0,
        "CurrentVerification(ver_001) should be retracted as of tid2"
    );
    assert_eq!(
        new_pointer_at_tid2, 1,
        "CurrentVerification(ver_002) should be present as of tid2"
    );

    // The historical IV1 must survive the correction (history is
    // append-only); both verifications should be in admitted state
    // at tid2.
    let iv_at_tid2 = claims_at_tid2
        .iter()
        .filter(|c| c.predicate == "IndependentlyVerifiedRevenue")
        .count();
    assert_eq!(
        iv_at_tid2, 2,
        "both IV1 (original 92) and IV2 (corrected 91) should be admitted at tid2"
    );
}

/// Test #10: empty audit log.
///
/// `reconstruct_state_at(pool, any_uuid)` against a database whose
/// audit table is empty must return `TransitionNotFound`, not
/// `Ok(empty State)`. Pins the "as of *this actual committed
/// transition*" contract at its edge: even when there are no
/// committed transitions at all, an unknown id is still an error.
/// A regression that returned an empty state for "the audit table
/// has nothing matching" would silently succeed with the wrong
/// semantics.
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
