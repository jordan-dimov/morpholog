//! Shared test helpers for the morpholog-postgres integration tests.
//!
//! Each per-test-file binary declares `mod common;` to bring these
//! into scope. Helpers are marked `#[allow(dead_code)]` because not
//! every integration test binary uses every helper.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{EvalValue, Invariant, Transformation, Transition};
use morpholog_postgres::{PgError, PgPool, PgProposalOutcome, propose_against_pg};

/// Default actor for integration tests that don't model authority.
/// Future authority-focused tests will supply their own actor.
pub fn test_actor() -> EvalValue {
    EvalValue::Subject("test_actor".to_string())
}

/// Build a `Transition` with the shared `test_actor()`. Used by tests
/// that need to pass a `&Transition` directly to functions other than
/// `propose_against_pg`.
pub fn test_transition(t: &Transformation, args: Vec<EvalValue>) -> Transition {
    Transition {
        transformation_name: t.name.clone(),
        args,
        actor: test_actor(),
    }
}

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
