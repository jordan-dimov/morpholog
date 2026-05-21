//! Smoke test for [`morpholog_outbox::StdoutDeliverer`].
//!
//! Verifies that the worker happy-path works end-to-end with a
//! concrete deliverer (not a test-only stub): commit a row,
//! process it via the StdoutDeliverer, observe `delivered` status.
//! Output to stdout is not captured or asserted here - the test's
//! job is to pin the wiring, not the format.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use morpholog_core::Transition;
use morpholog_test_support::{dec, subj};
use morpholog_examples::double_entry_ledger;
use morpholog_outbox::{StdoutDeliverer, process_available_outbox_rows};
use morpholog_postgres::{PgPool, PgProposalOutcome, ProcessOutcome, propose_against_pg};

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-outbox integration tests \
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



#[tokio::test]
async fn stdout_deliverer_marks_row_delivered_via_drain() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let transformation = double_entry_ledger::post_simple_entry();
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: vec![
            subj("entry_001"),
            subj("d_2026_05_17"),
            subj("p_stdout"),
            subj("cash"),
            subj("revenue"),
            dec(100),
        ],
        actor: subj("outbox_test"),
    };
    let outcome = propose_against_pg(
        &pool,
        &transformation,
        &transition,
        &double_entry_ledger::all_invariants(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    let outcomes = process_available_outbox_rows(
        &pool,
        "worker_a",
        "JournalEntryPosted",
        Duration::from_secs(30),
        &StdoutDeliverer,
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], ProcessOutcome::Delivered { .. }));

    let (status,): (String,) = sqlx::query_as("SELECT status FROM morpholog.outbox LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "delivered");
}
