//! Transformation execution: the `propose` API, the `propose_with_trace`
//! diagnostic twin, and the supporting types they return.
//!
//! `propose` is the kernel's central entry point: given a transformation,
//! a transition (actor and arguments), a pre-state and the invariants, it
//! returns an `Outcome`. `propose_with_trace` also returns a
//! per-statement trace.
//!
//! Both run the same executor, so they cannot drift. Without tracing, no
//! trace storage is allocated.

use serde::{Deserialize, Serialize};

use crate::admission::{Admission, EffectiveDelta, effective_delta};
use crate::definitions::DefinitionTable;
use crate::derive::{eval_invariant_over, sorted_witness};
use crate::eval::{
    EvalContext, EvalError, RenderedClaim, eval_value, find_failure, find_matches, matching_claims,
    resolve_term, unsatisfied_positive_claims,
};
use crate::format;
use crate::impact::Impact;
use crate::ir::{
    ArgDecl, Claim, Definition, Intent, IntentName, Invariant, InvariantName, PredicateName, Prop,
    RuleName, Stmt, Subject, Term, Transformation, TransformationName, Var,
};
use crate::state::{
    Bindings, Bound, Candidate, ClaimInstance, EvalValue, IntentInstance, State, StateRef,
};

/// A proposed state transition. Persisted to the audit log when accepted.
///
/// - `transformation_name`: the transformation to run, by name.
/// - `args`: positional arguments matching the transformation's
///   `parameters`.
/// - `actor`: the [`Subject`] proposing it. It is context, not a
///   parameter, so domain arguments stay clean. Serialised as a tagged
///   [`EvalValue::Subject`] (see [`crate::actor_repr`]).
/// - `decided_at`: the decision time `decided_at` reads in the body,
///   given by the runtime: the deciding transaction's start, which the
///   audit row records as `committed_at`. Not serialised: the record
///   carries it in that column, and a caller of the runtime cannot
///   supply it. `None` where nothing gives one, and a body that reads it
///   then raises [`EvalError::UnboundDecisionTime`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub transformation_name: TransformationName,
    pub args: Vec<EvalValue>,
    #[serde(with = "crate::actor_repr")]
    pub actor: Subject,
    #[serde(skip)]
    pub decided_at: Option<jiff::Timestamp>,
}

/// The result of proposing a transformation: the candidate state is
/// admissible (Accepted), or a gate or invariant rejected it.
#[must_use = "a proposal outcome must be inspected; a dropped `Rejected` silently treats a refused change as if it had committed"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Accepted {
        asserted_claims: Vec<ClaimInstance>,
        retracted_claims: Vec<ClaimInstance>,
        emitted_intents: Vec<IntentInstance>,
        candidate_state: State,
    },
    Rejected {
        reason: RejectionReason,
    },
}

/// One variable and the value it held where an invariant failed.
///
/// It tells a reader *which* subject broke the rule. It is kept as data,
/// outside the reason string, because that string is a pinned wire format
/// and an embedder should read values, not parse prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessBinding {
    pub var: Var,
    pub value: EvalValue,
}

/// The two values a refused rule compared, where the diagnosis blames a
/// comparison or an equality: the operator as the source spells it, and
/// the operands evaluated under the bindings the diagnosis stopped at.
/// Kept as data for the same reason the witness is: an embedder prints
/// values, it does not reproduce the rule's arithmetic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compared {
    pub op: String,
    pub left: EvalValue,
    pub right: EvalValue,
}

impl Compared {
    /// Whether the comparison holds over its own operands, by the
    /// kernel's orderings; `None` when the operator or the operands are
    /// not of a shape it orders. A diagnosis attaches a comparison only
    /// when this is `Some(false)`: a refusal never names a comparison
    /// that held.
    pub fn holds(&self) -> Option<bool> {
        use std::cmp::Ordering;
        let ordering = || -> Option<Ordering> {
            match (&self.left, &self.right) {
                (EvalValue::Decimal(l), EvalValue::Decimal(r)) => Some(l.cmp(r)),
                (
                    EvalValue::Quantity {
                        amount: l,
                        unit: lu,
                    },
                    EvalValue::Quantity {
                        amount: r,
                        unit: ru,
                    },
                ) if lu == ru => Some(l.cmp(r)),
                (EvalValue::Date(l), EvalValue::Date(r)) => Some(l.cmp(r)),
                (EvalValue::Timestamp(l), EvalValue::Timestamp(r)) => Some(l.cmp(r)),
                (EvalValue::Duration(l), EvalValue::Duration(r)) => Some(l.cmp(r)),
                _ => None,
            }
        };
        match self.op.as_str() {
            "=" => Some(self.left == self.right),
            "!=" => Some(self.left != self.right),
            "<=" | "on_or_before" | "at_or_before" | "no_longer_than" => {
                ordering().map(|o| o != Ordering::Greater)
            }
            "<" | "before" | "strictly_before" | "shorter_than" => {
                ordering().map(|o| o == Ordering::Less)
            }
            ">=" | "on_or_after" | "at_or_after" | "no_shorter_than" => {
                ordering().map(|o| o != Ordering::Less)
            }
            ">" | "after" | "strictly_after" | "longer_than" => {
                ordering().map(|o| o == Ordering::Greater)
            }
            _ => None,
        }
    }
}

/// Why a proposal was rejected. [`std::fmt::Display`] gives the pinned
/// wire string used in envelopes, traces and the rejection log. To get the
/// rule name or kind, match the variant; never parse the display text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionReason {
    /// An invariant did not hold over the candidate state. `version` is
    /// the version checked. `witness` holds the bindings where it failed,
    /// empty when no single case can be blamed. Display omits both.
    Invariant {
        name: InvariantName,
        version: u32,
        witness: Vec<WitnessBinding>,
        /// The failing comparison's values, when the diagnosis blames
        /// one; absent otherwise, and absent when the diagnosis could
        /// not evaluate it.
        compared: Option<Compared>,
    },
    /// A `require` gate found no witness over the pre-state.
    ///
    /// `name` is the gate's optional stable identifier. Prefer it to
    /// `rendered`, which changes whenever the expression is reworded.
    /// `witness` holds the bindings in scope at the sub-expression the
    /// failure walk blames: the parameters, what earlier statements
    /// bound, and what the conjuncts before the failing one bound under
    /// the first surviving context. It is the context the diagnosis was
    /// made in, not every way the gate could have held. The actor is
    /// not among them; every envelope carries it apart.
    Require {
        name: Option<RuleName>,
        rendered: String,
        witness: Vec<WitnessBinding>,
        /// As for `Invariant`.
        compared: Option<Compared>,
    },
    /// A `bind` lookup matched no candidates. (Multi-match is an
    /// [`EvalError`], not a rejection.) `witness` as for `Require`.
    BindNone {
        name: Option<RuleName>,
        rendered: String,
        witness: Vec<WitnessBinding>,
        /// As for `Invariant`; a `bind` is a claim pattern, so rarely.
        compared: Option<Compared>,
    },
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectionReason::Invariant { name, .. } => {
                write!(f, "invariant `{name}` violated")
            }
            RejectionReason::Require { name, rendered, .. } => match name {
                Some(n) => write!(
                    f,
                    "require `{n}` failed: {rendered} did not hold over pre-state"
                ),
                None => write!(f, "require failed: {rendered} did not hold over pre-state"),
            },
            RejectionReason::BindNone { name, rendered, .. } => match name {
                Some(n) => write!(f, "bind `{n}` failed: {rendered} matched no candidates"),
                None => write!(f, "bind_one failed: {rendered} matched no candidates"),
            },
        }
    }
}

/// The failing part of a refused gate, rendered, the bindings it failed
/// under (the context the walk blamed, sorted by variable name) and the
/// comparison it blames, if one. Nothing more specific than the whole
/// gate means the bindings the gate started from, and the whole gate's
/// own comparison when it is one.
fn gate_failure(
    expr: &Prop,
    ctx: &EvalContext<'_>,
) -> (Option<String>, Vec<WitnessBinding>, Option<Compared>) {
    match find_failure(expr, ctx) {
        Some(failure) => (
            Some(failure.rendered),
            sorted_witness(failure.bindings),
            failure.compared,
        ),
        None => (
            None,
            sorted_witness(ctx.bindings.clone()),
            crate::eval::compared_at(expr, ctx),
        ),
    }
}

pub(crate) enum StmtOutcome {
    Continue,
    Rejected(RejectionReason),
}

// Trace: the per-statement record `propose_with_trace` produces.

/// The outcome of `propose_with_trace`. Like `propose`'s result, but a
/// [`Vec<TraceEntry>`] comes back on **both** paths, so an error (a
/// multi-match `BindOne`, a type mismatch, an unbound actor) keeps the
/// steps that led to it.
///
/// Each statement and invariant check adds one entry; a rejecting
/// `require` / `bind_one` also names its failing sub-expression (see
/// [`RequireOutcome`]).
#[must_use = "a traced proposal carries the outcome (a dropped `Rejected` silently treats a refused change as committed) and the diagnostic trace"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracedProposal {
    /// The transformation reached Accepted or Rejected. `trace` holds
    /// every statement that ran and every invariant that was checked.
    Completed {
        outcome: Outcome,
        trace: Vec<TraceEntry>,
    },
    /// The transformation hit a kernel error (bad arguments, an evaluator
    /// failure, a multi-match `BindOne`). `trace` holds every statement
    /// that ran before it.
    Errored {
        error: EvalError,
        trace: Vec<TraceEntry>,
    },
}

/// One step in the trace: one entry per statement and per invariant
/// check. A `For` nests a sub-trace per iteration.
///
/// Expressions are rendered with [`crate::format::format_prop_inline`];
/// their exact text is not pinned. The serde shape is what the CLI's
/// `--trace` flag emits, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEntry {
    Require {
        expression: String,
        /// The gate's name, when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        outcome: RequireOutcome,
    },
    BindOne {
        expression: String,
        /// As for [`TraceEntry::Require`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        outcome: BindOneOutcome,
    },
    Let {
        name: Var,
        value: EvalValue,
    },
    LetNewSubject {
        name: Var,
        subject: EvalValue,
    },
    Assert {
        claim: ClaimInstance,
    },
    /// The claims actually retracted, not a count, so a wildcard that
    /// removes more than expected shows up.
    Retract {
        predicate: PredicateName,
        retracted: Vec<ClaimInstance>,
    },
    Emit {
        intent: IntentInstance,
    },
    For {
        binding: Var,
        iterations: Vec<ForIterationTrace>,
    },
    /// One invariant check and whether it `held`. A failure also yields
    /// `Outcome::Rejected` in the surrounding `TracedProposal`.
    InvariantCheck {
        name: InvariantName,
        expression: String,
        held: bool,
    },
}

/// The trace of one `For` iteration, with the `item` it ran for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForIterationTrace {
    pub item: EvalValue,
    pub trace: Vec<TraceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequireOutcome {
    /// The expression matched. `match_count` is how many bindings
    /// matched; `require` does not keep them (that is `BindOne`'s job).
    Held { match_count: usize },
    Rejected {
        reason: String,
        /// The most specific sub-expression that failed, rendered, when
        /// the kernel can find one. `None` for `Exists`, `Not`, `Or` and
        /// leaf expressions. Only the expression, never prose.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failing_sub_expression: Option<String>,
        /// The positive claims whose absence failed the gate. Empty unless
        /// the gate is a claim, or an `And` that failed on a positive
        /// claim; a present blocker or a comparison leaves it empty. Feeds
        /// `explain`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        directly_missing_claims: Vec<RenderedClaim>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BindOneOutcome {
    /// The expression matched exactly once.
    Bound {
        /// The whole new binding context, sorted by variable. `BindOne`
        /// replaces the context, so this is the full set, not a delta.
        bindings: Vec<WitnessBinding>,
    },
    NoMatch {
        /// As for `RequireOutcome::Rejected.failing_sub_expression`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failing_sub_expression: Option<String>,
        /// As for `RequireOutcome::Rejected.directly_missing_claims`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        directly_missing_claims: Vec<RenderedClaim>,
    },
    MultipleMatches {
        count: usize,
    },
}

/// Where trace entries go: `Off` drops them, `On` appends. Lets traced
/// and untraced proposals share one executor.
pub(crate) enum TraceSink<'a> {
    Off,
    On(&'a mut Vec<TraceEntry>),
}

impl<'a> TraceSink<'a> {
    #[inline]
    fn push(&mut self, entry: TraceEntry) {
        if let TraceSink::On(v) = self {
            v.push(entry);
        }
    }

    #[inline]
    fn is_on(&self) -> bool {
        matches!(self, TraceSink::On(_))
    }
}

/// Propose a transformation against a pre-state. Runs the body, builds
/// the candidate state, and returns Accepted iff every invariant holds
/// over the cases the change could affect. No database, audit or outbox.
///
/// `transition.transformation_name` must match `transformation.name`.
#[cfg(test)]
pub(crate) fn propose(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
    subjects: &mut dyn SubjectSource,
) -> Result<Outcome, EvalError> {
    propose_inner(
        Vocabulary::undeclared(),
        transformation,
        transition,
        pre_state,
        &Admission::of(invariants, definitions),
        &mut TraceSink::Off,
        subjects,
    )
}

/// The declarations an admitted claim and an emitted intent are checked
/// against: each value must be of the kind its field declares. Borrowed
/// from a [`crate::PreparedProgram`], whose lookups it uses, so every way
/// of deciding checks against the same declarations.
#[derive(Clone, Copy)]
pub(crate) struct Vocabulary<'a>(Option<&'a crate::PreparedProgram>);

impl<'a> Vocabulary<'a> {
    pub(crate) fn of(program: &'a crate::PreparedProgram) -> Self {
        Self(Some(program))
    }

    /// No declarations, for kernel tests that run a hand-built body
    /// outside any programme. Nothing outside tests can build one.
    #[cfg(test)]
    pub(crate) fn undeclared() -> Self {
        Self(None)
    }

    fn predicate(self, name: &PredicateName) -> Option<&'a [ArgDecl]> {
        self.0
            .and_then(|p| p.predicate(name))
            .map(|d| d.args.as_slice())
    }

    fn intent(self, name: &IntentName) -> Option<&'a [ArgDecl]> {
        self.0
            .and_then(|p| p.intent(name))
            .map(|d| d.args.as_slice())
    }
}

/// `propose` with a per-statement and per-invariant trace, returned on
/// both the success and the error path.
pub(crate) fn propose_with_trace(
    vocabulary: Vocabulary<'_>,
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
    subjects: &mut dyn SubjectSource,
) -> TracedProposal {
    let mut entries: Vec<TraceEntry> = vec![];
    let result = {
        let mut sink = TraceSink::On(&mut entries);
        propose_inner(
            vocabulary,
            transformation,
            transition,
            pre_state,
            &Admission::of(invariants, definitions),
            &mut sink,
            subjects,
        )
    };
    match result {
        Ok(outcome) => TracedProposal::Completed {
            outcome,
            trace: entries,
        },
        Err(error) => TracedProposal::Errored {
            error,
            trace: entries,
        },
    }
}

/// The executor behind `propose` and `propose_with_trace`.
pub(crate) fn propose_inner(
    vocabulary: Vocabulary<'_>,
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    admission: &Admission<'_>,
    trace: &mut TraceSink<'_>,
    subjects: &mut dyn SubjectSource,
) -> Result<Outcome, EvalError> {
    let staged = stage_delta_inner(
        vocabulary,
        transformation,
        transition,
        pre_state,
        admission.definitions(),
        trace,
        subjects,
    )?;
    finish_staged_inner(staged, pre_state, admission, None, trace)
}

/// [`propose`] with impact plans built ahead of time (see
/// [`crate::PreparedProgram::admission`]), so nothing is planned per call.
pub(crate) fn propose_with(
    vocabulary: Vocabulary<'_>,
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    admission: &Admission<'_>,
    subjects: &mut dyn SubjectSource,
) -> Result<Outcome, EvalError> {
    let mut trace = TraceSink::Off;
    let staged = stage_delta_inner(
        vocabulary,
        transformation,
        transition,
        pre_state,
        admission.definitions(),
        &mut trace,
        subjects,
    )?;
    finish_staged_inner(staged, pre_state, admission, None, &mut trace)
}

/// Where `new Subject()` gets its subjects: an input to execution, like
/// the state and the proposal. The kernel neither makes nor inspects
/// identifiers; the PostgreSQL and CLI runtimes supply UUIDv7.
///
/// One subject is drawn each time execution reaches `new Subject()`: in
/// statement order, a `for` body once per element in collection order,
/// nested bodies depth-first, and nothing after execution stops. So the
/// same state, proposal and sequence of subjects give one result, trace
/// included.
///
/// The caller owes fresh subjects: none already in the state or drawn
/// before, unless it is replaying a recorded execution on purpose.
///
/// Any iterator of subjects is a source, so a recorded sequence is
/// `ids.into_iter()` and a stream is `std::iter::repeat_with(..)`.
pub trait SubjectSource {
    /// The next subject, or `None` when the source has run out.
    fn next_subject(&mut self) -> Option<Subject>;
}

impl<I: Iterator<Item = Subject>> SubjectSource for I {
    fn next_subject(&mut self) -> Option<Subject> {
        self.next()
    }
}

/// A transformation body's result before any invariant is checked: a
/// gate rejection, or the claims it would admit and retract and the
/// intents it would emit. Lets an adapter run a body once and then check
/// the invariants its own way.
///
/// Staging checks each admitted and emitted value against its declared
/// kind. A delta built by hand skips that check, so a caller who builds
/// one vouches that every value fits its field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedDelta {
    Rejected {
        reason: RejectionReason,
    },
    Staged {
        asserted: Vec<ClaimInstance>,
        retracted: Vec<ClaimInstance>,
        emitted: Vec<IntentInstance>,
    },
}

/// Run only the transformation body, stopping before the invariants.
/// `propose` is exactly this followed by [`finish_staged_delta_with`].
pub(crate) fn propose_stage_delta(
    vocabulary: Vocabulary<'_>,
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    definitions: &[Definition],
    subjects: &mut dyn SubjectSource,
) -> Result<StagedDelta, EvalError> {
    stage_delta_inner(
        vocabulary,
        transformation,
        transition,
        pre_state,
        definitions,
        &mut TraceSink::Off,
        subjects,
    )
}

/// Evaluate the invariants over the candidate state a staged delta
/// implies, completing what [`crate::PreparedProgram::stage_delta`] began, under rules
/// whose impact plans were built once. A staged rejection passes through
/// unchanged. The delta's values are not checked against their declared
/// kinds here; staging did that, and a hand-built delta is trusted.
pub fn finish_staged_delta_with(
    staged: StagedDelta,
    pre_state: &State,
    admission: &Admission<'_>,
) -> Result<Outcome, EvalError> {
    finish_staged_inner(staged, pre_state, admission, None, &mut TraceSink::Off)
}

pub(crate) fn stage_delta_inner(
    vocabulary: Vocabulary<'_>,
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    definitions: &[Definition],
    trace: &mut TraceSink<'_>,
    subjects: &mut dyn SubjectSource,
) -> Result<StagedDelta, EvalError> {
    if transformation.name != transition.transformation_name {
        return Err(EvalError::TypeMismatch(format!(
            "transition names transformation `{}` but Transformation passed is `{}`",
            transition.transformation_name, transformation.name,
        )));
    }
    if transition.args.len() != transformation.parameters.len() {
        return Err(EvalError::TypeMismatch(format!(
            "transformation `{}` expects {} args, got {}",
            transformation.name,
            transformation.parameters.len(),
            transition.args.len(),
        )));
    }
    // No parameter can be declared as a calendar span, so one here is a
    // caller error. Refuse it before it reaches a claim through an `Any`
    // position or a collection element.
    if transition
        .args
        .iter()
        .any(EvalValue::contains_calendar_span)
    {
        return Err(EvalError::TypeMismatch(format!(
            "transformation `{}` cannot take a calendar span argument: a span \
             shifts a date inside an expression and is never itself a governed value",
            transformation.name,
        )));
    }

    let mut bindings = Bindings::new();
    for (name, val) in transformation
        .parameters
        .iter()
        .zip(transition.args.iter().cloned())
    {
        bindings.insert(name.clone(), Bound::from(val));
    }

    let mut asserted: Vec<ClaimInstance> = vec![];
    let mut retracted: Vec<ClaimInstance> = vec![];
    let mut emitted: Vec<IntentInstance> = vec![];

    let given = Some(transition);
    let definition_table = DefinitionTable::new(definitions);
    for stmt in &transformation.body {
        match execute_stmt(
            vocabulary,
            stmt,
            pre_state,
            &mut bindings,
            given,
            definition_table,
            &mut asserted,
            &mut retracted,
            &mut emitted,
            trace,
            subjects,
        )? {
            StmtOutcome::Continue => {}
            StmtOutcome::Rejected(reason) => return Ok(StagedDelta::Rejected { reason }),
        }
    }

    Ok(StagedDelta::Staged {
        asserted,
        retracted,
        emitted,
    })
}

/// A proposal decided: refused, or admitted with its change and the
/// effective delta the admission was judged on. No candidate state is
/// built; a caller that wants one proposes instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decided {
    Accepted {
        asserted_claims: Vec<ClaimInstance>,
        retracted_claims: Vec<ClaimInstance>,
        emitted_intents: Vec<IntentInstance>,
        effective: EffectiveDelta,
    },
    Rejected {
        reason: RejectionReason,
    },
}

/// The invariants over the cases an effective change could affect, read
/// through `view`: the candidate as the caller holds it, owned or a view
/// over the pre-state and the delta. The first refusal, or none.
fn judge_invariants(
    view: StateRef<'_>,
    pre_state: &State,
    admission: &Admission<'_>,
    effective: &EffectiveDelta,
    trace: &mut TraceSink<'_>,
) -> Result<Option<RejectionReason>, EvalError> {
    let definitions = admission.definitions();
    for (inv, plan) in admission.invariants().iter().zip(admission.plans()) {
        // Check only the cases the change could affect.
        let cases = match plan.classify(&effective.asserted, &effective.retracted) {
            Impact::Untouched => continue,
            Impact::Unbounded => None,
            Impact::Bounded(cases) => Some(cases),
        };
        let held = match &cases {
            None => eval_invariant_over(inv, view, Some(pre_state), definitions)?,
            Some(cases) => crate::derive::eval_invariant_cases_over(
                inv,
                view,
                Some(pre_state),
                definitions,
                cases,
            )?,
        };
        if trace.is_on() {
            trace.push(TraceEntry::InvariantCheck {
                name: inv.name.clone(),
                expression: format::format_prop_inline(&inv.body),
                held,
            });
        }
        if !held {
            // Found only on refusal, and only among the checked cases, so
            // it never blames a case the transition did not touch.
            let diagnosis = match &cases {
                None => {
                    crate::derive::invariant_witness_over(inv, view, Some(pre_state), definitions)?
                }
                Some(cases) => crate::derive::invariant_witness_cases_over(
                    inv,
                    view,
                    Some(pre_state),
                    definitions,
                    cases,
                )?,
            };
            return Ok(Some(RejectionReason::Invariant {
                name: inv.name.clone(),
                version: inv.version,
                witness: diagnosis.witness,
                compared: diagnosis.compared,
            }));
        }
    }

    Ok(None)
}

/// [`finish_staged_delta_with`] without the candidate state: the verdict
/// and the effective delta. For a measurement that times the decision
/// apart from the state's advance; an embedder proposes.
#[cfg(any(test, feature = "measure"))]
pub fn decide_staged_delta_with(
    staged: StagedDelta,
    pre_state: &State,
    admission: &Admission<'_>,
) -> Result<Decided, EvalError> {
    decide_staged(staged, pre_state, admission, None, &mut TraceSink::Off)
}

/// Judge a staged delta without building its candidate: the invariants
/// read a view of the pre-state with the delta over it, so nothing of
/// the pre-state is copied. For a caller that advances its own state
/// afterwards, as a rehearsal does.
pub(crate) fn decide_staged(
    staged: StagedDelta,
    pre_state: &State,
    admission: &Admission<'_>,
    established: Option<&EffectiveDelta>,
    trace: &mut TraceSink<'_>,
) -> Result<Decided, EvalError> {
    let (asserted, retracted, emitted) = match staged {
        StagedDelta::Rejected { reason } => return Ok(Decided::Rejected { reason }),
        StagedDelta::Staged {
            asserted,
            retracted,
            emitted,
        } => (asserted, retracted, emitted),
    };
    let effective = match established {
        Some(effective) => effective.clone(),
        None => effective_delta(pre_state, &asserted, &retracted),
    };
    let candidate = Candidate::new(pre_state, &asserted, &retracted);
    let view = StateRef::Candidate(&candidate);
    if let Some(reason) = judge_invariants(view, pre_state, admission, &effective, trace)? {
        return Ok(Decided::Rejected { reason });
    }
    Ok(Decided::Accepted {
        asserted_claims: asserted,
        retracted_claims: retracted,
        emitted_intents: emitted,
        effective,
    })
}

/// Judge a staged delta and return the owned candidate the outcome
/// carries: the candidate is built once and the invariants read it.
pub(crate) fn finish_staged_inner(
    staged: StagedDelta,
    pre_state: &State,
    admission: &Admission<'_>,
    established: Option<&EffectiveDelta>,
    trace: &mut TraceSink<'_>,
) -> Result<Outcome, EvalError> {
    let (asserted, retracted, emitted) = match staged {
        StagedDelta::Rejected { reason } => return Ok(Outcome::Rejected { reason }),
        StagedDelta::Staged {
            asserted,
            retracted,
            emitted,
        } => (asserted, retracted, emitted),
    };
    let candidate = pre_state.with_delta(&asserted, &retracted);
    let computed;
    let effective = match established {
        Some(effective) => effective,
        None => {
            computed = effective_delta(pre_state, &asserted, &retracted);
            &computed
        }
    };
    if let Some(reason) = judge_invariants(
        StateRef::Owned(&candidate),
        pre_state,
        admission,
        effective,
        trace,
    )? {
        return Ok(Outcome::Rejected { reason });
    }
    Ok(Outcome::Accepted {
        asserted_claims: asserted,
        retracted_claims: retracted,
        emitted_intents: emitted,
        candidate_state: candidate,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_stmt(
    vocabulary: Vocabulary<'_>,
    stmt: &Stmt,
    pre_state: &State,
    bindings: &mut Bindings,
    transition: Option<&Transition>,
    definitions: DefinitionTable<'_>,
    asserted: &mut Vec<ClaimInstance>,
    retracted: &mut Vec<ClaimInstance>,
    emitted: &mut Vec<IntentInstance>,
    trace: &mut TraceSink<'_>,
    subjects: &mut dyn SubjectSource,
) -> Result<StmtOutcome, EvalError> {
    match stmt {
        Stmt::Require { prop: expr, name } => {
            // A body reads only the pre-state, so `pre(...)` here is
            // `PreStateUnavailable`.
            let ctx = EvalContext::new(pre_state, None, bindings, transition, definitions);
            let matches = find_matches(expr, &ctx)?;
            if matches.is_empty() {
                let rendered = format::format_prop_inline(expr);
                // One descent names the failing part and the bindings it
                // failed under, so the trace and the witness agree.
                let (failing, witness, compared) = gate_failure(expr, &ctx);
                let reason = RejectionReason::Require {
                    name: name.clone(),
                    rendered: rendered.clone(),
                    witness,
                    compared,
                };
                if trace.is_on() {
                    let directly_missing_claims = unsatisfied_positive_claims(expr, &ctx);
                    trace.push(TraceEntry::Require {
                        expression: rendered,
                        name: name.as_ref().map(ToString::to_string),
                        outcome: RequireOutcome::Rejected {
                            reason: reason.to_string(),
                            failing_sub_expression: failing,
                            directly_missing_claims,
                        },
                    });
                }
                Ok(StmtOutcome::Rejected(reason))
            } else {
                if trace.is_on() {
                    trace.push(TraceEntry::Require {
                        expression: format::format_prop_inline(expr),
                        name: name.as_ref().map(ToString::to_string),
                        outcome: RequireOutcome::Held {
                            match_count: matches.len(),
                        },
                    });
                }
                Ok(StmtOutcome::Continue)
            }
        }
        Stmt::BindOne { prop: expr, name } => {
            // A unique match *replaces* the binding context rather than
            // extending it.
            let ctx = EvalContext::new(pre_state, None, bindings, transition, definitions);
            let mut matches = find_matches(expr, &ctx)?;
            match matches.len() {
                0 => {
                    let rendered = format::format_prop_inline(expr);
                    let (failing, witness, compared) = gate_failure(expr, &ctx);
                    if trace.is_on() {
                        let directly_missing_claims = unsatisfied_positive_claims(expr, &ctx);
                        trace.push(TraceEntry::BindOne {
                            expression: rendered.clone(),
                            name: name.as_ref().map(ToString::to_string),
                            outcome: BindOneOutcome::NoMatch {
                                failing_sub_expression: failing,
                                directly_missing_claims,
                            },
                        });
                    }
                    Ok(StmtOutcome::Rejected(RejectionReason::BindNone {
                        name: name.clone(),
                        rendered,
                        witness,
                        compared,
                    }))
                }
                1 => {
                    let new_bindings = matches.swap_remove(0);
                    if trace.is_on() {
                        let mut sorted: Vec<WitnessBinding> = new_bindings
                            .iter()
                            .map(|(k, v)| WitnessBinding {
                                var: k.clone(),
                                value: EvalValue::clone(v),
                            })
                            .collect();
                        sorted.sort_by(|a, b| a.var.cmp(&b.var));
                        trace.push(TraceEntry::BindOne {
                            expression: format::format_prop_inline(expr),
                            name: name.as_ref().map(ToString::to_string),
                            outcome: BindOneOutcome::Bound { bindings: sorted },
                        });
                    }
                    *bindings = new_bindings;
                    Ok(StmtOutcome::Continue)
                }
                n => {
                    let rendered = format::format_prop_inline(expr);
                    let err_msg = format!(
                        "bind_one matched {n} candidates; expected exactly one: {rendered}"
                    );
                    if trace.is_on() {
                        trace.push(TraceEntry::BindOne {
                            expression: rendered,
                            name: name.as_ref().map(ToString::to_string),
                            outcome: BindOneOutcome::MultipleMatches { count: n },
                        });
                    }
                    Err(EvalError::TypeMismatch(err_msg))
                }
            }
        }
        Stmt::Let { name, value } => {
            let ctx = EvalContext::new(pre_state, None, bindings, transition, definitions);
            let v = eval_value(value, &ctx)?;
            if trace.is_on() {
                trace.push(TraceEntry::Let {
                    name: name.clone(),
                    value: v.clone(),
                });
            }
            bindings.insert(name.clone(), Bound::from(v));
            Ok(StmtOutcome::Continue)
        }
        Stmt::LetNewSubject { name } => {
            let subject = EvalValue::Subject(
                subjects
                    .next_subject()
                    .ok_or(EvalError::SubjectSourceExhausted)?,
            );
            if trace.is_on() {
                trace.push(TraceEntry::LetNewSubject {
                    name: name.clone(),
                    subject: subject.clone(),
                });
            }
            bindings.insert(name.clone(), Bound::from(subject));
            Ok(StmtOutcome::Continue)
        }
        Stmt::Assert(claim) => {
            let instance = resolve_claim(vocabulary, claim, bindings, transition)?;
            if trace.is_on() {
                trace.push(TraceEntry::Assert {
                    claim: instance.clone(),
                });
            }
            asserted.push(instance);
            Ok(StmtOutcome::Continue)
        }
        Stmt::Retract { predicate, args } => {
            let ctx = EvalContext::new(pre_state, None, bindings, transition, definitions);
            let matched = matching_claims(predicate, args, &ctx)?;
            if trace.is_on() {
                trace.push(TraceEntry::Retract {
                    predicate: predicate.clone(),
                    retracted: matched.clone(),
                });
            }
            retracted.extend(matched);
            Ok(StmtOutcome::Continue)
        }
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            let coll_ctx = EvalContext::new(pre_state, None, bindings, transition, definitions);
            let coll_val = eval_value(collection, &coll_ctx)?;
            let EvalValue::Collection(items) = coll_val else {
                return Err(EvalError::TypeMismatch("For expects a collection".into()));
            };
            // Each iteration starts from the outer bindings, restored on
            // exit. Split on tracing so the untraced path allocates
            // nothing per iteration.
            let outer = bindings.clone();
            if trace.is_on() {
                let mut iterations: Vec<ForIterationTrace> = vec![];
                for item in items {
                    crate::state::tally::binding_copy(outer.len());
                    bindings.clone_from(&outer);
                    let item_for_trace = item.clone();
                    bindings.insert(binding.clone(), Bound::from(item));
                    let mut iter_entries: Vec<TraceEntry> = vec![];
                    // The block ends iter_sink's borrow before
                    // iter_entries moves.
                    let iter_result: Result<Option<RejectionReason>, EvalError> = 'inner: {
                        let mut iter_sink = TraceSink::On(&mut iter_entries);
                        for inner in body {
                            match execute_stmt(
                                vocabulary,
                                inner,
                                pre_state,
                                bindings,
                                transition,
                                definitions,
                                asserted,
                                retracted,
                                emitted,
                                &mut iter_sink,
                                subjects,
                            ) {
                                Ok(StmtOutcome::Continue) => {}
                                Ok(StmtOutcome::Rejected(r)) => break 'inner Ok(Some(r)),
                                Err(e) => break 'inner Err(e),
                            }
                        }
                        Ok(None)
                    };
                    match iter_result {
                        Err(e) => {
                            iterations.push(ForIterationTrace {
                                item: item_for_trace,
                                trace: iter_entries,
                            });
                            trace.push(TraceEntry::For {
                                binding: binding.clone(),
                                iterations,
                            });
                            *bindings = outer;
                            return Err(e);
                        }
                        Ok(maybe_rejected) => {
                            iterations.push(ForIterationTrace {
                                item: item_for_trace,
                                trace: iter_entries,
                            });
                            if let Some(r) = maybe_rejected {
                                trace.push(TraceEntry::For {
                                    binding: binding.clone(),
                                    iterations,
                                });
                                *bindings = outer;
                                return Ok(StmtOutcome::Rejected(r));
                            }
                        }
                    }
                }
                *bindings = outer;
                trace.push(TraceEntry::For {
                    binding: binding.clone(),
                    iterations,
                });
                Ok(StmtOutcome::Continue)
            } else {
                let mut off = TraceSink::Off;
                for item in items {
                    crate::state::tally::binding_copy(outer.len());
                    bindings.clone_from(&outer);
                    bindings.insert(binding.clone(), Bound::from(item));
                    for inner in body {
                        match execute_stmt(
                            vocabulary,
                            inner,
                            pre_state,
                            bindings,
                            transition,
                            definitions,
                            asserted,
                            retracted,
                            emitted,
                            &mut off,
                            subjects,
                        )? {
                            StmtOutcome::Continue => {}
                            StmtOutcome::Rejected(r) => {
                                *bindings = outer;
                                return Ok(StmtOutcome::Rejected(r));
                            }
                        }
                    }
                }
                *bindings = outer;
                Ok(StmtOutcome::Continue)
            }
        }
        Stmt::Emit(intent) => {
            let instance = resolve_intent(vocabulary, intent, bindings, transition)?;
            if trace.is_on() {
                trace.push(TraceEntry::Emit {
                    intent: instance.clone(),
                });
            }
            emitted.push(instance);
            Ok(StmtOutcome::Continue)
        }
    }
}

pub(crate) fn resolve_claim(
    vocabulary: Vocabulary<'_>,
    claim: &Claim,
    bindings: &Bindings,
    transition: Option<&Transition>,
) -> Result<ClaimInstance, EvalError> {
    let declared = vocabulary.predicate(&claim.predicate);
    let mut args = Vec::with_capacity(claim.args.len());
    for (i, t) in claim.args.iter().enumerate() {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in assert".into(),
            ));
        }
        let value = resolve_term(t, bindings, transition)?;
        if value.contains_calendar_span() {
            return Err(EvalError::TypeMismatch(format!(
                "a calendar span cannot be admitted into claim `{}`: it shifts a \
                 date inside an expression and is never itself a governed value",
                claim.predicate
            )));
        }
        if let Some(field) = declared.and_then(|d| d.get(i)) {
            let found = value.kind();
            if !field.kind.holds(&found) {
                return Err(EvalError::ClaimKindMismatch {
                    predicate: claim.predicate.to_string(),
                    field: field.name.clone(),
                    declared: field.kind.clone(),
                    found,
                });
            }
        }
        args.push(value);
    }
    Ok(ClaimInstance {
        predicate: claim.predicate.clone(),
        args,
    })
}

pub(crate) fn resolve_intent(
    vocabulary: Vocabulary<'_>,
    intent: &Intent,
    bindings: &Bindings,
    transition: Option<&Transition>,
) -> Result<IntentInstance, EvalError> {
    let declared = vocabulary.intent(&intent.name);
    let mut args = Vec::with_capacity(intent.args.len());
    for (i, t) in intent.args.iter().enumerate() {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in emit".into(),
            ));
        }
        let value = resolve_term(t, bindings, transition)?;
        if value.contains_calendar_span() {
            return Err(EvalError::TypeMismatch(format!(
                "a calendar span cannot be emitted in intent `{}`: it shifts a \
                 date inside an expression and is never itself a governed value",
                intent.name
            )));
        }
        if let Some(field) = declared.and_then(|d| d.get(i)) {
            let found = value.kind();
            if !field.kind.holds(&found) {
                return Err(EvalError::IntentKindMismatch {
                    intent: intent.name.to_string(),
                    field: field.name.clone(),
                    declared: field.kind.clone(),
                    found,
                });
            }
        }
        args.push(value);
    }
    Ok(IntentInstance {
        name: intent.name.clone(),
        args,
    })
}
