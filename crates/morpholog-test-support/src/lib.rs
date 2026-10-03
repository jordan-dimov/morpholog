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
    ClaimInstance, DerivedClaim, EvalError, EvalValue, Explanation, IntentInstance, Invariant,
    Outcome, PreparedProgram, Program, RejectionReason, State, Subject, SubjectSource,
    TracedProposal, Transformation, Transition, ValidatedProgram,
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

/// Fresh UUIDv7 subjects for `new Subject()`, as the runtimes supply them:
/// for tests that need new identifiers but not particular ones. Never
/// restarts, so chained proposals cannot reuse a subject.
pub fn fresh() -> impl SubjectSource {
    std::iter::repeat_with(|| Subject::from(uuid::Uuid::now_v7().to_string()))
}

/// Exactly these subjects, in order, for tests that pin which
/// `new Subject()` gets which; running out is the kernel's typed error.
pub fn subjects<const N: usize>(ids: [&str; N]) -> impl SubjectSource {
    ids.map(Subject::from).into_iter()
}

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

/// Validate and prepare a test programme, panicking on a validation error:
/// evaluation takes only validated programmes.
pub fn prepare(program: &Program) -> PreparedProgram {
    PreparedProgram::new(program.clone())
        .unwrap_or_else(|errors| panic!("a test programme must validate: {errors:?}"))
}

fn transition_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
) -> Transition {
    Transition {
        transformation_name: t.name.clone(),
        args,
        actor: actor.into(),
    }
}

fn propose_in(
    prepared: &PreparedProgram,
    transition: &Transition,
    pre: &State,
) -> Result<Outcome, EvalError> {
    prepared
        .propose(transition, pre, &mut fresh())
        .map(|outcome| {
            outcome.unwrap_or_else(|| {
                panic!(
                    "no transformation `{}` in the programme",
                    transition.transformation_name
                )
            })
        })
}

fn accepted(outcome: Result<Outcome, EvalError>, t: &Transformation) -> State {
    match outcome.expect("propose should not error") {
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

fn rejected(outcome: Result<Outcome, EvalError>, t: &Transformation) -> RejectionReason {
    match outcome.expect("propose should not error") {
        Outcome::Rejected { reason } => reason,
        Outcome::Accepted { .. } => {
            panic!("expected Rejected from `{}`, got Accepted", t.name)
        }
    }
}

/// Propose `t` from `program` with the shared [`test_actor`], returning
/// the raw [`Outcome`].
pub fn propose_with_test_actor(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    program: &Program,
) -> Result<Outcome, EvalError> {
    propose_in(&prepare(program), &test_transition(t, args), pre)
}

/// [`propose_with_test_actor`] with a caller-supplied actor.
pub fn propose_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: &State,
    program: &Program,
) -> Result<Outcome, EvalError> {
    propose_in(&prepare(program), &transition_as(t, args, actor), pre)
}

/// Propose with [`test_actor`] and return the accepted candidate state, for chained setup.
/// Panics on rejection or kernel error.
pub fn must_accept(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: State,
    program: &Program,
) -> State {
    accepted(
        propose_in(&prepare(program), &test_transition(t, args), &pre),
        t,
    )
}

/// [`must_accept`] with a caller-supplied actor.
pub fn must_accept_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: State,
    program: &Program,
) -> State {
    accepted(
        propose_in(&prepare(program), &transition_as(t, args, actor), &pre),
        t,
    )
}

/// Propose with [`test_actor`] and return the [`RejectionReason`].
/// Panics on acceptance or kernel error.
pub fn must_reject(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    program: &Program,
) -> RejectionReason {
    rejected(
        propose_in(&prepare(program), &test_transition(t, args), pre),
        t,
    )
}

/// [`must_reject`] with a caller-supplied actor.
pub fn must_reject_as(
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
    pre: &State,
    program: &Program,
) -> RejectionReason {
    rejected(
        propose_in(&prepare(program), &transition_as(t, args, actor), pre),
        t,
    )
}

// ============================================================
// Validated evaluation
// ============================================================
//
// Evaluation takes a validated programme. These validate `program` first,
// panicking if it does not validate, and look the rule up by name.

/// [`PreparedProgram::propose`] for a test programme.
pub fn propose(
    program: &Program,
    transition: &Transition,
    pre: &State,
    subjects: &mut dyn SubjectSource,
) -> Result<Outcome, EvalError> {
    prepare(program)
        .propose(transition, pre, subjects)
        .map(|outcome| outcome.expect("the transition names a transformation in the programme"))
}

/// [`PreparedProgram::propose_with_trace`] for a test programme.
pub fn propose_with_trace(
    program: &Program,
    transition: &Transition,
    pre: &State,
    subjects: &mut dyn SubjectSource,
) -> TracedProposal {
    prepare(program)
        .propose_with_trace(transition, pre, subjects)
        .expect("the transition names a transformation in the programme")
}

/// [`PreparedProgram::explain`] for a test programme.
pub fn explain(
    program: &Program,
    transition: &Transition,
    pre: &State,
    subjects: &mut dyn SubjectSource,
) -> Explanation {
    prepare(program).explain(transition, pre, subjects)
}

/// Whether `invariant`, which `program` declares, holds in `state`.
pub fn eval_invariant(
    program: &Program,
    invariant: &Invariant,
    state: &State,
    pre: Option<&State>,
) -> Result<bool, EvalError> {
    validated(program)
        .eval_invariant(invariant.name.as_str(), state, pre)
        .map(|held| held.expect("the programme declares the invariant"))
}

/// The rows of `derived`, which `program` declares, in `state`.
pub fn enumerate_derived(
    program: &Program,
    derived: &DerivedClaim,
    state: &State,
) -> Result<Vec<ClaimInstance>, EvalError> {
    validated(program)
        .enumerate_derived(derived.predicate.as_str(), state)
        .map(|rows| rows.expect("the programme declares the derived claim"))
}

/// `program` validated, panicking on a validation error.
pub fn validated(program: &Program) -> ValidatedProgram<'_> {
    program
        .validated()
        .unwrap_or_else(|errors| panic!("a test programme must validate: {errors:?}"))
}

// ============================================================
// Example fixture
// ============================================================

/// A programme prepared once, so each propose call passes only what varies.
///
/// An act the example does not declare, such as one hand-built to skip the
/// gates, runs against the example's rules with that act declared beside
/// the example's own, in a programme that must still validate.
pub struct Example {
    program: Program,
    prepared: PreparedProgram,
}

impl Example {
    pub fn new(program: &Program) -> Self {
        Self {
            program: program.clone(),
            prepared: prepare(program),
        }
    }

    fn run(
        &self,
        t: &Transformation,
        transition: &Transition,
        pre: &State,
    ) -> Result<Outcome, EvalError> {
        match self.prepared.transformation(&t.name) {
            Some(declared) if declared == t => propose_in(&self.prepared, transition, pre),
            Some(_) => panic!(
                "`{}` differs from the example's act of that name; give the hand-built one its own name",
                t.name
            ),
            None => {
                let mut program = self.program.clone();
                program.transformations.push(t.clone());
                propose_in(&prepare(&program), transition, pre)
            }
        }
    }

    /// [`propose_with_test_actor`] against this example.
    pub fn propose(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        pre: &State,
    ) -> Result<Outcome, EvalError> {
        self.run(t, &test_transition(t, args), pre)
    }

    /// [`propose_as`] against this example.
    pub fn propose_as(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        actor: impl Into<Subject>,
        pre: &State,
    ) -> Result<Outcome, EvalError> {
        self.run(t, &transition_as(t, args, actor), pre)
    }

    /// [`must_accept`] against this example.
    pub fn must_accept(&self, t: &Transformation, args: Vec<EvalValue>, pre: State) -> State {
        accepted(self.propose(t, args, &pre), t)
    }

    /// [`must_accept_as`] against this example.
    pub fn must_accept_as(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        actor: impl Into<Subject>,
        pre: State,
    ) -> State {
        accepted(self.propose_as(t, args, actor, &pre), t)
    }

    /// [`must_reject`] against this example.
    pub fn must_reject(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        pre: &State,
    ) -> RejectionReason {
        rejected(self.propose(t, args, pre), t)
    }

    /// [`must_reject_as`] against this example.
    pub fn must_reject_as(
        &self,
        t: &Transformation,
        args: Vec<EvalValue>,
        actor: impl Into<Subject>,
        pre: &State,
    ) -> RejectionReason {
        rejected(self.propose_as(t, args, actor, pre), t)
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
