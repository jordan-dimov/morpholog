//! Shared test helpers for the in-memory example integration tests.
//!
//! Each per-example test file declares `mod common;` to bring these into
//! scope. Helpers are marked `#[allow(dead_code)]` because not every
//! integration test binary uses every helper, and Cargo treats each
//! `tests/*.rs` file as a separate compilation unit.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use jiff::civil::Date;
use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, Invariant, Outcome, State, Transformation, Transition,
    propose,
};
use rust_decimal::Decimal;

pub fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

pub fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
}

pub fn date(s: &str) -> EvalValue {
    EvalValue::Date(s.parse::<Date>().expect("valid ISO civil date"))
}

pub fn claim_instance(pred: &str, args: &[EvalValue]) -> ClaimInstance {
    ClaimInstance {
        predicate: pred.to_string(),
        args: args.to_vec(),
    }
}

pub fn test_actor() -> EvalValue {
    subj("test_actor")
}

pub fn test_transition(t: &Transformation, args: Vec<EvalValue>) -> Transition {
    Transition {
        transformation_name: t.name.clone(),
        args,
        actor: test_actor(),
    }
}

/// Convenience for tests that previously called `propose(t, args, ...)`
/// directly. Constructs a `Transition` with the shared `test_actor()`
/// and forwards to `propose`. Lets test sites keep their old shape
/// after the `propose` signature change.
pub fn propose_with_test_actor(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    invariants: &[Invariant],
) -> Result<Outcome, EvalError> {
    let transition = test_transition(t, args);
    propose(t, &transition, pre, invariants)
}

pub fn must_accept(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: State,
    invariants: &[Invariant],
) -> State {
    let transition = test_transition(t, args);
    match propose(t, &transition, &pre, invariants).expect("propose should not error") {
        Outcome::Accepted {
            candidate_state, ..
        } => candidate_state,
        Outcome::Rejected { reason } => {
            panic!(
                "expected Accepted from `{}`, got Rejected: {reason}",
                t.name
            )
        }
    }
}

/// Variant of `must_accept` that lets a test supply its own actor
/// rather than the shared `test_actor()`. Used by authority tests
/// that need to assert on which actor proposed which transition.
pub fn must_accept_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: EvalValue,
    pre: State,
    invariants: &[Invariant],
) -> State {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor,
    };
    match propose(t, &transition, &pre, invariants).expect("propose should not error") {
        Outcome::Accepted {
            candidate_state, ..
        } => candidate_state,
        Outcome::Rejected { reason } => {
            panic!(
                "expected Accepted from `{}`, got Rejected: {reason}",
                t.name
            )
        }
    }
}

/// Propose with a specific actor. Returns the raw `Outcome` so the
/// caller can inspect both `Accepted` and `Rejected` cases.
pub fn propose_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: EvalValue,
    pre: &State,
    invariants: &[Invariant],
) -> Result<Outcome, EvalError> {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor,
    };
    propose(t, &transition, pre, invariants)
}

pub fn has_claim(state: &State, predicate: &str, args: &[EvalValue]) -> bool {
    state.claims_for(predicate).any(|c| c.args == args)
}
