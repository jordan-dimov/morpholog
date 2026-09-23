//! The outbox delivery and compensation flow, in hand-rolled code.
//!
//! Pairs with [`docs/outbox-sketch.md`]. `morpholog-outbox`'s worker
//! automates this; the hand-rolled version keeps the database contract
//! tested apart from that worker.
//!
//! The step is `post_simple_entry` from `double_entry_ledger`; the
//! compensation is the same transformation with debit and credit
//! swapped, so both entries balance.
//!
//! [`docs/outbox-sketch.md`]: ../../../docs/outbox-sketch.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{EvalValue, IntentInstance, Transformation};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{PgPool, PgProposalOutcome, list_audit_rows, list_pending_outbox};
use uuid::Uuid;

mod common;
use common::{dec, intent_instance, subj};
use common::{expect_committed, reset_db, test_pool};

// ============================================================
// Test infrastructure
// ============================================================

// ============================================================
// Local stand-ins for the worker's Deliverer and CompensationSpec.
// ============================================================

/// Three-way outcome, in MassTransit / NServiceBus vocabulary.
#[derive(Debug, PartialEq)]
enum DeliveryOutcome {
    Delivered,
    /// Retry after `retry_after_ms`; not exercised here.
    #[allow(dead_code)]
    Transient {
        retry_after_ms: u64,
    },
    NonRetryable {
        reason: String,
    },
}

/// A stand-in for the `Deliverer` trait.
type SpikeDeliverer = fn(&IntentInstance) -> DeliveryOutcome;

fn mock_deliverer_always_succeeds(_intent: &IntentInstance) -> DeliveryOutcome {
    DeliveryOutcome::Delivered
}

fn mock_deliverer_always_fails_nonretryably(_intent: &IntentInstance) -> DeliveryOutcome {
    DeliveryOutcome::NonRetryable {
        reason: "counterparty bank rejected wire: AML routing lock".to_string(),
    }
}

/// A stand-in for `CompensationSpec`: the compensating transformation
/// and its args, already resolved.
struct SpikeCompensation {
    transformation: Transformation,
    args: Vec<EvalValue>,
}

// ============================================================
// Hand-rolled consumer loop.
//
// Takes one pending outbox row, calls the deliverer, and routes the
// outcome: commit -> deliver -> route -> maybe compensate. No leases,
// retries or circuit breaking; just enough to check the audit log.
// ============================================================

/// Returns the compensation's `transition_id` if compensation
/// fired; `None` otherwise (Delivered, Transient with no
/// compensation, or no pending row).
async fn process_one_pending(
    pool: &PgPool,
    deliverer: SpikeDeliverer,
    compensation: Option<SpikeCompensation>,
) -> Result<Option<Uuid>, Box<dyn std::error::Error>> {
    let pending = list_pending_outbox(pool).await?;
    let Some(row) = pending.first() else {
        return Ok(None);
    };

    // The shape a Deliverer works on.
    let intent = intent_instance(&row.intent_type, &row.arguments);

    match deliverer(&intent) {
        DeliveryOutcome::Delivered => {
            sqlx::query(
                "UPDATE morpholog.outbox
                 SET status='delivered', delivered_at=now(), attempt_count=attempt_count+1
                 WHERE intent_id=$1",
            )
            .bind(row.intent_id)
            .execute(pool)
            .await?;
            Ok(None)
        }
        DeliveryOutcome::Transient { .. } => {
            sqlx::query(
                "UPDATE morpholog.outbox
                 SET attempt_count=attempt_count+1, last_attempt_at=now()
                 WHERE intent_id=$1",
            )
            .bind(row.intent_id)
            .execute(pool)
            .await?;
            Ok(None)
        }
        DeliveryOutcome::NonRetryable { reason: _reason } => {
            sqlx::query(
                "UPDATE morpholog.outbox
                 SET status='failed', attempt_count=attempt_count+1, last_attempt_at=now()
                 WHERE intent_id=$1",
            )
            .bind(row.intent_id)
            .execute(pool)
            .await?;

            // The compensation goes through propose_against_pg like any
            // other transformation, so it is checked and audited.
            if let Some(comp) = compensation {
                let outcome = common::propose_pg_with_test_actor(
                    pool,
                    &common::compiled(double_entry_ledger::program()),
                    &comp.transformation,
                    comp.args,
                )
                .await?;
                match outcome {
                    PgProposalOutcome::Committed { transition_id, .. } => Ok(Some(transition_id)),
                    PgProposalOutcome::Rejected { reason, .. } => {
                        panic!(
                            "compensation transformation was rejected by an invariant: {reason}. \
                             In production this is the genuinely-broken state; the worker should \
                             leave the outbox row in a 'compensation_failed' state and require \
                             operator intervention."
                        );
                    }
                }
            } else {
                Ok(None)
            }
        }
    }
}

// ============================================================
// Tests
// ============================================================

/// A terminal delivery failure triggers a compensating transformation,
/// checked and audited like any other. The audit log holds the original
/// commit, then the compensation.
#[tokio::test]
async fn outbox_spike_compensates_on_nonretryable_failure() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let period = subj("p_spike");

    // 1. Post entry_001: cash debit 100, revenue credit 100. This
    //    enqueues a JournalEntryPosted intent.
    let tid_commit = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &common::compiled(double_entry_ledger::program()),
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_001"),
                subj("d_2026_05_17"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(100),
            ],
        )
        .await
        .unwrap(),
    );

    // 2. The outbox row should be pending and carry the
    //    JournalEntryPosted intent.
    let pending = list_pending_outbox(&pool).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].intent_type, "JournalEntryPosted");

    // 3. The compensation: a reversing entry with debit and credit
    //    swapped, so both entries balance.
    let compensation = SpikeCompensation {
        transformation: double_entry_ledger::post_simple_entry(),
        args: vec![
            subj("entry_001_reversal"),
            subj("d_2026_05_17"),
            period.clone(),
            subj("account_revenue"), // now the debit side
            subj("account_cash"),    // now the credit side
            dec(100),
        ],
    };

    // 4. A deliverer that always returns NonRetryable: the row is
    //    marked failed and the compensation runs.
    let compensation_tid = process_one_pending(
        &pool,
        mock_deliverer_always_fails_nonretryably,
        Some(compensation),
    )
    .await
    .unwrap()
    .expect("compensation should have fired on NonRetryable");

    // 5. The original row is 'failed'. Only the reversal's own intent
    //    is pending, for the next pass.
    let pending_after = list_pending_outbox(&pool).await.unwrap();
    assert_eq!(
        pending_after.len(),
        1,
        "the compensation transformation enqueues its own outbox row; \
         the original is no longer pending (it's 'failed')"
    );
    assert_eq!(
        pending_after[0].transition_id, compensation_tid,
        "the remaining pending row belongs to the compensation"
    );

    // 6. Two audit rows: the original commit and the compensation.
    let audit = list_audit_rows(&pool).await.unwrap();
    assert_eq!(
        audit.len(),
        2,
        "audit log must contain both the original commit and the compensation"
    );
    assert_eq!(audit[0].transition_id, tid_commit);
    assert_eq!(audit[1].transition_id, compensation_tid);

    // 7. Both the original and the reversal stay admitted. The
    //    reversal cancels the original in the balances, not by
    //    removing it.
    let (je_count, jl_count): (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM morpholog.claims WHERE predicate_name='JournalEntry'),
            (SELECT count(*) FROM morpholog.claims WHERE predicate_name='JournalLine')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(je_count, 2, "two JournalEntries: original + reversal");
    assert_eq!(jl_count, 4, "four JournalLines: 2 for each entry");
}

/// Successful delivery marks the row delivered and runs no compensation.
#[tokio::test]
async fn outbox_spike_marks_delivered_on_success() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // 1. Commit a transformation. Same shape as the failure test;
    //    the difference is the deliverer below.
    let _tid = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &common::compiled(double_entry_ledger::program()),
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_001"),
                subj("d_2026_05_17"),
                subj("p_spike"),
                subj("account_cash"),
                subj("account_revenue"),
                dec(100),
            ],
        )
        .await
        .unwrap(),
    );

    // 2. A succeeding deliverer, with no compensation wired.
    let result = process_one_pending(&pool, mock_deliverer_always_succeeds, None)
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "no compensation should have fired on Delivered"
    );

    // 3. Outbox row is now in 'delivered' state with `delivered_at`
    //    set. No rows remain pending.
    let pending = list_pending_outbox(&pool).await.unwrap();
    assert!(
        pending.is_empty(),
        "no rows should be pending after successful delivery"
    );

    let (status, delivered_at): (String, Option<jiff_sqlx::Timestamp>) =
        sqlx::query_as("SELECT status, delivered_at FROM morpholog.outbox LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "delivered");
    assert!(
        delivered_at.is_some(),
        "delivered_at must be set when status='delivered'"
    );

    // 4. The audit log contains only the original commit. No
    //    compensation row, because nothing went wrong.
    let audit = list_audit_rows(&pool).await.unwrap();
    assert_eq!(
        audit.len(),
        1,
        "happy path writes one audit row; compensation does not fire"
    );
}
