//! Shared test helpers for the in-memory example integration tests.
//!
//! Each per-example test file declares `mod common;` to bring these into
//! scope. Helpers are marked `#[allow(dead_code)]` because not every
//! integration test binary uses every helper, and Cargo treats each
//! `tests/*.rs` file as a separate compilation unit.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    ClaimInstance, EvalValue, Invariant, Outcome, State, Transformation, propose,
};
use rust_decimal::Decimal;

pub fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

pub fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
}

pub fn claim_instance(pred: &str, args: &[EvalValue]) -> ClaimInstance {
    ClaimInstance {
        predicate: pred.to_string(),
        args: args.to_vec(),
    }
}

pub fn must_accept(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: State,
    invariants: &[Invariant],
) -> State {
    match propose(t, args, &pre, invariants).expect("propose should not error") {
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

pub fn has_claim(state: &State, predicate: &str, args: &[EvalValue]) -> bool {
    state
        .claims
        .iter()
        .any(|c| c.predicate == predicate && c.args == args)
}
