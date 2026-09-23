//! Shared sync test helpers for the Morpholog workspace, so test crates stop re-defining the
//! same constructors and drifting apart.
//!
//! - `subj`, `dec`, `date`, `bool_`, `coll`, ...: build an [`EvalValue`] from a plain Rust value.
//!   `role` is `subj` for a subject that names a delegated role.
//! - `test_actor`, `test_transition`: a default actor for tests that do not model authority.
//! - `propose_with_test_actor`, `propose_as`, `must_accept[_as]`, `must_reject[_as]`: wrappers
//!   over the kernel's [`propose`].
//!
//! Async helpers live in `crates/morpholog-postgres/tests/common/`: `morpholog-postgres`
//! depends on this crate, and keeping sqlx and tokio out keeps it small.
//!
//! A bad fixture is a test-author bug, so helpers panic with a clear message rather than
//! return errors.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

pub mod differential;

use jiff::civil::Date;
use morpholog_core::{
    ClaimInstance, Definition, EvalError, EvalValue, IntentInstance, Invariant, Outcome, Program,
    RejectionReason, State, Subject, Transformation, Transition, propose,
};
use rust_decimal::Decimal;

// ============================================================
// EvalValue constructors
// ============================================================

/// Build an [`EvalValue::Subject`].
pub fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(s))
}

/// [`subj`], for a subject that names a delegated role.
pub fn role(s: &str) -> EvalValue {
    subj(s)
}

/// Build an integer [`EvalValue::Decimal`]. For a fractional part, use [`dec_str`].
pub fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

/// Build an [`EvalValue::Decimal`] by parsing a string. Panics if it is malformed.
pub fn dec_str(s: &str) -> EvalValue {
    EvalValue::Decimal(s.parse::<Decimal>().expect("valid decimal string"))
}

/// Build an [`EvalValue::Timestamp`] from an RFC 3339 string. Panics if it is malformed.
pub fn ts(s: &str) -> EvalValue {
    EvalValue::Timestamp(s.parse().expect("test timestamp literal must parse"))
}

/// Build an [`EvalValue::Duration`] from an ISO-8601 string. Panics if it is malformed.
pub fn dur(s: &str) -> EvalValue {
    EvalValue::Duration(s.parse().expect("test duration literal must parse"))
}

/// Build an [`EvalValue::CalendarSpan`] with the kernel's own grammar. Panics if it is
/// malformed. Mostly for refusal tests: every storage and wire boundary rejects a span.
pub fn cal_span(s: &str) -> EvalValue {
    EvalValue::CalendarSpan(
        morpholog_core::calendar::parse_calendar_span(s)
            .expect("test calendar-span literal must parse"),
    )
}

/// Build an [`EvalValue::Quantity`] from a decimal string and a unit. Panics if the amount is
/// malformed.
pub fn qty(amount: &str, unit: &str) -> EvalValue {
    EvalValue::Quantity {
        amount: amount.parse().expect("test quantity amount must parse"),
        unit: morpholog_core::Unit::from(unit.to_string()),
    }
}

/// Build an [`EvalValue::Date`] from an ISO-8601 date string. Panics if it is malformed.
pub fn date(s: &str) -> EvalValue {
    EvalValue::Date(s.parse::<Date>().expect("valid ISO civil date"))
}

/// Build an [`EvalValue::Bool`]. The underscore avoids `bool(...)` reading like a cast.
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
pub fn claim_instance(predicate: &str, args: &[EvalValue]) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args: args.to_vec(),
    }
}

/// Build an [`IntentInstance`] from an intent name and an arg slice.
pub fn intent_instance(name: &str, args: &[EvalValue]) -> IntentInstance {
    IntentInstance {
        name: name.into(),
        args: args.to_vec(),
    }
}

// ============================================================
// Default actor and transition
// ============================================================

/// Default actor for tests that do not model authority.
pub fn test_actor() -> Subject {
    Subject::from("test_actor")
}

/// Build a [`Transition`] with the shared [`test_actor`].
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

/// [`propose`] with the shared [`test_actor`], returning the raw [`Outcome`].
pub fn propose_with_test_actor(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<Outcome, EvalError> {
    let transition = test_transition(t, args);
    propose(t, &transition, pre, invariants, definitions)
}

/// [`propose`] with a caller-supplied actor.
pub fn propose_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<Outcome, EvalError> {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor: actor.into(),
    };
    propose(t, &transition, pre, invariants, definitions)
}

/// Propose with [`test_actor`] and return the accepted candidate state, for chained setup.
/// Panics on rejection or kernel error.
pub fn must_accept(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> State {
    let transition = test_transition(t, args);
    match propose(t, &transition, &pre, invariants, definitions).expect("propose should not error")
    {
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

/// [`must_accept`] with a caller-supplied actor.
pub fn must_accept_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> State {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor: actor.into(),
    };
    match propose(t, &transition, &pre, invariants, definitions).expect("propose should not error")
    {
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

/// Propose with [`test_actor`] and return the [`RejectionReason`].
/// Panics on acceptance or kernel error.
pub fn must_reject(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> RejectionReason {
    let transition = test_transition(t, args);
    match propose(t, &transition, pre, invariants, definitions).expect("propose should not error") {
        Outcome::Rejected { reason } => reason,
        Outcome::Accepted { .. } => {
            panic!("expected Rejected from `{}`, got Accepted", t.name)
        }
    }
}

/// [`must_reject`] with a caller-supplied actor.
pub fn must_reject_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> RejectionReason {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor: actor.into(),
    };
    match propose(t, &transition, pre, invariants, definitions).expect("propose should not error") {
        Outcome::Rejected { reason } => reason,
        Outcome::Accepted { .. } => {
            panic!("expected Rejected from `{}`, got Accepted", t.name)
        }
    }
}

// ============================================================
// Example fixture
// ============================================================

/// A programme's rules bound once, so each propose call passes only what varies. Use the free
/// helpers to run against a subset of the rules.
pub struct Example {
    invariants: Vec<Invariant>,
    definitions: Vec<Definition>,
}

impl Example {
    pub fn new(program: &Program) -> Self {
        Self {
            invariants: program.invariants.clone(),
            definitions: program.definitions.clone(),
        }
    }

    /// [`propose_with_test_actor`] against this example's rules.
    pub fn propose(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        pre: &State,
    ) -> Result<Outcome, EvalError> {
        propose_with_test_actor(t, args, pre, &self.invariants, &self.definitions)
    }

    /// [`propose_as`] against this example's rules.
    pub fn propose_as(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        actor: impl Into<Subject>,
        pre: &State,
    ) -> Result<Outcome, EvalError> {
        propose_as(t, args, actor, pre, &self.invariants, &self.definitions)
    }

    /// [`must_accept`] against this example's rules.
    pub fn must_accept(&self, t: &Transformation, args: Vec<EvalValue>, pre: State) -> State {
        must_accept(t, args, pre, &self.invariants, &self.definitions)
    }

    /// [`must_accept_as`] against this example's rules.
    pub fn must_accept_as(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        actor: impl Into<Subject>,
        pre: State,
    ) -> State {
        must_accept_as(t, args, actor, pre, &self.invariants, &self.definitions)
    }

    /// [`must_reject`] against this example's rules.
    pub fn must_reject(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        pre: &State,
    ) -> RejectionReason {
        must_reject(t, args, pre, &self.invariants, &self.definitions)
    }

    /// [`must_reject_as`] against this example's rules.
    pub fn must_reject_as(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        actor: impl Into<Subject>,
        pre: &State,
    ) -> RejectionReason {
        must_reject_as(t, args, actor, pre, &self.invariants, &self.definitions)
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
