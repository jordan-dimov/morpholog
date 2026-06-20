//! Smoke test for [`morpholog_outbox::StdoutDeliverer`].
//!
//! Verifies that the worker happy-path works end-to-end with a
//! concrete deliverer (not a test-only stub): commit a row,
//! process it via the StdoutDeliverer, observe `delivered` status.
//! Output to stdout is not captured or asserted here - the test's
//! job is to pin the wiring, not the format.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

mod common;
use common::{commit_simple_entry, reset_db, test_pool};

use morpholog_outbox::{StdoutDeliverer, process_available_outbox_rows};
use morpholog_postgres::ProcessOutcome;

#[tokio::test]
async fn stdout_deliverer_marks_row_delivered_via_drain() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let _ = commit_simple_entry(&pool, "entry_001", "p_stdout").await;

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
