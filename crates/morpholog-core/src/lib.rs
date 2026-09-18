//! Morpholog v0 semantic kernel.
//!
//! The synchronous, pure heart of Morpholog. It defines the IR
//! (invariants, transformations, claims, statements, expressions),
//! evaluates invariants against in-memory state, and exposes [`propose`],
//! the function that turns a proposed transformation into either an
//! accepted post-state or a rejected attempt.
//!
//! Does no I/O. The PostgreSQL persistence adapter lives in the separate
//! `morpholog-postgres` crate and wraps this kernel as an async boundary;
//! async must not infect this crate. Worked-example IR lives in the
//! `morpholog-examples` crate.
//!
//! The kernel is the trust boundary: its evaluation and proposal paths
//! reject malformed input with a typed `EvalError`, never a `panic!`, and
//! it never touches floating point (business values are decimal). The
//! `warn`s below keep both mechanical - `panic!` stays out of non-test
//! code and float arithmetic is a compile error. (Internal guards still
//! `assert!` / `unreachable!` on structurally-impossible IR; those are
//! programmer-error checks on already-validated data, not the input path,
//! and `clippy::panic` covers neither.) Test code is exempt via
//! `.clippy.toml` (`allow-panic-in-tests`).
#![warn(clippy::panic, clippy::float_arithmetic)]

pub mod actor_repr;
mod admission;
pub mod format;
pub mod ir_builder;

pub mod analysis;
pub mod calendar;
mod check;
mod compiled;
mod controls;
mod coverage;
mod definitions;
mod derive;
mod disciplines;
mod eval;
mod explain;
mod fold;
mod guarantees;
mod impact;
mod ir;
mod lint;
mod propose;
pub mod schema;
mod score;
mod state;
mod sums;
mod validate;

pub use admission::{Admission, EffectiveDelta, effective_delta};
pub use analysis::{
    AnalysisError, ParamKind, has_admission_gate, predicates_asserted_by_stmt,
    predicates_read_by_stmt, predicates_referenced_by_derived, predicates_referenced_by_prop,
    predicates_written_by, transformation_param_kinds, transformations_asserting,
};
pub use compiled::CompiledProgram;
pub use controls::{
    ControlMatrix, GateControl, GateFrontLoad, GateRef, InvariantFrontLoad, TransformationControls,
    controls, render_controls,
};
pub use coverage::{
    CoverageReport, CoverageTracker, CoverageVerdict, InvariantCoverage, TransformationUsage,
    render_coverage,
};
pub use definitions::resolve_defined_calls;
pub use derive::{enumerate_derived, eval_invariant, invariant_witness};
pub use disciplines::{in_force_define_name, lower_discipline_definitions, lower_disciplines};
pub use eval::{EvalError, RenderedClaim};
pub use explain::{
    ErrorRejection, Explanation, GateKind, GateRejection, InvariantRejection, MissingClaim,
    Rejection, TransitionRef, Verdict, explain,
};
pub use guarantees::{Guarantee, guarantees, render_guarantees};
pub use impact::{Impact, ImpactPlan};
pub use ir::{
    ArgDecl, ArithOp, Builtin, Claim, CompareOp, Definition, DefinitionName, DefinitionOrigin,
    DerivedClaim, DerivedValue, Discipline, ExtremumOp, Intent, IntentDecl, IntentName, Invariant,
    InvariantName, InvariantOrigin, OrderedDomain, PredicateArgKind, PredicateDecl, PredicateName,
    Program, Prop, RuleName, Stmt, Subject, SumSeed, Term, Transformation, TransformationName,
    Unit, Value, ValueExpr, Var,
};
pub use lint::{Lint, SharedWriterPeer, lints, shared_writer_lints};
pub use propose::{
    BindOneOutcome, ForIterationTrace, Outcome, RejectionReason, RequireOutcome, StagedDelta,
    TraceEntry, TracedProposal, Transition, WitnessBinding, finish_staged_delta,
    finish_staged_delta_with, propose, propose_stage_delta, propose_with, propose_with_trace,
};
pub use schema::{intent_arg_schema, transformation_arg_schema};
pub use score::{
    BatchScore, CandidateScore, CandidateScorer, CaseOutcome, CaseResult, InvariantScore,
    SCORE_FORMAT_VERSION, SCORE_SEMANTICS, ScoreError, SliceInvariantScore, SliceScore,
    SplitBoundaryReport, SplitScore, invariants_using_pre,
};
pub use state::{ClaimInstance, Claims, EvalValue, IntentInstance, State};
pub use sums::lower_sum_seeds;
pub use validate::{ValidatedProgram, ValidationContext, ValidationError, VocabularyKind};

#[cfg(test)]
mod kernel_tests;
