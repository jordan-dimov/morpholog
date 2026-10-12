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
use morpholog_core::PreparedProgram;
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
        &common::pg_program(double_entry_ledger::program()),
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
    CompensationSpec::new(
        PreparedProgram::new(double_entry_ledger::program()).expect("the ledger is valid"),
        "post_simple_entry".into(),
        Box::new(move |_row: &OutboxRow| {
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
    )
    .expect("the ledger declares the compensating transformation")
}

/// A compensating transformation `balanced_posted_entry` always rejects:
/// `post_split_entry` with mismatched amounts.
fn unbalanced_compensation_spec(suffix: &str) -> CompensationSpec {
    let suffix = suffix.to_string();
    CompensationSpec::new(
        PreparedProgram::new(double_entry_ledger::program()).expect("the ledger is valid"),
        "post_split_entry".into(),
        Box::new(move |_row: &OutboxRow| {
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
    )
    .expect("the ledger declares the compensating transformation")
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
    // The compensation is a commit like any other: it names the programme
    // its spec owns.
    let compensating = audit
        .iter()
        .find(|r| r.transition_id == compensation_tid)
        .unwrap();
    let ledger = PreparedProgram::new(double_entry_ledger::program()).unwrap();
    assert_eq!(
        compensating.model_hash.as_deref(),
        Some(ledger.model_hash())
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

/// A compensation names a transformation of the programme it owns; one the
/// programme does not declare is refused when the spec is built, not at
/// the first failed delivery.
#[test]
fn a_compensation_spec_refuses_a_transformation_its_programme_lacks() {
    let refused = CompensationSpec::new(
        PreparedProgram::new(double_entry_ledger::program()).expect("the ledger is valid"),
        "no_such_reversal".into(),
        Box::new(|_row: &OutboxRow| Vec::new()),
    );
    assert!(
        matches!(
            refused,
            Err(morpholog_postgres::PgError::UnknownTransformation { .. })
        ),
        "got {:?}",
        refused.err()
    );
}

// A worker loses its lease after the downstream saw the delivery, so it
// cannot record what happened. The fault modelled is a crash or a lost
// connection in that window, not a hostile party. Delivery is at least
// once: a second worker reclaims the row and delivers again, and both
// attempts must carry one stable identity the downstream can deduplicate
// on. Nothing promises the downstream sees the effect only once.

/// What the downstream sees of one delivery attempt.
#[derive(Debug, Clone, PartialEq)]
struct Attempt {
    intent_id: Uuid,
    idempotency_key: String,
    intent_type: String,
    arguments: Vec<morpholog_core::EvalValue>,
    transition_id: Uuid,
    accepted: bool,
}

/// A downstream that records every attempt. On the first one it accepts
/// the delivery or refuses it, as told, and then the worker's lease is
/// lost before it can record anything. Later attempts are accepted.
struct RecordingDownstream {
    pool: PgPool,
    first_accepted: bool,
    attempts: std::sync::Mutex<Vec<Attempt>>,
}
impl Deliverer for RecordingDownstream {
    async fn deliver(&self, row: &OutboxRow) -> DeliveryOutcome {
        let first = self.attempts.lock().unwrap().is_empty();
        let accepted = !first || self.first_accepted;
        self.attempts.lock().unwrap().push(Attempt {
            intent_id: row.intent_id,
            idempotency_key: row.idempotency_key.clone(),
            intent_type: row.intent_type.clone(),
            arguments: row.arguments.clone(),
            transition_id: row.transition_id,
            accepted,
        });
        if !first {
            return DeliveryOutcome::Delivered;
        }
        sqlx::query(
            "UPDATE morpholog.outbox SET lock_expires_at = now() - interval '1 second'
             WHERE intent_id=$1",
        )
        .bind(row.intent_id)
        .execute(&self.pool)
        .await
        .unwrap();
        if accepted {
            DeliveryOutcome::Delivered
        } else {
            DeliveryOutcome::Transient {
                next_attempt_at: Timestamp::now(),
            }
        }
    }
}

async fn governed_rows(pool: &PgPool) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM morpholog.audit),
                (SELECT count(*) FROM morpholog.claims),
                (SELECT count(*) FROM morpholog.outbox)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// One intent, a first worker that loses its lease after the downstream
/// answered, and a second worker that reclaims and delivers. Returns the
/// attempts the downstream saw.
async fn deliver_across_a_lost_lease(first_accepted: bool) -> Vec<Attempt> {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_post_simple_entry(&pool, "entry_redelivered").await;
    let before = governed_rows(&pool).await;
    assert_eq!(before.2, 1, "one intent to deliver");
    let downstream = RecordingDownstream {
        pool: pool.clone(),
        first_accepted,
        attempts: std::sync::Mutex::new(Vec::new()),
    };

    let lost = process_one_outbox_row(
        &pool,
        "worker_a",
        INTENT_TYPE,
        LEASE,
        &downstream,
        None,
        Timestamp::now(),
    )
    .await
    .unwrap();
    let ProcessOutcome::LeaseLost { intent_id } = lost else {
        panic!("the first worker should lose its lease, got {lost:?}");
    };
    let (status, delivered_at): (String, Option<jiff_sqlx::Timestamp>) =
        sqlx::query_as("SELECT status, delivered_at FROM morpholog.outbox WHERE intent_id=$1")
            .bind(intent_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        status, "in_progress",
        "nothing was recorded for the attempt"
    );
    assert!(delivered_at.is_none());
    assert_eq!(
        governed_rows(&pool).await,
        before,
        "an attempt changes no governed row"
    );

    let delivered = process_one_outbox_row(
        &pool,
        "worker_b",
        INTENT_TYPE,
        LEASE,
        &downstream,
        None,
        Timestamp::now(),
    )
    .await
    .unwrap();
    assert!(
        matches!(delivered, ProcessOutcome::Delivered { intent_id: id } if id == intent_id),
        "the second worker should reclaim and deliver the same intent, got {delivered:?}"
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM morpholog.outbox WHERE intent_id=$1")
            .bind(intent_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "delivered");
    assert_eq!(
        governed_rows(&pool).await,
        before,
        "redelivery adds no audit row, claim or intent"
    );

    let attempts = downstream.attempts.into_inner().unwrap();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    let identity = |a: &Attempt| Attempt {
        accepted: true,
        ..a.clone()
    };
    assert_eq!(
        identity(&attempts[0]),
        identity(&attempts[1]),
        "both attempts must carry one identity and payload"
    );
    assert_eq!(attempts[0].intent_id, intent_id);
    attempts
}

#[tokio::test]
async fn an_ambiguous_success_is_delivered_again_as_the_same_intent() {
    let attempts = deliver_across_a_lost_lease(true).await;
    assert!(
        attempts.iter().all(|a| a.accepted),
        "the downstream accepted both, and deduplicates on the key: {attempts:?}"
    );
}

#[tokio::test]
async fn a_refused_attempt_is_delivered_later_as_the_same_intent() {
    let attempts = deliver_across_a_lost_lease(false).await;
    assert_eq!(
        attempts.iter().map(|a| a.accepted).collect::<Vec<_>>(),
        [false, true],
        "the redelivery is the first accepted one"
    );
}
