//! Shared test helpers for the morpholog-outbox integration tests.
//!
//! The outbox tests cannot reuse `morpholog-postgres`'s integration-test
//! `common` module (it belongs to a different crate's test binaries), so
//! this is the outbox crate's own copy of the same small infrastructure,
//! plus a `commit_simple_entry` setup the worker tests share.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{CompiledProgram, Subject, Transition};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{PgPool, PgProposalOutcome, Proposal, propose_against_pg};
use morpholog_test_support::{dec, subj};
use uuid::Uuid;

/// Connect to the integration-test database named by `DATABASE_URL`.
pub async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-outbox integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    let url = morpholog_postgres::with_default_user(&url);
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

/// Truncate the governed `morpholog.*` tables on test entry.
pub async fn reset_db(pool: &PgPool) {
    sqlx::query(morpholog_postgres::testing::RESET_SQL)
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

/// Unwrap a committed outcome's transition id, panicking on rejection.
pub fn expect_committed(outcome: PgProposalOutcome) -> Uuid {
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => {
            panic!("expected Committed; got Rejected({reason})")
        }
    }
}

/// Commit one balanced double-entry ledger entry so its `emit` lands an
/// outbox row - the setup every worker test opens with. `period` keeps
/// each test's rows distinct; the accounts derive from `entry_id` so the
/// entry balances. Returns the committed transition id.
pub async fn commit_simple_entry(pool: &PgPool, entry_id: &str, period: &str) -> Uuid {
    let transformation = double_entry_ledger::post_simple_entry();
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: vec![
            subj(entry_id),
            subj("d_2026_05_17"),
            subj(period),
            subj(&format!("cash_{entry_id}")),
            subj(&format!("revenue_{entry_id}")),
            dec(100),
        ],
        actor: Subject::from("outbox_test"),
    };
    let compiled = CompiledProgram::new(double_entry_ledger::program()).expect("valid programme");
    let proposal = Proposal::gateway(&transition);
    let outcome = propose_against_pg(pool, &compiled, &proposal)
        .await
        .unwrap();
    expect_committed(outcome)
}
