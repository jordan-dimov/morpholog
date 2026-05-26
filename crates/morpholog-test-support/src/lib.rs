//! Shared test helpers for the Morpholog workspace.
//!
//! Every test crate (`morpholog-core`, `morpholog-examples`,
//! `morpholog-postgres`, `morpholog-outbox`, `morpholog-cli`) ends up
//! re-defining the same handful of constructors and convenience
//! wrappers: build an [`EvalValue::Subject`] from a `&str`, build an
//! [`EvalValue::Decimal`] from an `i64`, run a sync [`propose`] with a
//! default actor. Before this crate, those re-definitions lived
//! inline in every per-test-file `mod common;` or directly in the
//! test file. Same shapes, same names, drifting independently. This
//! crate collapses them to one source of truth.
//!
//! Scope: **sync helpers only**. Async wrappers around
//! `propose_against_pg` live in `crates/morpholog-postgres/tests/common/`
//! because they depend on `morpholog-postgres` (which depends on
//! this crate would be illegal as a cycle, and would also pull
//! sqlx/tokio into every consumer). Keeping the sync surface in a
//! tiny no-async-deps crate keeps the dep graph clean.
//!
//! Naming convention:
//! - `subj`, `dec`, `date`, `bool_`, `coll`: construct an
//!   [`EvalValue`] from a Rust-friendly input.
//! - `role`: semantic alias for `subj` when the subject names a
//!   delegated role (e.g. `role(ROLE_RANDOMISE_PARTICIPANT)`); same
//!   runtime, documents reader intent. Mirrors the `ir_builder::role` term
//!   alias on the IR side.
//! - `test_actor`, `test_transition`: a shared default actor for
//!   tests that do not model authority. Authority-focused tests
//!   build their own [`Transition`] with a specific actor.
//! - `propose_with_test_actor`, `must_accept`, `must_accept_as`,
//!   `propose_as`: ergonomic wrappers over the kernel's
//!   [`propose`] surface.
//!
//! Helpers are `#[allow(dead_code)]` because not every test crate
//! uses every helper. `expect_used` and `unwrap_used` are allowed at
//! the crate level: this is test-fixture code where a malformed
//! fixture (a bad decimal string, a missing field, an unexpected
//! `Rejected`) is a test-author bug that should panic with a clear
//! message, not propagate a recoverable error up through the
//! call chain.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use jiff::civil::Date;
use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, IntentInstance, Invariant, Outcome, State, Subject,
    Transformation, Transition, propose,
};
use rust_decimal::Decimal;

// ============================================================
// EvalValue constructors
// ============================================================

/// Build an [`EvalValue::Subject`].
pub fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(s))
}

/// Semantic alias for [`subj`]. Identical runtime; documents reader
/// intent when the subject names a delegated role.
pub fn role(s: &str) -> EvalValue {
    subj(s)
}

/// Build an [`EvalValue::Decimal`] from an integer mantissa with scale
/// zero. The common case for hand-written test fixtures. For exact
/// decimals with a fractional part, use [`dec_str`].
pub fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

/// Build an [`EvalValue::Decimal`] by parsing a source string.
/// Panics on a malformed decimal - this is a test helper, not
/// production code, and a bad fixture is a test-author bug.
pub fn dec_str(s: &str) -> EvalValue {
    EvalValue::Decimal(s.parse::<Decimal>().expect("valid decimal string"))
}

/// Build an [`EvalValue::Date`] by parsing an ISO-8601 civil-date
/// string. Panics on a malformed date - same rationale as [`dec_str`].
pub fn date(s: &str) -> EvalValue {
    EvalValue::Date(s.parse::<Date>().expect("valid ISO civil date"))
}

/// Build an [`EvalValue::Bool`]. Named with a trailing underscore
/// because `bool` is a Rust type and an unsuffixed `bool(...)` reads
/// like a cast.
pub fn bool_(b: bool) -> EvalValue {
    EvalValue::Bool(b)
}

/// Build an [`EvalValue::Collection`] from a `Vec<EvalValue>`.
pub fn coll(items: Vec<EvalValue>) -> EvalValue {
    EvalValue::Collection(items)
}

// ============================================================
// Claim construction
// ============================================================

/// Build a [`ClaimInstance`] from a predicate name and an arg slice.
/// Convenience over `ClaimInstance { predicate: predicate.into(),
/// args: args.to_vec() }` at the call site.
pub fn claim_instance(predicate: &str, args: &[EvalValue]) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args: args.to_vec(),
    }
}

/// Build an [`IntentInstance`] from an intent name and an arg slice.
/// The emit-vocabulary mirror of [`claim_instance`].
pub fn intent_instance(name: &str, args: &[EvalValue]) -> IntentInstance {
    IntentInstance {
        name: name.into(),
        args: args.to_vec(),
    }
}

// ============================================================
// Default actor and transition
// ============================================================

/// Default actor for tests that do not model authority. Authority-
/// focused tests build their own [`Transition`] with a specific actor.
pub fn test_actor() -> Subject {
    Subject::from("test_actor")
}

/// Build a [`Transition`] with the shared [`test_actor`]. Used by
/// tests that need to pass a `&Transition` directly to functions
/// other than [`propose`].
pub fn test_transition(t: &Transformation, args: Vec<EvalValue>) -> Transition {
    Transition {
        transformation_name: t.name.clone(),
        args,
        actor: test_actor(),
    }
}

// ============================================================
// Sync propose helpers
// ============================================================

/// [`propose`] with the shared [`test_actor`]. Returns the raw
/// [`Outcome`] so callers can inspect both `Accepted` and `Rejected`.
pub fn propose_with_test_actor(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    invariants: &[Invariant],
) -> Result<Outcome, EvalError> {
    let transition = test_transition(t, args);
    propose(t, &transition, pre, invariants)
}

/// [`propose`] with a caller-supplied actor. Used by authority tests
/// that need to assert which actor proposed which transition.
pub fn propose_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: &State,
    invariants: &[Invariant],
) -> Result<Outcome, EvalError> {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor: actor.into(),
    };
    propose(t, &transition, pre, invariants)
}

/// Propose with [`test_actor`] and require the outcome to be
/// [`Outcome::Accepted`]. Returns the resulting candidate state for
/// chained setup steps. Panics on rejection or kernel error - this
/// is for fixture-building paths where any failure is a fixture bug.
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

/// [`must_accept`] with a caller-supplied actor. Used by tests that
/// need to assert on which actor was recorded.
pub fn must_accept_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: State,
    invariants: &[Invariant],
) -> State {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor: actor.into(),
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

// ============================================================
// State inspection
// ============================================================

/// Returns `true` iff `state` admits a claim with the given
/// predicate and exact argument list.
pub fn has_claim(state: &State, predicate: &str, args: &[EvalValue]) -> bool {
    state.claims_for(predicate).any(|c| c.args == args)
}
