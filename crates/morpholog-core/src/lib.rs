//! Morpholog v0 semantic kernel.
//!
//! The synchronous, pure heart of Morpholog. It defines the IR
//! (invariants, transformations, claims, statements, expressions),
//! evaluates invariants against in-memory state, and exposes
//! [`PreparedProgram::propose`], which turns a proposed transformation into
//! either an accepted post-state or a rejected attempt. Evaluation takes a
//! validated programme: [`PreparedProgram`] or [`ValidatedProgram`].
//!
//! Does no I/O. The PostgreSQL persistence adapter lives in the separate
//! `morpholog-postgres` crate and wraps this kernel as an async boundary;
//! async must not infect this crate. Worked-example IR lives in the
//! `morpholog-examples` crate.
//!
//! The kernel is the trust boundary. It rejects malformed input with a typed
//! `EvalError`, never a `panic!`, and it never uses floating point. The lints
//! below enforce both. Internal `assert!` / `unreachable!` guards on
//! already-validated IR remain; they catch programmer error, not bad input.
//! Tests are exempt via `.clippy.toml` (`allow-panic-in-tests`).
#![warn(clippy::panic, clippy::float_arithmetic)]

pub mod actor_repr;
mod admission;
pub mod format;
pub mod ir_builder;

pub mod analysis;
pub mod calendar;
mod check;
mod controls;
mod coverage;
mod definitions;
mod derive;
mod disciplines;
mod eval;
pub mod execution;
mod explain;
pub mod fold;
mod guarantees;
mod impact;
mod ir;
mod lint;
mod prepared;
mod propose;
mod reads;
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
pub use controls::{
    ControlMatrix, GateControl, GateFrontLoad, GateRef, InvariantFrontLoad, TransformationControls,
    controls, render_controls,
};
pub use coverage::{
    CoverageReport, CoverageTracker, CoverageVerdict, InvariantCoverage, TransformationUsage,
    render_coverage,
};
pub use definitions::resolve_defined_calls;
pub use disciplines::{in_force_define_name, lower_discipline_definitions, lower_disciplines};
pub use eval::{EvalError, RenderedClaim, literal_value, ordered_compare_error};
pub use explain::{
    ErrorRejection, Explanation, GateKind, GateRejection, InvariantRejection, MissingClaim,
    Rejection, TransitionRef, Verdict,
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
pub use lint::{
    Lint, PYTHON_CLIENT_MEMBERS, PYTHON_KEYWORDS, SharedWriterPeer, client_refuses_name, lints,
    shared_writer_lints,
};
pub use prepared::PreparedProgram;
pub use propose::{
    BindOneOutcome, ForIterationTrace, Outcome, RejectionReason, RequireOutcome, StagedDelta,
    SubjectSource, TraceEntry, TracedProposal, Transition, WitnessBinding,
    finish_staged_delta_with,
};
pub use reads::{KeyedPattern, KnownTerm, ReadFilter, ReadPlan};
pub use schema::{intent_arg_schema, transformation_arg_schema};
pub use score::{
    BatchScore, CandidateScore, CandidateScorer, CaseOutcome, CaseResult, InvariantScore,
    SCORE_FORMAT_VERSION, SCORE_SEMANTICS, ScoreError, SliceInvariantScore, SliceScore,
    SplitBoundaryReport, SplitScore, invariants_using_pre,
};
pub use state::{ClaimInstance, Claims, EvalValue, IntentInstance, State};
pub use sums::lower_sum_seeds;
pub use validate::{ValidatedProgram, ValidationContext, ValidationError, VocabularyKind};

/// The version of the kernel's semantics: what a validated programme means
/// when it is evaluated. It moves when the result of any public semantic
/// evaluation can change for the same programme, state and inputs: a
/// proposal's admitted change, emitted intents, rejection (rule, version,
/// witness) or error; an invariant's truth or error; a derived claim's rows
/// or error. Cost, plans, the compiled route, diagnostics, explanation
/// wording and wire formats are not semantics, and never move it.
///
/// Version 1 is the first semantics recorded, not the semantics of
/// everything before it. `runtime-semantics.md` lists what each version
/// changed.
pub const SEMANTICS_VERSION: u32 = 2;

#[cfg(test)]
mod kernel_tests;
