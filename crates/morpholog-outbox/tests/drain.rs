//! Integration tests for `process_available_outbox_rows`, apart from any scheduling:
//! - an empty outbox returns an empty Vec;
//! - all due rows drain in one pass;
//! - rows not yet due are not picked up;
//! - outcomes like `LeaseLost` do not stop the drain.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{commit_simple_entry, reset_db, test_pool};

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use morpholog_outbox::process_available_outbox_rows;
use morpholog_postgres::{
    Deliverer, DeliveryOutcome, OutboxRow, PgPool, ProcessOutcome,
    testing::{AlwaysDelivers, AlwaysTransient},
};

// ============================================================
// Test infrastructure
// ============================================================

const INTENT_TYPE: &str = "JournalEntryPosted";
const LEASE: Duration = Duration::from_secs(30);

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn drain_returns_empty_vec_when_no_rows_available() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let outcomes =
        process_available_outbox_rows(&pool, "worker_a", INTENT_TYPE, LEASE, &AlwaysDelivers, None)
            .await
            .unwrap();
    assert!(outcomes.is_empty());
}

#[tokio::test]
async fn drain_processes_all_available_rows_in_one_pass() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_simple_entry(&pool, "entry_001", "p_drain").await;
    let _ = commit_simple_entry(&pool, "entry_002", "p_drain").await;
    let _ = commit_simple_entry(&pool, "entry_003", "p_drain").await;

    let outcomes =
        process_available_outbox_rows(&pool, "worker_a", INTENT_TYPE, LEASE, &AlwaysDelivers, None)
            .await
            .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, ProcessOutcome::Delivered { .. })),
        "every outcome should be Delivered, got {outcomes:?}"
    );

    // Outbox now has no pending rows.
    let pending_count: (i64,) =
        sqlx::query_as("SELECT count(*) FROM morpholog.outbox WHERE status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending_count.0, 0);
}

#[tokio::test]
async fn drain_does_not_redeliver_transient_row_in_same_pass() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_simple_entry(&pool, "entry_001", "p_drain").await;
    let _ = commit_simple_entry(&pool, "entry_002", "p_drain").await;
    // Each retry is scheduled an hour out, so the drain must stop after one attempt per row.
    let later = Timestamp::now() + SignedDuration::from_hours(1);

    let outcomes = process_available_outbox_rows(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysTransient {
            next_attempt_at: later,
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        outcomes.len(),
        2,
        "each row should produce one TransientRetry; drain stops because no row is due"
    );
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, ProcessOutcome::TransientRetry { .. })),
        "every outcome should be TransientRetry, got {outcomes:?}"
    );
}

#[tokio::test]
async fn drain_pass_boundary_blocks_subsecond_retries_until_next_pass() {
    // Each retry is due 1ms later, which has passed by the next claim. Only the pass-start
    // cutoff stops the drain from re-claiming the same rows forever.
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_simple_entry(&pool, "entry_001", "p_drain").await;
    let _ = commit_simple_entry(&pool, "entry_002", "p_drain").await;

    struct SubsecondTransient;
    impl Deliverer for SubsecondTransient {
        async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
            DeliveryOutcome::Transient {
                next_attempt_at: Timestamp::now() + SignedDuration::from_millis(1),
            }
        }
    }

    let outcomes = process_available_outbox_rows(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &SubsecondTransient,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        outcomes.len(),
        2,
        "drain must process each row exactly once per pass, even when \
         next_attempt_at is sub-second; got {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, ProcessOutcome::TransientRetry { .. })),
        "every outcome should be TransientRetry, got {outcomes:?}"
    );
}

#[tokio::test]
async fn drain_continues_through_lease_lost_outcomes() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_simple_entry(&pool, "entry_lost", "p_drain").await;
    let _ = commit_simple_entry(&pool, "entry_ok", "p_drain").await;
    let pool_for_deliverer = pool.clone();

    /// The first call expires its own lease before returning Delivered; later calls deliver
    /// normally.
    struct ExpireFirstThenDeliver {
        pool: PgPool,
        call_count: std::sync::atomic::AtomicU32,
    }
    impl Deliverer for ExpireFirstThenDeliver {
        async fn deliver(&self, row: &OutboxRow) -> DeliveryOutcome {
            let prior = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if prior == 0 {
                sqlx::query(
                    "UPDATE morpholog.outbox
                     SET lock_expires_at = now() - interval '1 second'
                     WHERE intent_id=$1",
                )
                .bind(row.intent_id)
                .execute(&self.pool)
                .await
                .unwrap();
            }
            DeliveryOutcome::Delivered
        }
    }

    let outcomes = process_available_outbox_rows(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &ExpireFirstThenDeliver {
            pool: pool_for_deliverer,
            call_count: 0.into(),
        },
        None,
    )
    .await
    .unwrap();
    // LeaseLost for the sabotaged claim, then Delivered for that row reclaimed, then Delivered
    // for the other row.
    assert_eq!(
        outcomes.len(),
        3,
        "drain must continue through LeaseLost; got {outcomes:?}"
    );
    assert!(
        matches!(outcomes[0], ProcessOutcome::LeaseLost { .. }),
        "first outcome was the sabotaged claim, got {:?}",
        outcomes[0]
    );
    assert!(
        outcomes[1..]
            .iter()
            .all(|o| matches!(o, ProcessOutcome::Delivered { .. })),
        "remaining outcomes should all be Delivered, got {outcomes:?}"
    );
}
