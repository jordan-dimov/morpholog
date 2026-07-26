//! Integration tests for the compensation lease helpers
//! (`begin_compensation`, `complete_compensation`,
//! `mark_compensation_failed`).
//!
//! These helpers close the compensation-idempotency gap the bare
//! delivery-state helpers leave open: the lease pattern
//! ensures at most one worker holds the right to invoke a
//! compensating transformation for a given failed outbox row.
//!
//! Delivery-state mutators (`mark_outbox_delivered`,
//! `mark_outbox_transient_attempt`, `mark_outbox_failed`,
//! `record_compensation`) live in `outbox_helpers.rs`; claim/release
//! lease helpers for the delivery path live in `outbox_lease.rs`.
//! Each file stays focused on one helper family.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use chrono::{DateTime, Utc};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    OutboxUpdate, PgError, PgPool, PgProposalOutcome, begin_compensation, complete_compensation,
    mark_compensation_failed, mark_outbox_failed,
};
use uuid::Uuid;

mod common;
use common::{dec, subj};
use common::{reset_db, test_pool};

// ============================================================
// Test infrastructure
// ============================================================

/// Commit one ledger entry with the supplied `entry_id` and return
/// the resulting outbox row's `intent_id`.
async fn enqueue_pending(pool: &PgPool, entry_id: &str) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj(entry_id),
            subj("d_2026_05_17"),
            subj("p_compensation"),
            subj(&format!("cash_{entry_id}")),
            subj(&format!("revenue_{entry_id}")),
            dec(100),
        ],
    )
    .await
    .unwrap();
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => panic!("setup rejected: {reason}"),
    };
    let (intent_id,): (Uuid,) = sqlx::query_as(
        "SELECT intent_id FROM morpholog.outbox
         ORDER BY enqueued_at DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    intent_id
}

/// Drive a row through the path that precedes a compensation claim:
/// commit, force the in_progress lease, mark failed. Returns the
/// intent_id. After this the row is in `status='failed'` with no
/// lease held and `compensation_transition_id IS NULL`.
async fn enqueue_then_fail(pool: &PgPool, entry_id: &str) -> Uuid {
    let intent_id = enqueue_pending(pool, entry_id).await;
    // Take the delivery lease directly (simulating claim_pending_outbox_row).
    sqlx::query(
        "UPDATE morpholog.outbox
         SET status='in_progress',
             locked_by='delivery_worker',
             lock_expires_at=now()+interval '30 seconds'
         WHERE intent_id=$1",
    )
    .bind(intent_id)
    .execute(pool)
    .await
    .unwrap();
    let _ = mark_outbox_failed(pool, intent_id, "delivery_worker", "test failure")
        .await
        .unwrap();
    intent_id
}

/// Commit a real compensating transformation against the dev DB so
/// that `compensation_transition_id` references a row that actually
/// exists in `morpholog.audit`. The FK constraint would reject a
/// synthesized UUID.
async fn commit_compensation_transformation(pool: &PgPool, suffix: &str) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj(&format!("compensation_{suffix}")),
            subj("d_2026_05_17"),
            subj("p_compensation"),
            // Debit and credit swapped vs the original so the reversal
            // also passes balanced_posted_entry.
            subj(&format!("revenue_{suffix}_target")),
            subj(&format!("cash_{suffix}_target")),
            dec(100),
        ],
    )
    .await
    .unwrap();
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => panic!("compensation rejected: {reason}"),
    }
}

async fn fetch_status_and_compensation(
    pool: &PgPool,
    intent_id: Uuid,
) -> (
    String,                // status
    Option<String>,        // locked_by
    Option<DateTime<Utc>>, // lock_expires_at
    Option<Uuid>,          // compensation_transition_id
    Option<String>,        // failure_reason
) {
    sqlx::query_as(
        "SELECT status, locked_by, lock_expires_at, compensation_transition_id, failure_reason
         FROM morpholog.outbox WHERE intent_id=$1",
    )
    .bind(intent_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

const LEASE: Duration = Duration::from_secs(30);

// ============================================================
// begin_compensation
// ============================================================

#[tokio::test]
async fn begin_compensation_claims_failed_row_and_sets_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;

    let claimed = begin_compensation(&pool, intent_id, "comp_worker", LEASE)
        .await
        .unwrap()
        .expect("must return Some when a failed uncompensated row exists");
    assert_eq!(claimed.intent_id, intent_id);
    assert_eq!(claimed.status, "compensation_in_progress");
    assert_eq!(claimed.locked_by, Some("comp_worker".to_string()));
    assert!(claimed.lock_expires_at.is_some());
}

#[tokio::test]
async fn begin_compensation_returns_none_when_row_does_not_exist() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let claimed = begin_compensation(&pool, Uuid::nil(), "comp_worker", LEASE)
        .await
        .unwrap();
    assert!(claimed.is_none());
}

#[tokio::test]
async fn begin_compensation_returns_none_when_row_is_still_pending() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // Row stays in `pending` state; compensation must not be
    // claimable - delivery hasn't even been attempted.
    let intent_id = enqueue_pending(&pool, "entry_001").await;

    let claimed = begin_compensation(&pool, intent_id, "comp_worker", LEASE)
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "pending row must not be eligible for compensation claim"
    );
}

#[tokio::test]
async fn begin_compensation_returns_none_when_compensation_already_recorded() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;

    // First worker claims and completes compensation.
    let _ = begin_compensation(&pool, intent_id, "comp_worker_a", LEASE)
        .await
        .unwrap()
        .expect("first claim succeeds");
    let comp_tid = commit_compensation_transformation(&pool, "a").await;
    let _ = complete_compensation(&pool, intent_id, "comp_worker_a", comp_tid)
        .await
        .unwrap();

    // Second worker tries to claim - row is back to `failed` but
    // now carries a compensation_transition_id, so the WHERE filter
    // excludes it.
    let claimed = begin_compensation(&pool, intent_id, "comp_worker_b", LEASE)
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "row that already has a compensation_transition_id must not be re-claimable"
    );
}

#[tokio::test]
async fn begin_compensation_returns_none_when_already_in_compensation_in_progress() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;

    let _ = begin_compensation(&pool, intent_id, "comp_worker_a", LEASE)
        .await
        .unwrap()
        .expect("first claim succeeds");

    // Second worker tries to claim while worker_a still holds the
    // lease. The WHERE filter excludes anything not in `failed`,
    // and the row is now `compensation_in_progress`. Even if the
    // SKIP LOCKED didn't fire, the status filter alone rejects it.
    let claimed = begin_compensation(&pool, intent_id, "comp_worker_b", LEASE)
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "compensation_in_progress row must not be re-claimable mid-lease"
    );
}

#[tokio::test]
async fn begin_compensation_does_not_reclaim_expired_compensation_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;

    let _ = begin_compensation(&pool, intent_id, "comp_worker_a", LEASE)
        .await
        .unwrap()
        .expect("first claim succeeds");

    // Force the lease to expired (simulating a crashed worker
    // mid-compensation). Unlike claim_pending_outbox_row, which
    // transparently reclaims expired in_progress leases,
    // begin_compensation must NOT transparently reclaim an expired
    // compensation_in_progress lease - doing so would risk duplicate
    // compensation if the previous worker had already committed
    // the compensating transformation. The row should stay stuck
    // and require operator intervention.
    sqlx::query(
        "UPDATE morpholog.outbox SET lock_expires_at = now() - interval '1 second'
         WHERE intent_id=$1",
    )
    .bind(intent_id)
    .execute(&pool)
    .await
    .unwrap();

    let claimed = begin_compensation(&pool, intent_id, "comp_worker_b", LEASE)
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "expired compensation_in_progress lease must NOT be transparently reclaimed; \
         the row stays stuck and requires operator intervention"
    );
}

#[tokio::test]
async fn begin_compensation_rejects_sub_second_lease_duration() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;

    let err = begin_compensation(&pool, intent_id, "comp_worker", Duration::ZERO)
        .await
        .expect_err("zero-second lease must be rejected explicitly");
    assert!(matches!(err, PgError::InvalidState(_)));
}

// ============================================================
// complete_compensation
// ============================================================

#[tokio::test]
async fn complete_compensation_sets_pointer_and_returns_row_to_failed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;
    let _ = begin_compensation(&pool, intent_id, "comp_worker", LEASE)
        .await
        .unwrap()
        .unwrap();
    let comp_tid = commit_compensation_transformation(&pool, "a").await;

    let result = complete_compensation(&pool, intent_id, "comp_worker", comp_tid)
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::Applied);

    let (status, locked_by, lock_expires_at, compensation_transition_id, _) =
        fetch_status_and_compensation(&pool, intent_id).await;
    assert_eq!(status, "failed");
    assert!(locked_by.is_none(), "lease released");
    assert!(lock_expires_at.is_none(), "lease released");
    assert_eq!(compensation_transition_id, Some(comp_tid));
}

#[tokio::test]
async fn complete_compensation_returns_lease_lost_when_worker_does_not_hold_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;
    let _ = begin_compensation(&pool, intent_id, "comp_worker_a", LEASE)
        .await
        .unwrap()
        .unwrap();
    let comp_tid = commit_compensation_transformation(&pool, "a").await;

    let result = complete_compensation(&pool, intent_id, "comp_worker_b_imposter", comp_tid)
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::LeaseLost);

    // Row stays in compensation_in_progress under worker_a's lease;
    // pointer is NOT set.
    let (status, locked_by, _, compensation_transition_id, _) =
        fetch_status_and_compensation(&pool, intent_id).await;
    assert_eq!(status, "compensation_in_progress");
    assert_eq!(locked_by, Some("comp_worker_a".to_string()));
    assert!(compensation_transition_id.is_none());
}

#[tokio::test]
async fn complete_compensation_returns_lease_lost_when_status_is_not_compensation_in_progress() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // Row is in `failed` (no compensation claim yet). complete_compensation
    // should error out because the status precondition is not met.
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;
    let comp_tid = commit_compensation_transformation(&pool, "a").await;

    let result = complete_compensation(&pool, intent_id, "comp_worker", comp_tid)
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::LeaseLost);
}

// ============================================================
// mark_compensation_failed
// ============================================================

#[tokio::test]
async fn mark_compensation_failed_sets_status_and_reason_releases_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;
    let _ = begin_compensation(&pool, intent_id, "comp_worker", LEASE)
        .await
        .unwrap()
        .unwrap();

    let reason = "compensation transformation rejected: balance violates ledger invariant";
    let result = mark_compensation_failed(&pool, intent_id, "comp_worker", reason)
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::Applied);

    let (status, locked_by, lock_expires_at, _, failure_reason) =
        fetch_status_and_compensation(&pool, intent_id).await;
    assert_eq!(status, "compensation_failed");
    assert!(locked_by.is_none());
    assert!(lock_expires_at.is_none());
    assert_eq!(failure_reason, Some(reason.to_string()));
}

#[tokio::test]
async fn mark_compensation_failed_returns_lease_lost_when_worker_does_not_hold_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_then_fail(&pool, "entry_001").await;
    let _ = begin_compensation(&pool, intent_id, "comp_worker_a", LEASE)
        .await
        .unwrap()
        .unwrap();

    let result =
        mark_compensation_failed(&pool, intent_id, "comp_worker_b_imposter", "no permission")
            .await
            .unwrap();
    assert_eq!(result, OutboxUpdate::LeaseLost);

    // Row stays in compensation_in_progress under worker_a's lease.
    let (status, locked_by, _, _, _) = fetch_status_and_compensation(&pool, intent_id).await;
    assert_eq!(status, "compensation_in_progress");
    assert_eq!(locked_by, Some("comp_worker_a".to_string()));
}
