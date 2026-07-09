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

use morpholog_core::{CompiledProgram, EvalValue, Program, Subject, Transformation, Transition};
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
///
/// First waits for other open transactions on the database to drain: a
/// previous test's pool closes its connections asynchronously, and a
/// straggler still inside a transaction lowers the audit watermark, so
/// a checkpoint taken by THIS test can otherwise cover none of its own
/// rows (withhold-never-lose working as designed, against a transaction
/// the test cannot see). Bounded, then proceeds - a wait this long means
/// something is genuinely stuck and the test should fail visibly.
pub async fn reset_db(pool: &PgPool) {
    for _ in 0..200 {
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database()
               AND pid != pg_backend_pid()
               AND xact_start IS NOT NULL",
        )
        .fetch_one(pool)
        .await
        .expect("failed to count open transactions");
        if open == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.audit_checkpoints, morpholog.rejections CASCADE")
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

/// `reset_db` plus the `morpholog_read.*` derived cache. **Only** for the
/// tests that exercise the derived read cache or SQL views; do not make
/// this the default reset - everything else uses [`reset_db`].
pub async fn reset_db_and_read_cache(pool: &PgPool) {
    sqlx::raw_sql(
        "TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.audit_checkpoints, morpholog.rejections CASCADE; \
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

/// Compile a test programme (validates + indexes). The facade-based
/// propose path now takes a `&CompiledProgram`; tests build one from an
/// example's `program()` and reuse it across that test's proposals.
pub fn compiled(program: Program) -> CompiledProgram {
    CompiledProgram::new(program).expect("test programme is valid")
}

/// Convenience for tests: build the `Transition` with `test_actor()` and
/// propose through the `CompiledProgram` facade. `transformation` names
/// the transition; the programme's rule slices come from `compiled`.
pub async fn propose_pg_with_test_actor(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
) -> Result<PgProposalOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg(pool, compiled, &transition).await
}

/// Admit an `AuditSigningKey(key_id, purpose, public_key)` claim through a
/// minimal key-governance programme - the keys-as-claims authorisation a
/// signed checkpoint is verified against. Lets the signing tests assert
/// `Intact` on a checkpoint whose key the ledger actually authorised.
pub async fn authorize_signing_key(pool: &PgPool, key_id: &str, purpose: &str, public_key: &str) {
    use morpholog_core::ir_builder::{assert_, params, predicate, program, transformation, var};
    let t = transformation(
        "authorize_signing_key",
        params(&["key_id", "purpose", "public_key"]),
        vec![assert_(
            "AuditSigningKey",
            vec![var("key_id"), var("purpose"), var("public_key")],
        )],
    );
    let prog = program("key_governance")
        .predicates(vec![
            predicate("AuditSigningKey")
                .subject("key_id")
                .subject("purpose")
                .subject("public_key")
                .build(),
        ])
        .transformations(vec![t.clone()])
        .build();
    let outcome = propose_pg_with_test_actor(
        pool,
        &compiled(prog),
        &t,
        vec![subj(key_id), subj(purpose), subj(public_key)],
    )
    .await
    .unwrap();
    expect_committed(outcome);
}

/// `propose_pg_with_test_actor` plus structured trace. Uses the shared
/// `test_actor()` for tests that don't model authority.
pub async fn propose_pg_with_trace_using_test_actor(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
) -> Result<PgTracedOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg_with_trace(pool, compiled, &transition).await
}

/// Variant that lets the caller supply an explicit actor. Used by
/// authority tests that need to assert on which actor proposed which
/// transition.
pub async fn propose_pg_as(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
) -> Result<PgProposalOutcome, PgError> {
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args,
        actor: actor.into(),
    };
    propose_against_pg(pool, compiled, &transition).await
}
