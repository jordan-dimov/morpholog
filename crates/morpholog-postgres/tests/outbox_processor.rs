//! End-to-end integration tests for the single-row processor
//! (`process_one_outbox_row`).
//!
//! A real outbox row goes through each branch of the delivery and
//! compensation state machine:
//! - happy-path delivery marks the row `delivered`;
//! - transient failures return the row to `pending` with a retry
//!   instant;
//! - non-retryable failures with no compensation spec mark the row
//!   `failed`;
//! - non-retryable failures with a compensation spec drive the
//!   row through the `failed` -> `compensation_in_progress` -> `failed`
//!   sequence with `compensation_transition_id` set;
//! - a compensating transformation rejected by an invariant leaves the
//!   row in `compensation_failed`.
//!
//! The example is `double_entry_ledger`: the original posts a balanced
//! entry, and the compensation posts the reversal with debit and credit
//! swapped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    CompensationSpec, Deliverer, DeliveryOutcome, OutboxRow, PgPool, PgProposalOutcome,
    ProcessOutcome, list_audit_rows, list_pending_outbox, process_one_outbox_row,
    testing::{AlwaysDelivers, AlwaysNonRetryable, AlwaysTransient},
};
use uuid::Uuid;

mod common;
use common::{dec, subj};
use common::{reset_db, test_pool};

// ============================================================
// Test infrastructure
// ============================================================

async fn commit_post_simple_entry(pool: &PgPool, entry_id: &str) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &common::compiled(double_entry_ledger::program()),
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj(entry_id),
            subj("d_2026_05_17"),
            subj("p_processor"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
    )
    .await
    .unwrap();
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => panic!("setup rejected: {reason}"),
    }
}

const INTENT_TYPE: &str = "JournalEntryPosted";
const LEASE: Duration = Duration::from_secs(30);

// ============================================================
// Deliverer stubs
// ============================================================
//
// The shared stubs live in `morpholog_postgres::testing`; only ones
// that reach into the database stay here.

/// Deliverer that expires its own lease before returning the configured
/// outcome, so the mark_* helper that follows returns
/// OutboxUpdate::LeaseLost.
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

/// A compensating transformation that balances: the reversal with debit
/// and credit swapped. The args are hardcoded, since the test knows the
/// original posting.
fn balanced_reversal_spec(suffix: &str) -> CompensationSpec {
    let suffix = suffix.to_string();
    CompensationSpec {
        transformation: double_entry_ledger::post_simple_entry(),
        invariants: double_entry_ledger::all_invariants(),
        definitions: double_entry_ledger::definitions(),
        args_from_row: Box::new(move |_row: &OutboxRow| {
            vec![
                subj(&format!("reversal_{suffix}")),
                subj("d_2026_05_17"),
                subj("p_processor"),
                // The original debited cash and credited revenue.
                subj("account_revenue"),
                subj("account_cash"),
                dec(100),
            ]
        }),
    }
}

/// A compensating transformation `balanced_posted_entry` always rejects:
/// `post_split_entry` with mismatched amounts.
fn unbalanced_compensation_spec(suffix: &str) -> CompensationSpec {
    let suffix = suffix.to_string();
    CompensationSpec {
        transformation: double_entry_ledger::post_split_entry(),
        invariants: double_entry_ledger::all_invariants(),
        definitions: double_entry_ledger::definitions(),
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
        Timestamp::now(),
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
        Timestamp::now(),
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
    let next = Timestamp::now() + SignedDuration::from_secs(120);

    let outcome = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysTransient {
            next_attempt_at: next,
        },
        None,
        Timestamp::now(),
    )
    .await
    .unwrap();
    match outcome {
        ProcessOutcome::TransientRetry {
            next_attempt_at, ..
        } => {
            assert!(
                next_attempt_at.duration_since(next).as_secs().abs() < 2,
                "next_attempt_at returned in ProcessOutcome must match what the deliverer requested"
            );
        }
        other => panic!("expected TransientRetry, got {other:?}"),
    }

    // The row is back to `pending` with next_attempt_at set.
    let (status, next_attempt_at): (String, Option<jiff_sqlx::Timestamp>) =
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
        Timestamp::now(),
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
        Timestamp::now(),
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

    // The audit log holds the original commit, then the compensating
    // transformation.
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
        Timestamp::now(),
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

    // The row is compensation_failed, and failure_reason now holds the
    // compensation's rejection instead of the delivery failure.
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
// These force the lease to expire mid-delivery. A lease lost during
// compensation would need a hook between begin_compensation and
// complete_compensation that the processor does not expose, so that
// case is documented in docs/outbox-sketch.md but not tested.

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
        Timestamp::now(),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, ProcessOutcome::LeaseLost { .. }),
        "expected LeaseLost (delivery-mark branch), got {outcome:?}"
    );

    // Not `delivered`: still in_progress under the expired lease, until
    // the next claim reclaims it.
    let (status, delivered_at): (String, Option<jiff_sqlx::Timestamp>) =
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
    // A compensation spec is supplied on purpose: it must not run when
    // mark_outbox_failed was a no-op.
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
        Timestamp::now(),
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

/// Compensation carries a bare transformation and no programme, so the
/// declaration check the propose paths run cannot see it. The authorisation
/// check reads the claims, which every path shares: a policy claim the
/// runtime cannot read stops a compensation commit too.
#[tokio::test]
async fn compensation_refuses_to_commit_under_an_unreadable_policy_claim() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let original_tid = commit_post_simple_entry(&pool, "entry_pol").await;
    let spec = balanced_reversal_spec("entry_pol");

    // A policy claim with the wrong arity. It looks like a restriction
    // but protects nothing, so nothing may commit while it stands.
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         VALUES ('ActorAssertionRestricted',
                 '[{\"type\":\"subject\",\"value\":\"a\"},
                   {\"type\":\"subject\",\"value\":\"extra\"}]'::jsonb,
                 $1)",
    )
    .bind(original_tid)
    .execute(&pool)
    .await
    .unwrap();

    let audit_before = list_audit_rows(&pool).await.unwrap().len();
    let result = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &AlwaysNonRetryable::new("counterparty rejected"),
        Some(&spec),
        Timestamp::now(),
    )
    .await;
    assert!(
        result.is_err(),
        "compensation must not commit under an unreadable policy: {result:?}"
    );
    assert_eq!(
        list_audit_rows(&pool).await.unwrap().len(),
        audit_before,
        "no compensating transition may have been written"
    );
}
