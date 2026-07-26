//! Integration tests for the outbox delivery-state helpers
//! (doctrine in `docs/outbox-sketch.md`).
//!
//! Covered in this file: `mark_outbox_delivered`,
//! `mark_outbox_transient_attempt`, `mark_outbox_failed`,
//! `record_compensation`. The lease-management helpers
//! (`claim_pending_outbox_row`, `release_outbox_claim`) live in a
//! sibling file (`outbox_lease.rs`) so each file stays focused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    OutboxUpdate, PgError, PgPool, PgProposalOutcome, list_pending_outbox, mark_outbox_delivered,
    mark_outbox_failed, mark_outbox_transient_attempt, record_compensation,
};
use uuid::Uuid;

mod common;
use common::{dec, subj};
use common::{reset_db, test_pool};

// ============================================================
// Test infrastructure
// ============================================================

/// Commit one ledger entry and return the resulting outbox row's
/// `intent_id`. The row is in `status='pending'` with no lease
/// held.
async fn enqueue_one_pending(pool: &PgPool) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_05_17"),
            subj("p_helpers"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
    )
    .await
    .unwrap();
    let _tid = match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => panic!("setup rejected: {reason}"),
    };
    list_pending_outbox(pool).await.unwrap()[0].intent_id
}

/// Take the lease on a pending row directly (the helper that does
/// this lives in `outbox_lease.rs` and is the subject of a sibling
/// test file; here we just need the lease in place so we can
/// exercise the mark_* helpers, so we set the columns ourselves).
async fn force_lease(pool: &PgPool, intent_id: Uuid, worker_id: &str, lease_secs: i64) {
    sqlx::query(
        "UPDATE morpholog.outbox
         SET status='in_progress',
             locked_by=$2,
             lock_expires_at=now()+($3 || ' seconds')::interval
         WHERE intent_id=$1",
    )
    .bind(intent_id)
    .bind(worker_id)
    .bind(lease_secs.to_string())
    .execute(pool)
    .await
    .unwrap();
}

/// Fetch a single outbox row by intent_id for assertions, regardless
/// of status. (The public `list_pending_outbox` filters to pending
/// only.)
async fn fetch_row(
    pool: &PgPool,
    intent_id: Uuid,
) -> (
    String,                // status
    i32,                   // attempt_count
    Option<DateTime<Utc>>, // delivered_at
    Option<DateTime<Utc>>, // failed_at
    Option<String>,        // failure_reason
    Option<DateTime<Utc>>, // next_attempt_at
    Option<Uuid>,          // compensation_transition_id
    Option<String>,        // locked_by
    Option<DateTime<Utc>>, // lock_expires_at
) {
    sqlx::query_as(
        "SELECT status, attempt_count, delivered_at, failed_at, failure_reason,
                next_attempt_at, compensation_transition_id, locked_by,
                lock_expires_at
         FROM morpholog.outbox WHERE intent_id=$1",
    )
    .bind(intent_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn mark_outbox_delivered_sets_status_and_clears_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    force_lease(&pool, intent_id, "worker_a", 30).await;

    let result = mark_outbox_delivered(&pool, intent_id, "worker_a")
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::Applied);

    let row = fetch_row(&pool, intent_id).await;
    assert_eq!(row.0, "delivered", "status -> delivered");
    assert_eq!(row.1, 1, "attempt_count incremented");
    assert!(row.2.is_some(), "delivered_at must be set");
    assert!(row.7.is_none(), "locked_by must be cleared");
    assert!(row.8.is_none(), "lock_expires_at must be cleared");
}

#[tokio::test]
async fn mark_outbox_delivered_returns_lease_lost_when_worker_does_not_hold_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    force_lease(&pool, intent_id, "worker_a", 30).await;

    // Different worker tries to mark delivered.
    let result = mark_outbox_delivered(&pool, intent_id, "worker_b_who_does_not_hold_the_lease")
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::LeaseLost);

    // Row is unchanged: still in_progress, still locked by worker_a.
    let row = fetch_row(&pool, intent_id).await;
    assert_eq!(row.0, "in_progress");
    assert_eq!(row.7, Some("worker_a".to_string()));
}

#[tokio::test]
async fn mark_outbox_transient_attempt_schedules_retry_and_releases_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    force_lease(&pool, intent_id, "worker_a", 30).await;

    let next = Utc::now() + ChronoDuration::seconds(60);
    let result = mark_outbox_transient_attempt(&pool, intent_id, "worker_a", next)
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::Applied);

    let row = fetch_row(&pool, intent_id).await;
    assert_eq!(row.0, "pending", "row returns to pending for retry");
    assert_eq!(row.1, 1, "attempt_count incremented");
    assert!(row.5.is_some(), "next_attempt_at must be set");
    assert!(
        (row.5.unwrap() - next).num_seconds().abs() < 2,
        "next_attempt_at must roughly match the requested instant"
    );
    assert!(row.7.is_none(), "lease released");
}

#[tokio::test]
async fn mark_outbox_transient_attempt_accepts_past_next_attempt_at() {
    // The helper does NOT validate that next_attempt_at is in the
    // future. Loop-safety against same-pass reclaim lives at
    // `claim_pending_outbox_row`'s pass boundary; adding a
    // validation here would conflict with the lease-loss-as-signal
    // contract and would spuriously fail a slow legitimate
    // delivery whose retry instant elapsed during transit.
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    force_lease(&pool, intent_id, "worker_a", 30).await;

    let past = Utc::now() - ChronoDuration::seconds(5);
    let result = mark_outbox_transient_attempt(&pool, intent_id, "worker_a", past)
        .await
        .expect("past next_attempt_at must be accepted");
    assert_eq!(result, OutboxUpdate::Applied);

    let row = fetch_row(&pool, intent_id).await;
    assert_eq!(row.0, "pending", "row returns to pending");
    assert!(row.5.is_some(), "next_attempt_at was written");
    assert!(row.7.is_none(), "lease released");
}

#[tokio::test]
async fn mark_outbox_failed_captures_reason_and_releases_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    force_lease(&pool, intent_id, "worker_a", 30).await;

    let reason = "counterparty bank rejected wire: AML routing lock";
    let result = mark_outbox_failed(&pool, intent_id, "worker_a", reason)
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::Applied);

    let row = fetch_row(&pool, intent_id).await;
    assert_eq!(row.0, "failed");
    assert_eq!(row.1, 1);
    assert!(row.3.is_some(), "failed_at must be set");
    assert_eq!(row.4, Some(reason.to_string()));
    assert!(row.7.is_none(), "lease released");
}

#[tokio::test]
async fn record_compensation_links_compensation_to_failed_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    force_lease(&pool, intent_id, "worker_a", 30).await;
    let _ = mark_outbox_failed(&pool, intent_id, "worker_a", "test")
        .await
        .unwrap();

    // Run a real second transformation through the kernel to get a
    // genuine transition_id we can attach as the compensation linkage.
    // Using post_simple_entry with debit/credit swapped, mirroring the
    // outbox-spike compensation shape.
    let compensation_outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001_reversal"),
            subj("d_2026_05_17"),
            subj("p_helpers"),
            subj("account_revenue"),
            subj("account_cash"),
            dec(100),
        ],
    )
    .await
    .unwrap();
    let compensation_tid = match compensation_outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => panic!("compensation rejected: {reason}"),
    };

    record_compensation(&pool, intent_id, compensation_tid)
        .await
        .unwrap();

    let row = fetch_row(&pool, intent_id).await;
    assert_eq!(row.6, Some(compensation_tid));
}

#[tokio::test]
async fn record_compensation_errors_when_row_is_not_failed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    // Row is still 'pending' - no failure, no compensation should
    // be allowed.

    let bogus_tid = Uuid::nil();
    let err = record_compensation(&pool, intent_id, bogus_tid)
        .await
        .expect_err("must error against pending row");
    match err {
        PgError::InvalidState(msg) => {
            assert!(msg.contains("status='failed'"));
        }
        other => panic!("expected InvalidState, got {other:?}"),
    }
}

#[tokio::test]
async fn record_compensation_errors_on_double_record() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_one_pending(&pool).await;
    force_lease(&pool, intent_id, "worker_a", 30).await;
    let _ = mark_outbox_failed(&pool, intent_id, "worker_a", "test")
        .await
        .unwrap();

    // First compensation linkage: insert a real audit row to satisfy
    // the FK.
    let compensation_outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001_reversal"),
            subj("d_2026_05_17"),
            subj("p_helpers"),
            subj("account_revenue"),
            subj("account_cash"),
            dec(100),
        ],
    )
    .await
    .unwrap();
    let PgProposalOutcome::Committed {
        transition_id: comp_tid_a,
        ..
    } = compensation_outcome
    else {
        unreachable!()
    };

    record_compensation(&pool, intent_id, comp_tid_a)
        .await
        .unwrap();

    // Attempting to record a second compensation against the same
    // outbox row is a programming bug and must error rather than
    // silently overwrite.
    let comp_outcome_b = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001_reversal_b"),
            subj("d_2026_05_17"),
            subj("p_helpers"),
            subj("account_revenue"),
            subj("account_cash"),
            dec(50),
        ],
    )
    .await
    .unwrap();
    let PgProposalOutcome::Committed {
        transition_id: comp_tid_b,
        ..
    } = comp_outcome_b
    else {
        unreachable!()
    };

    let err = record_compensation(&pool, intent_id, comp_tid_b)
        .await
        .expect_err("must error on double-record");
    assert!(matches!(err, PgError::InvalidState(_)));

    // Row still carries the first compensation; the second was
    // rejected.
    let row = fetch_row(&pool, intent_id).await;
    assert_eq!(row.6, Some(comp_tid_a));
}

#[tokio::test]
async fn record_compensation_errors_when_intent_does_not_exist() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // No outbox row at all; just try to record compensation against
    // a random UUID. The helper should surface the not-found case
    // explicitly rather than blaming status only.
    // Any UUID that is not in the outbox suffices; nil is convenient
    // and matches the no-FK-needed pattern used elsewhere in this file.
    let missing_intent_id = Uuid::nil();
    let any_transition_id = Uuid::nil();
    let err = record_compensation(&pool, missing_intent_id, any_transition_id)
        .await
        .expect_err("must error when intent_id does not exist");
    match err {
        PgError::InvalidState(msg) => {
            assert!(
                msg.contains("not found"),
                "error message should name the not-found possibility, got: {msg}"
            );
        }
        other => panic!("expected InvalidState, got {other:?}"),
    }
}
