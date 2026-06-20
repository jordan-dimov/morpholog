//! Shared test helpers for the morpholog-postgres integration tests.
//!
//! Sync helpers (constructors, default actor, in-memory propose
//! wrappers) come from `morpholog-test-support` via the re-export
//! below. This file owns the **async** PG-specific wrappers
//! (`propose_pg_*`) because they depend on `morpholog-postgres`
//! itself - putting them in test-support would create a dep cycle
//! and would also force tokio/sqlx into every consumer of the
//! support crate.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Definition, EvalValue, Invariant, Subject, Transformation, Transition};
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, PgTracedOutcome, propose_against_pg,
    propose_against_pg_with_trace,
};
use uuid::Uuid;

/// Connect to the integration-test database named by `DATABASE_URL`.
/// These suites share one schema and TRUNCATE it on entry, so point
/// `DATABASE_URL` at a disposable database (`postgres:///morpholog_dev`).
pub async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

/// Truncate the governed `morpholog.*` tables - the default reset every
/// integration test runs on entry.
pub async fn reset_db(pool: &PgPool) {
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.rejections CASCADE")
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

/// `reset_db` plus the `morpholog_read.*` derived cache. **Only** for the
/// tests that exercise the derived read cache or SQL views; do not make
/// this the default reset - everything else uses [`reset_db`].
pub async fn reset_db_and_read_cache(pool: &PgPool) {
    sqlx::raw_sql(
        "TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.rejections CASCADE; \
         TRUNCATE morpholog_read.derived_claims, morpholog_read.derived_active, \
                  morpholog_read.derived_refreshes CASCADE;",
    )
    .execute(pool)
    .await
    .expect("reset");
}

/// Unwrap a committed outcome's transition id, panicking on rejection -
/// the common shape for tests that set up state they expect to commit.
pub fn expect_committed(outcome: PgProposalOutcome) -> Uuid {
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => {
            panic!("expected Committed; got Rejected({reason})")
        }
    }
}

// Re-export the test-support surface so per-test files can `use
// common::{subj, dec, ...};` rather than depending on
// morpholog-test-support directly. The `allow(unused_imports)` is
// necessary because each per-test file pulls a different subset:
// without it, every binary that doesn't use the full set generates
// noise pointing at the re-export rather than the file that's
// actually missing the import.
#[allow(unused_imports)]
pub use morpholog_test_support::{
    bool_, claim_instance, coll, date, dec, dec_str, has_claim, intent_instance, role, subj,
    test_actor, test_transition,
};

/// Convenience for tests that previously called `propose_against_pg`
/// with the old `(pool, transformation, args, invariants)` shape.
/// Constructs the `Transition` with `test_actor()` and forwards.
pub async fn propose_pg_with_test_actor(
    pool: &PgPool,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<PgProposalOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg(pool, transformation, &transition, invariants, definitions).await
}

/// `propose_pg_with_test_actor` plus structured trace. Wraps the
/// new `propose_against_pg_with_trace` and uses the shared
/// `test_actor()` for tests that don't model authority.
pub async fn propose_pg_with_trace_using_test_actor(
    pool: &PgPool,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<PgTracedOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg_with_trace(pool, transformation, &transition, invariants, definitions).await
}

/// Variant that lets the caller supply an explicit actor. Used by
/// authority tests that need to assert on which actor proposed which
/// transition.
pub async fn propose_pg_as(
    pool: &PgPool,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<PgProposalOutcome, PgError> {
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args,
        actor: actor.into(),
    };
    propose_against_pg(pool, transformation, &transition, invariants, definitions).await
}
