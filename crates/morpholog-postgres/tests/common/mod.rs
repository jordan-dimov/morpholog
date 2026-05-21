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

use morpholog_core::{EvalValue, Invariant, Transformation, Transition};
use morpholog_postgres::{PgError, PgPool, PgProposalOutcome, propose_against_pg};

// Re-export the test-support surface so per-test files can `use
// common::{subj, dec, ...};` rather than depending on
// morpholog-test-support directly. The `allow(unused_imports)` is
// necessary because each per-test file pulls a different subset:
// without it, every binary that doesn't use the full set generates
// noise pointing at the re-export rather than the file that's
// actually missing the import.
#[allow(unused_imports)]
pub use morpholog_test_support::{
    bool_, claim_instance, coll, date, dec, dec_str, has_claim, role, subj, test_actor,
    test_transition,
};

/// Convenience for tests that previously called `propose_against_pg`
/// with the old `(pool, transformation, args, invariants)` shape.
/// Constructs the `Transition` with `test_actor()` and forwards.
pub async fn propose_pg_with_test_actor(
    pool: &PgPool,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    invariants: &[Invariant],
) -> Result<PgProposalOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg(pool, transformation, &transition, invariants).await
}

/// Variant that lets the caller supply an explicit actor. Used by
/// authority tests that need to assert on which actor proposed which
/// transition.
pub async fn propose_pg_as(
    pool: &PgPool,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    actor: EvalValue,
    invariants: &[Invariant],
) -> Result<PgProposalOutcome, PgError> {
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args,
        actor,
    };
    propose_against_pg(pool, transformation, &transition, invariants).await
}
