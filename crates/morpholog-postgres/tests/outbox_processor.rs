//! End-to-end integration tests for the single-row processor
//! (`process_one_outbox_row`) added in PR 2 of the outbox arc.
//!
//! These tests drive a real outbox row through every branch of the
//! delivery-and-compensation state machine, against a real
//! PostgreSQL database, asserting that:
//! - happy-path delivery marks the row `delivered`;
//! - transient failures return the row to `pending` with a retry
//!   instant;
//! - non-retryable failures with no compensation spec mark the row
//!   `failed`;
//! - non-retryable failures with a compensation spec drive the
//!   row through the `failed` -> `compensation_in_progress` -> `failed`
//!   sequence with `compensation_transition_id` set;
//! - a compensating transformation that is rejected by an invariant
//!   leaves the row in `compensation_failed` (the genuinely-broken
//!   state).
//!
//! The processor is exercised against the `double_entry_ledger`
//! example: the original commit posts a balanced entry; the
//! compensation posts a reversal with debit and credit swapped, so
//! `balanced_posted_entry` continues to hold across the audit log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    CompensationSpec, Deliverer, DeliveryOutcome, OutboxRow, PgPool, PgProposalOutcome,
    ProcessOutcome, list_audit_rows, list_pending_outbox, process_one_outbox_row,
    testing::{AlwaysDelivers, AlwaysNonRetryable, AlwaysTransient},
};
use uuid::Uuid;

mod common;
use common::{dec, subj};

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



async fn commit_post_simple_entry(pool: &PgPool, entry_id: &str) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj(entry_id),
            subj("d_2026_05_17"),
            subj("p_processor"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .unwrap();
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => panic!("setup rejected: {reason}"),
    }
}

const INTENT_TYPE: &str = "JournalEntryPosted";
const LEASE: Duration = Duration::from_secs(30);

// ============================================================
// Deliverer stubs
// ============================================================
//
// AlwaysDelivers, AlwaysTransient, AlwaysNonRetryable live in
// `morpholog_postgres::testing` so this crate's tests and
// `morpholog-outbox`'s tests share the same stubs. Test-file-local
// shapes that need processor-side state (e.g. forcing lease
// expiry via direct SQL) stay here.

/// Deliverer that forces its own lease to expire (via direct SQL)
/// before returning the configured outcome. Used to exercise the
/// processor's LeaseLost branches deterministically: by the time
/// the mark_* helper runs, the row's lock_expires_at is in the
/// past, so the helper returns OutboxUpdate::LeaseLost.
struct ExpireLeaseThenReturn {
    pool: PgPool,
    outcome: DeliveryOutcome,
}
impl Deliverer for ExpireLeaseThenReturn {
    async fn deliver(&self, row: &OutboxRow) -> DeliveryOutcome {
        sqlx::query(
            "UPDATE morpholog.outbox SET lock_expires_at = now() - interval '1 second'
             WHERE intent_id=$1",
        )
        .bind(row.intent_id)
        .execute(&self.pool)
        .await
        .unwrap();
        self.outcome.clone()
    }
}

// ============================================================
// Compensation specs
// ============================================================

/// A compensating transformation that genuinely balances: it posts
/// a reversal with debit and credit accounts swapped. The args
/// closure ignores the failed row's contents and hardcodes the
/// reversal - the test setup is the only caller, and it knows the
/// exact original posting it committed.
fn balanced_reversal_spec(suffix: &str) -> CompensationSpec {
    let suffix = suffix.to_string();
    CompensationSpec {
        transformation: double_entry_ledger::post_simple_entry(),
        invariants: double_entry_ledger::all_invariants(),
        args_from_row: Box::new(move |_row: &OutboxRow| {
            vec![
                subj(&format!("reversal_{suffix}")),
                subj("d_2026_05_17"),
                subj("p_processor"),
                // Original posted cash debit / revenue credit; the
                // reversal swaps them so both entries balance under
                // balanced_posted_entry.
                subj("account_revenue"),
                subj("account_cash"),
                dec(100),
            ]
        }),
    }
}

/// A compensating transformation that is guaranteed to be rejected
/// by `balanced_posted_entry`: it uses `post_split_entry` with
/// mismatched debit and credit amounts. Demonstrates the
/// `compensation_failed` branch.
fn unbalanced_compensation_spec(suffix: &str) -> CompensationSpec {
    let suffix = suffix.to_string();
    CompensationSpec {
        transformation: double_entry_ledger::post_split_entry(),
        invariants: double_entry_ledger::all_invariants(),
        args_from_row: Box::new(move |_row: &OutboxRow| {
            // Debit 100, but two credits totalling only 95 -
            // balanced_posted_entry will reject.
            vec![
                subj(&format!("broken_reversal_{suffix}")),
                subj("d_2026_05_17"),
                subj("p_processor"),
                subj("account_revenue"),
                dec(100),
                subj("account_cash"),
                dec(50),
                subj("account_other"),
                dec(45),
            ]
        }),
    }
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn process_one_outbox_row_returns_no_row_available_when_empty() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysDelivers,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(outcome, ProcessOutcome::NoRowAvailable);
}

#[tokio::test]
async fn process_one_outbox_row_marks_delivered_on_success() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_post_simple_entry(&pool, "entry_001").await;

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysDelivers,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, ProcessOutcome::Delivered { .. }));

    // No pending rows left; outbox row is in `delivered`.
    assert!(list_pending_outbox(&pool).await.unwrap().is_empty());
    let (status,): (String,) = sqlx::query_as("SELECT status FROM morpholog.outbox LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "delivered");
}

#[tokio::test]
async fn process_one_outbox_row_schedules_retry_on_transient() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_post_simple_entry(&pool, "entry_001").await;
    let next = Utc::now() + ChronoDuration::seconds(120);

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysTransient {
            next_attempt_at: next,
        },
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    match outcome {
        ProcessOutcome::TransientRetry {
            next_attempt_at, ..
        } => {
            assert!(
                (next_attempt_at - next).num_seconds().abs() < 2,
                "next_attempt_at returned in ProcessOutcome must match what the deliverer requested"
            );
        }
        other => panic!("expected TransientRetry, got {other:?}"),
    }

    // The row is back to `pending` with next_attempt_at set.
    let (status, next_attempt_at): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, next_attempt_at FROM morpholog.outbox LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    assert!(next_attempt_at.is_some());
}

#[tokio::test]
async fn process_one_outbox_row_marks_failed_when_no_compensation_spec() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_post_simple_entry(&pool, "entry_001").await;

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysNonRetryable::new("no compensation wired"),
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    match outcome {
        ProcessOutcome::Failed { reason, .. } => {
            assert_eq!(reason, "no compensation wired");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let (status, failure_reason, compensation_transition_id): (
        String,
        Option<String>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT status, failure_reason, compensation_transition_id
         FROM morpholog.outbox LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(failure_reason, Some("no compensation wired".to_string()));
    assert!(compensation_transition_id.is_none());
}

#[tokio::test]
async fn process_one_outbox_row_compensates_on_nonretryable_with_spec() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let original_tid = commit_post_simple_entry(&pool, "entry_001").await;
    let spec = balanced_reversal_spec("entry_001");

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysNonRetryable::new("counterparty bank rejected wire: AML routing lock"),
        Some(&spec),
        Utc::now(),
    )
    .await
    .unwrap();
    let compensation_tid = match outcome {
        ProcessOutcome::Compensated {
            compensation_transition_id,
            ..
        } => compensation_transition_id,
        other => panic!("expected Compensated, got {other:?}"),
    };

    // The original outbox row is now `failed` with the
    // compensation pointer set.
    let (status, compensation_transition_id): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, compensation_transition_id FROM morpholog.outbox
         WHERE intent_type='JournalEntryPosted'
           AND transition_id=$1",
    )
    .bind(original_tid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(compensation_transition_id, Some(compensation_tid));

    // Audit log preserves the full lineage: original commit, then
    // the compensating transformation. This is the load-bearing
    // property of the compensation pattern.
    let audit = list_audit_rows(&pool).await.unwrap();
    let tids: Vec<Uuid> = audit.iter().map(|r| r.transition_id).collect();
    assert!(tids.contains(&original_tid), "original audit row preserved");
    assert!(
        tids.contains(&compensation_tid),
        "compensation audit row written"
    );
}

#[tokio::test]
async fn process_one_outbox_row_marks_compensation_failed_when_compensation_rejected() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_post_simple_entry(&pool, "entry_001").await;
    let spec = unbalanced_compensation_spec("entry_001");

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysNonRetryable::new("delivery failed"),
        Some(&spec),
        Utc::now(),
    )
    .await
    .unwrap();
    let rejection_reason = match outcome {
        ProcessOutcome::CompensationFailed { reason, .. } => reason,
        other => panic!("expected CompensationFailed, got {other:?}"),
    };
    assert!(
        rejection_reason.contains("balanced_posted_entry"),
        "rejection reason should name the invariant that fired, got: {rejection_reason}"
    );

    // The row is in compensation_failed; failure_reason now reflects
    // the compensation rejection (overwriting the original
    // delivery failure reason, per the helper's documented behavior).
    let (status, failure_reason, compensation_transition_id): (
        String,
        Option<String>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT status, failure_reason, compensation_transition_id
         FROM morpholog.outbox
         WHERE intent_type='JournalEntryPosted' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "compensation_failed");
    assert!(failure_reason.unwrap().contains("balanced_posted_entry"));
    assert!(
        compensation_transition_id.is_none(),
        "no compensation transition committed; pointer must remain NULL"
    );
}

// ============================================================
// LeaseLost coverage
// ============================================================
//
// Two tests force the lease to expire mid-deliver, exercising
// the processor's LeaseLost return paths on different intended
// outcomes. The third (LeaseLost during compensation completion)
// would require a hook between begin_compensation and
// complete_compensation that the current processor API does not
// expose; the orphan-audit case is documented in
// docs/outbox-sketch.md but not pinned by an automated test.

#[tokio::test]
async fn process_one_outbox_row_returns_lease_lost_when_delivery_lease_expires() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_post_simple_entry(&pool, "entry_001").await;

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &ExpireLeaseThenReturn {
            pool: pool.clone(),
            outcome: DeliveryOutcome::Delivered,
        },
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, ProcessOutcome::LeaseLost { .. }),
        "expected LeaseLost (delivery-mark branch), got {outcome:?}"
    );

    // Row was NOT moved to `delivered`. It is still in_progress
    // under the expired lease (an expired-lease reclaim by the
    // next claim would set things right, but that has not yet
    // happened in this test).
    let (status, delivered_at): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, delivered_at FROM morpholog.outbox LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "in_progress");
    assert!(
        delivered_at.is_none(),
        "mark_outbox_delivered must NOT have applied; row should look untouched"
    );
}

#[tokio::test]
async fn process_one_outbox_row_returns_lease_lost_on_failed_branch_when_lease_expires() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_post_simple_entry(&pool, "entry_001").await;
    // A compensation spec is supplied here on purpose: the test
    // pins that the early-return prevents the compensation arm
    // from running when mark_outbox_failed itself was a no-op.
    let spec = balanced_reversal_spec("entry_001");

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &ExpireLeaseThenReturn {
            pool: pool.clone(),
            outcome: DeliveryOutcome::NonRetryable {
                reason: "would-be terminal failure".to_string(),
            },
        },
        Some(&spec),
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, ProcessOutcome::LeaseLost { .. }),
        "expected LeaseLost (failed-mark branch), got {outcome:?}"
    );

    // Row was NOT moved to failed; the compensation arm was NOT
    // entered (no second audit row written).
    let (status, failure_reason): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_reason FROM morpholog.outbox LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "in_progress");
    assert!(failure_reason.is_none());
    let audit_count: (i64,) = sqlx::query_as("SELECT count(*) FROM morpholog.audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        audit_count.0, 1,
        "only the original commit's audit row should exist; \
         compensation must NOT have run when mark_outbox_failed was a no-op"
    );
}
