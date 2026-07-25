//! Integration tests for the outbox lease helpers
//! (`claim_pending_outbox_row`, `release_outbox_claim`; doctrine in
//! `docs/outbox-sketch.md`).
//!
//! The state-mutating helpers (`mark_outbox_delivered`,
//! `mark_outbox_transient_attempt`, `mark_outbox_failed`,
//! `record_compensation`) are exercised in the sibling file
//! `outbox_helpers.rs` so each file stays focused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    OutboxUpdate, PgError, PgPool, PgProposalOutcome, claim_pending_outbox_row,
    release_outbox_claim,
};
use uuid::Uuid;

mod common;
use common::{dec, subj};
use common::{reset_db, test_pool};

// ============================================================
// Test infrastructure
// ============================================================

/// Commit one ledger entry with the supplied `entry_id` and return
/// the resulting outbox row's `intent_id`. The row is in
/// `status='pending'`, no lease held, no `next_attempt_at`.
async fn enqueue_pending(pool: &PgPool, entry_id: &str) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj(entry_id),
            subj("d_2026_05_17"),
            subj("p_lease"),
            subj(&format!("account_cash_{entry_id}")),
            subj(&format!("account_revenue_{entry_id}")),
            dec(100),
        ],
    )
    .await
    .unwrap();
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => panic!("setup rejected: {reason}"),
    };
    // post_simple_entry emits one intent per call, so its intent_id
    // is the latest pending row for this transition.
    let (intent_id,): (Uuid,) = sqlx::query_as(
        "SELECT intent_id FROM morpholog.outbox
         ORDER BY enqueued_at DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    intent_id
}

/// Directly set the next_attempt_at on a pending row (used to
/// simulate a row whose backoff has not yet elapsed).
async fn set_next_attempt_at(pool: &PgPool, intent_id: Uuid, when: DateTime<Utc>) {
    sqlx::query("UPDATE morpholog.outbox SET next_attempt_at=$2 WHERE intent_id=$1")
        .bind(intent_id)
        .bind(when)
        .execute(pool)
        .await
        .unwrap();
}

/// Directly force a row into in_progress with an already-expired
/// lease (used to simulate a worker that crashed mid-delivery).
async fn force_expired_lease(pool: &PgPool, intent_id: Uuid, worker_id: &str) {
    sqlx::query(
        "UPDATE morpholog.outbox
         SET status='in_progress',
             locked_by=$2,
             lock_expires_at=now() - interval '1 second'
         WHERE intent_id=$1",
    )
    .bind(intent_id)
    .bind(worker_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn row_status_and_lease(pool: &PgPool, intent_id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, locked_by FROM morpholog.outbox WHERE intent_id=$1")
        .bind(intent_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

const INTENT_TYPE: &str = "JournalEntryPosted";
const LEASE: Duration = Duration::from_secs(30);

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn claim_returns_first_pending_row_and_sets_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let enqueued = enqueue_pending(&pool, "entry_001").await;

    let claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap()
        .expect("must return Some when a pending row is available");

    assert_eq!(claimed.intent_id, enqueued);
    assert_eq!(claimed.status, "in_progress");
    assert_eq!(claimed.locked_by, Some("worker_a".to_string()));
    let lease_until = claimed
        .lock_expires_at
        .expect("lock_expires_at must be populated");
    assert!(
        lease_until > Utc::now(),
        "lease must be in the future, got {lease_until}"
    );
    assert!(
        lease_until - Utc::now() <= ChronoDuration::seconds(31),
        "lease must be roughly the requested duration, got {} seconds",
        (lease_until - Utc::now()).num_seconds()
    );
}

#[tokio::test]
async fn claim_returns_none_when_no_pending_row_exists() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "must return None when the outbox is empty"
    );
}

#[tokio::test]
async fn claim_respects_intent_type_filter() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _enqueued = enqueue_pending(&pool, "entry_001").await;

    // The pending row's intent_type is JournalEntryPosted; a worker
    // dedicated to a different intent_type must not pick it up.
    let claimed = claim_pending_outbox_row(
        &pool,
        "worker_wire",
        "WireTransferRequested",
        LEASE,
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(
        claimed.is_none(),
        "worker filtering on a different intent_type must not claim the row"
    );
}

#[tokio::test]
async fn claim_returns_oldest_pending_row_first() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let first = enqueue_pending(&pool, "entry_001").await;
    // Insert a small wall-clock gap so the second row's
    // enqueued_at is provably later.
    tokio::time::sleep(Duration::from_millis(15)).await;
    let _second = enqueue_pending(&pool, "entry_002").await;

    let claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap()
        .expect("must return the oldest pending row");
    assert_eq!(
        claimed.intent_id, first,
        "must claim the earlier-enqueued row first"
    );
}

#[tokio::test]
async fn claim_skips_row_with_future_next_attempt_at() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_pending(&pool, "entry_001").await;
    // Simulate a row that was tried, failed transiently, and
    // scheduled to retry well in the future.
    set_next_attempt_at(&pool, intent_id, Utc::now() + ChronoDuration::hours(1)).await;

    let claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "row whose retry is scheduled in the future must not be claimable yet"
    );
}

#[tokio::test]
async fn claim_reclaims_row_whose_lease_has_expired() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_pending(&pool, "entry_001").await;
    force_expired_lease(&pool, intent_id, "worker_crashed").await;

    let claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap()
        .expect("expired-lease row must be reclaimable");
    assert_eq!(claimed.intent_id, intent_id);
    assert_eq!(claimed.locked_by, Some("worker_a".to_string()));
    assert_eq!(
        claimed.status, "in_progress",
        "row remains in_progress; lease just moved to the new worker"
    );
    let lease_until = claimed.lock_expires_at.unwrap();
    assert!(
        lease_until > Utc::now(),
        "reclaim must set a fresh future lease, not preserve the expired one"
    );
}

#[tokio::test]
async fn release_returns_row_to_pending_and_clears_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_pending(&pool, "entry_001").await;
    let _claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap()
        .expect("claim must succeed");

    let result = release_outbox_claim(&pool, intent_id, "worker_a")
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::Applied);

    let (status, locked_by) = row_status_and_lease(&pool, intent_id).await;
    assert_eq!(status, "pending", "row must return to pending");
    assert!(locked_by.is_none(), "lease must be cleared");

    // And the row is once again claimable by a different worker.
    let reclaimed = claim_pending_outbox_row(&pool, "worker_b", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap()
        .expect("released row must be re-claimable");
    assert_eq!(reclaimed.intent_id, intent_id);
    assert_eq!(reclaimed.locked_by, Some("worker_b".to_string()));
}

#[tokio::test]
async fn claim_rejects_sub_second_lease_duration() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // A pending row is irrelevant - the validation happens before
    // any SQL is issued.
    let _ = enqueue_pending(&pool, "entry_001").await;

    // A zero-duration lease would expire before the worker could
    // ever call mark_*, leaving the row effectively un-updatable.
    let zero = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, Duration::ZERO, Utc::now())
        .await
        .expect_err("zero-second lease must be rejected explicitly");
    assert!(matches!(zero, PgError::InvalidState(_)));

    // Sub-second durations get truncated by as_secs() to 0 and are
    // also rejected.
    let half = claim_pending_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        Duration::from_millis(500),
        Utc::now(),
    )
    .await
    .expect_err("sub-second lease must be rejected explicitly");
    assert!(matches!(half, PgError::InvalidState(_)));
}

#[tokio::test]
async fn release_returns_lease_lost_when_worker_does_not_hold_lease() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_pending(&pool, "entry_001").await;
    let _claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, Utc::now())
        .await
        .unwrap()
        .expect("claim must succeed");

    let result = release_outbox_claim(&pool, intent_id, "worker_b_imposter")
        .await
        .unwrap();
    assert_eq!(result, OutboxUpdate::LeaseLost);

    // Row remains in_progress under worker_a's lease.
    let (status, locked_by) = row_status_and_lease(&pool, intent_id).await;
    assert_eq!(status, "in_progress");
    assert_eq!(locked_by, Some("worker_a".to_string()));
}

#[tokio::test]
async fn claim_before_excludes_rows_scheduled_after_the_boundary() {
    // The pass-boundary variant treats next_attempt_at <= claim_before
    // as eligible, but anything scheduled later is invisible -
    // even if wall-clock time has moved past the row's
    // next_attempt_at by the time of the query. This is the loop
    // safety the drain relies on against deliverers that schedule
    // sub-second retries.
    let pool = test_pool().await;
    reset_db(&pool).await;
    let intent_id = enqueue_pending(&pool, "entry_001").await;

    // Schedule the row for "now plus a tiny window".
    let scheduled = Utc::now() + ChronoDuration::milliseconds(50);
    sqlx::query("UPDATE morpholog.outbox SET next_attempt_at = $1 WHERE intent_id = $2")
        .bind(scheduled)
        .bind(intent_id)
        .execute(&pool)
        .await
        .unwrap();

    // Pretend the pass started just before that schedule. The
    // row's next_attempt_at > claim_before, so the claim must
    // return None even though wall-clock has presumably moved
    // forward by the time the query runs.
    let pass_start = scheduled - ChronoDuration::milliseconds(1);
    let claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, pass_start)
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "row scheduled after pass_start must not be claimable in this pass; got {claimed:?}"
    );

    // With a later pass_start (after the schedule), the same row
    // is claimable.
    let claimed = claim_pending_outbox_row(&pool, "worker_a", INTENT_TYPE, LEASE, scheduled)
        .await
        .unwrap()
        .expect("row scheduled at-or-before pass_start must be claimable");
    assert_eq!(claimed.intent_id, intent_id);
}
