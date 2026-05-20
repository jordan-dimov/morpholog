//! Spike for the outbox worker + compensation pattern.
//!
//! Pairs with [`docs/outbox-sketch.md`]. Demonstrates the full
//! delivery + compensation flow end-to-end using hand-rolled code,
//! with no production worker, no `Deliverer` trait, no supervisor.
//! Every caller who wants outbox delivery + compensation today has
//! to write something equivalent to this. That ugliness is the case
//! for the implementation PR(s).
//!
//! The spike uses the existing `double_entry_ledger` transformations
//! as both the compensable step (`post_simple_entry` against
//! `account_cash` / `account_revenue`) and the compensation
//! (`post_simple_entry` with debit and credit accounts swapped, so
//! the reverse entry balances the original under
//! `balanced_posted_entry`). No new transformations or claim
//! predicates are introduced; the spike's job is to demonstrate the
//! coordinator-shaped wiring, not to add domain content.
//!
//! [`docs/outbox-sketch.md`]: ../../../docs/outbox-sketch.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{EvalValue, IntentInstance, Invariant, Transformation};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{PgPool, PgProposalOutcome, list_audit_rows, list_pending_outbox};
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

fn expect_committed(outcome: PgProposalOutcome) -> Uuid {
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => {
            panic!("expected Committed; got Rejected({reason})")
        }
    }
}

// ============================================================
// Stand-ins for the production Deliverer trait + WorkerConfig.
// Defined locally in the spike so the spike does not depend on any
// crate that does not yet exist.
// ============================================================

/// Three-way outcome borrowed from MassTransit / NServiceBus's
/// vocabulary, named per the design doc's lean.
#[derive(Debug, PartialEq)]
enum DeliveryOutcome {
    Delivered,
    /// A future implementation will retry after `retry_after_ms`;
    /// the spike does not exercise this branch.
    #[allow(dead_code)]
    Transient {
        retry_after_ms: u64,
    },
    NonRetryable {
        reason: String,
    },
}

/// A stand-in for the production `Deliverer` trait. Function
/// pointer in the spike; trait in the production crate.
type SpikeDeliverer = fn(&IntentInstance) -> DeliveryOutcome;

fn mock_deliverer_always_succeeds(_intent: &IntentInstance) -> DeliveryOutcome {
    DeliveryOutcome::Delivered
}

fn mock_deliverer_always_fails_nonretryably(_intent: &IntentInstance) -> DeliveryOutcome {
    DeliveryOutcome::NonRetryable {
        reason: "counterparty bank rejected wire: AML routing lock".to_string(),
    }
}

/// A stand-in for the production `CompensationSpec`. Holds the
/// compensating transformation plus the args to invoke it with and
/// the invariants to evaluate against.
///
/// In production this will be configured per intent type and the
/// `args` will come from an `Fn(&IntentInstance, &str) -> Vec<EvalValue>`
/// mapper; here in the spike, the caller of `process_one_pending`
/// pre-resolves both.
struct SpikeCompensation {
    transformation: Transformation,
    args: Vec<EvalValue>,
    invariants: Vec<Invariant>,
}

// ============================================================
// Hand-rolled consumer loop.
//
// This is the thing the production worker will eventually do, in
// one procedural function for the spike. It takes one pending
// outbox row, invokes the deliverer, and routes the outcome.
//
// The production version will:
//   - run in a supervised tokio task per delivery target;
//   - use SELECT ... FOR UPDATE SKIP LOCKED to coordinate with
//     other workers safely;
//   - wrap the deliverer call in a per-target circuit breaker;
//   - implement backoff with jitter for Transient outcomes;
//   - record `failed_at`, `failure_reason`, and
//     `compensation_transition_id` in new columns the schema does
//     not yet have.
//
// The spike does none of those; it just exercises the
// commit -> deliver -> route -> (maybe compensate) sequence so the
// audit-log shape can be asserted.
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
    let row = match pending.first() {
        Some(r) => r,
        None => return Ok(None),
    };

    // Reconstruct an IntentInstance from the outbox row. In
    // production this is the shape the Deliverer trait operates on.
    let intent = IntentInstance {
        name: row.intent_type.clone(),
        args: row.arguments.clone(),
    };

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
            // Mark the outbox row as failed. The production worker
            // will additionally record `failed_at`, `failure_reason`,
            // and (below) `compensation_transition_id` in new columns
            // the schema does not yet have; for the spike, `status`
            // alone is enough.
            sqlx::query(
                "UPDATE morpholog.outbox
                 SET status='failed', attempt_count=attempt_count+1, last_attempt_at=now()
                 WHERE intent_id=$1",
            )
            .bind(row.intent_id)
            .execute(pool)
            .await?;

            // If a compensation is wired, invoke it via
            // propose_against_pg. The compensation goes through
            // every invariant check just like any other
            // transformation, and writes its own audit row. The
            // ledger never lies: an auditor reading the audit log
            // will see commit -> failure -> compensation.
            if let Some(comp) = compensation {
                let outcome = common::propose_pg_with_test_actor(
                    pool,
                    &comp.transformation,
                    comp.args,
                    &comp.invariants,
                )
                .await?;
                match outcome {
                    PgProposalOutcome::Committed { transition_id, .. } => Ok(Some(transition_id)),
                    PgProposalOutcome::Rejected { reason } => {
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

/// Headline test: terminal delivery failure triggers a compensating
/// transformation that goes through every invariant check and
/// writes its own audit row. The audit log then preserves full
/// lineage: original commit, then compensation.
#[tokio::test]
async fn outbox_spike_compensates_on_nonretryable_failure() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = double_entry_ledger::all_invariants();
    let period = subj("p_spike");

    // 1. Commit the original transformation: post entry_001 with
    //    cash debit 100, revenue credit 100. This commits and
    //    enqueues a JournalEntryPosted intent.
    let tid_commit = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_001"),
                subj("d_2026_05_17"),
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

    // 2. The outbox row should be pending and carry the
    //    JournalEntryPosted intent.
    let pending = list_pending_outbox(&pool).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].intent_type, "JournalEntryPosted");

    // 3. Define the compensation: post a reversing entry with
    //    debit and credit accounts swapped, so the
    //    `balanced_posted_entry` invariant holds for both the
    //    original and the reversal.
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
        invariants: invariants.clone(),
    };

    // 4. Process the pending row with a deliverer that always
    //    returns NonRetryable. The consumer marks the row failed
    //    and invokes the compensation.
    let compensation_tid = process_one_pending(
        &pool,
        mock_deliverer_always_fails_nonretryably,
        Some(compensation),
    )
    .await
    .unwrap()
    .expect("compensation should have fired on NonRetryable");

    // 5. No rows remain pending. The original row is in status
    //    'failed'. The reversal's notification is queued (the
    //    compensation transformation itself emits its own intent),
    //    which is fine - it would be delivered on the next
    //    consumer pass.
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

    // 6. The audit log preserves the full lineage. Two transitions:
    //    the original commit and the compensation.
    let audit = list_audit_rows(&pool).await.unwrap();
    assert_eq!(
        audit.len(),
        2,
        "audit log must contain both the original commit and the compensation"
    );
    assert_eq!(audit[0].transition_id, tid_commit);
    assert_eq!(audit[1].transition_id, compensation_tid);

    // 7. Current state contains both the original and the reversal.
    //    The audit log is append-only; the reversal cancels the
    //    original semantically (via balanced debits and credits)
    //    but the original stays admitted - history is preserved.
    //    A trial-balance read against current state would now show
    //    zero balances on cash and revenue.
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

/// Happy path: successful delivery marks the outbox row delivered
/// and invokes no compensation. Pinned alongside the failure path
/// for symmetry - the worker's contract is "route based on
/// outcome," not "always compensate."
#[tokio::test]
async fn outbox_spike_marks_delivered_on_success() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = double_entry_ledger::all_invariants();

    // 1. Commit a transformation. Same shape as the failure test;
    //    the difference is the deliverer below.
    let _tid = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_001"),
                subj("d_2026_05_17"),
                subj("p_spike"),
                subj("account_cash"),
                subj("account_revenue"),
                dec(100),
            ],
            &invariants,
        )
        .await
        .unwrap(),
    );

    // 2. Process with a succeeding deliverer; no compensation is
    //    wired (None) - compensation is irrelevant when delivery
    //    succeeds.
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

    let (status, delivered_at): (String, Option<chrono::DateTime<chrono::Utc>>) =
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
