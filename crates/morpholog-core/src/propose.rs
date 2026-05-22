//! Transformation execution: the `propose` API, the `propose_with_trace`
//! diagnostic twin, and the supporting types they return.
//!
//! `propose` is the kernel's central entry point: it takes a
//! transformation, a transition (the actor + arguments under which it's
//! being proposed), a pre-state, and the active invariants, and returns
//! an `Outcome`. `propose_with_trace` adds structured per-statement
//! tracing alongside the outcome.
//!
//! Both share a single internal executor (`propose_inner` +
//! `execute_stmt`) via a `TraceSink` enum. The non-trace path allocates
//! no trace storage; per-statement work is a single-variant enum match
//! the optimiser collapses.

use serde::{Deserialize, Serialize};

use crate::derive::eval_invariant;
use crate::eval::{
    EvalError, eval_value, find_failing_subexpr, find_matches, resolve_term, unify_args,
};
use crate::format;
use crate::ir::{Claim, Intent, Invariant, Stmt, Term, Transformation};
use crate::state::{Bindings, ClaimInstance, EvalValue, IntentInstance, State};

/// A proposed state transition under proposed context.
///
/// A `Transition` is the value evaluated, accepted-or-rejected, and
/// persisted to the audit log on acceptance. It bundles three things:
///
/// - `transformation_name`: which named transformation is being proposed.
///   Must match the `name` of the [`Transformation`] passed to [`propose`].
/// - `args`: the per-call arguments to that transformation, positional,
///   matching the transformation's declared `parameters`.
/// - `actor`: the [`EvalValue::Subject`] under whose authority the
///   transition is being proposed. Carried as transition context, not
///   as a transformation parameter, so domain payloads stay free of
///   plumbing concerns.
///
/// The actor is plumbed through `propose` and persisted with the audit
/// row from this PR forward; admission rules that consult the actor
/// (authority checks) arrive in a later PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub transformation_name: String,
    pub args: Vec<EvalValue>,
    pub actor: EvalValue,
}

/// The result of proposing a transformation. Either the candidate state is
/// admissible (Accepted) or some predicate or invariant rejected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Accepted {
        asserted_claims: Vec<ClaimInstance>,
        retracted_claims: Vec<ClaimInstance>,
        emitted_intents: Vec<IntentInstance>,
        candidate_state: State,
    },
    Rejected {
        reason: String,
    },
}

pub(crate) enum StmtOutcome {
    Continue,
    Rejected(String),
}

// ===========================================================================
// Trace: per-statement diagnostic record produced by `propose_with_trace`
// ===========================================================================

/// Structured outcome of `propose_with_trace`. Mirrors `propose`'s
/// success/error split but carries a [`Vec<TraceEntry>`] on **both**
/// paths so that the worst debugging cases (multi-match `BindOne`,
/// type-mismatch `DateLe`, multi-match `ValueOf`, unbound actor) do
/// not silently discard the run-up that led to the failure.
///
/// Scope (v0): trace is **statement-level plus failure-walk on
/// rejection paths**. Each transformation statement and invariant
/// check produces one entry. When a `require` or `bind_one`
/// rejects, the entry's outcome carries a
/// `failing_sub_expression: Option<String>` field identifying the
/// most specific sub-expression responsible (e.g. the failing
/// conjunct of an `And`, or the body of a `Forall` that failed at
/// some iteration). Success paths drill no further than statement
/// level; a full structural ExprTrace mirroring the evaluator is
/// deferred until a worked example forces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracedProposal {
    /// The transformation ran to a normal outcome (Accepted or
    /// Rejected). `trace` contains every statement that ran plus
    /// every invariant that was checked.
    Completed {
        outcome: Outcome,
        trace: Vec<TraceEntry>,
    },
    /// The transformation surfaced a kernel-level error (bad
    /// arguments, evaluator failure, multi-match `BindOne`, etc.).
    /// `trace` contains every statement that ran before the error
    /// was raised - exactly the diagnostic surface that the
    /// `Result<_, EvalError>` shape would drop.
    Errored {
        error: EvalError,
        trace: Vec<TraceEntry>,
    },
}

/// One step in the trace produced by `propose_with_trace`. There is
/// one entry per statement and one per invariant check. `For` is
/// nested: its `iterations` carry a sub-trace per loop iteration.
///
/// Every variant that records an expression renders it via
/// [`crate::format::format_expr_inline`] for human-readable diagnostic
/// output; the exact string format is intentionally not pinned by
/// type so future formatter improvements (PR A's territory) propagate
/// here automatically.
///
/// Serde derives carry the wire format the CLI's `--trace` flag
/// emits. The enum uses an internally-tagged shape
/// (`{ "kind": "...", ... }`) so each entry is distinguishable in a
/// flat JSON array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEntry {
    Require {
        expression: String,
        outcome: RequireOutcome,
    },
    BindOne {
        expression: String,
        outcome: BindOneOutcome,
    },
    Let {
        name: String,
        value: EvalValue,
    },
    LetNewSubject {
        name: String,
        subject: EvalValue,
    },
    Assert {
        claim: ClaimInstance,
    },
    /// Retraction trace carries the **actual retracted claims**, not
    /// just a count. Retraction is exactly where debugging gets hard:
    /// a wildcard retract that takes out three claims when you
    /// expected one is invisible if only the count is recorded.
    Retract {
        predicate: String,
        retracted: Vec<ClaimInstance>,
    },
    Emit {
        intent: IntentInstance,
    },
    For {
        binding: String,
        iterations: Vec<ForIterationTrace>,
    },
    /// One invariant check. The expression string lets the trace
    /// show which invariant body was evaluated; `held` records the
    /// outcome. A failing invariant produces this entry plus an
    /// `Outcome::Rejected` in the surrounding `TracedProposal`.
    InvariantCheck {
        name: String,
        expression: String,
        held: bool,
    },
}

/// One iteration's worth of trace inside a `For` statement. The
/// `item` value lets a caller identify which iteration produced
/// which sub-trace - without it, a failing third iteration is hard
/// to attribute to the right collection element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForIterationTrace {
    pub item: EvalValue,
    pub trace: Vec<TraceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequireOutcome {
    /// The require's expression admitted at least one matching
    /// binding extension. `match_count` records the cardinality of
    /// `find_matches`'s return; the require does not export these
    /// bindings (that is `BindOne`'s job), but the count helps
    /// explain downstream behaviour.
    Held { match_count: usize },
    Rejected {
        reason: String,
        /// The most specific sub-expression responsible for the
        /// rejection, rendered via `format_expr_inline`, when the
        /// kernel can identify one. Populated on failure paths only:
        ///
        /// - `And` failures point at the first failing conjunct (and
        ///   recursively into it if compound).
        /// - `Implies` failures (left held, right rejected) point at
        ///   the right side.
        /// - `Forall` failures point at the body where some binding
        ///   from the source caused it to reject. Binding values are
        ///   not substituted into the rendered string in v0; the
        ///   caller correlates separately.
        ///
        /// `None` when no more specific sub-expression usefully
        /// applies: `Exists` failures are structural (no single
        /// sub-expression is "the one"); `Not` failures
        /// describe what *held* rather than what failed; leaf
        /// expressions (`Claim`, `Le`, `DateLe`, `Eq`, `Neq`, `In`,
        /// `Term`, arithmetic, `Sum`, `ValueOf`) are already as
        /// specific as the kernel can be.
        ///
        /// Distinct from `reason` (the human-readable rejection
        /// string `propose` already produces); this field carries
        /// only the rendered expression, never prose. A future
        /// `failure_shape` field could carry structured "what kind
        /// of failure" metadata if a worked example forces it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failing_sub_expression: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BindOneOutcome {
    /// The bind_one's expression matched exactly one binding set.
    /// `bindings` records the **full** new binding context the
    /// matcher returned (sorted by variable name for stable
    /// serialisation). PR B's doctrine is that `BindOne` replaces
    /// the current binding context with the returned set; the
    /// trace records the full set for completeness.
    Bound {
        bindings: Vec<(String, EvalValue)>,
    },
    NoMatch {
        /// The most specific sub-expression responsible for the
        /// failed match, when the kernel can identify one. Same
        /// semantics as `RequireOutcome::Rejected.failing_sub_expression`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failing_sub_expression: Option<String>,
    },
    MultipleMatches {
        count: usize,
    },
}

/// Internal sink used by the shared execution path. `Off` is a
/// no-op; `On(&mut Vec<TraceEntry>)` appends. Keeps the trace path
/// and the non-trace path on one executor without duplicating logic
/// or introducing a separate "traced evaluator" that would drift.
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

/// Propose a transformation against a pre-state. Stages asserts/retracts/
/// intents, builds the candidate state, evaluates every invariant against
/// that candidate state, and returns Accepted iff all invariants hold.
///
/// No PostgreSQL, no audit, no outbox - that's a later concern. This
/// proves the semantic loop: transformation proposes, invariants decide.
///
/// The proposal is given as a [`Transition`], which bundles the
/// transformation name (verified against `transformation.name`), the
/// arguments, and the actor under whose authority the transition is
/// being proposed. The actor is plumbed through from this PR; admission
/// rules that consult it arrive later.
pub fn propose(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
) -> Result<Outcome, EvalError> {
    // Input validation (transformation-name / actor-kind / arg-count
    // matching) lives in `propose_inner` so both `propose` and
    // `propose_with_trace` share a single source of truth and can't
    // drift if one gate is updated.
    propose_inner(
        transformation,
        transition,
        pre_state,
        invariants,
        &mut TraceSink::Off,
    )
}

/// `propose` with structured per-statement and per-invariant trace
/// recording. Returns a [`TracedProposal`] that carries the trace on
/// **both** success and error paths - the worst debugging cases
/// (multi-match `BindOne`, type-mismatch `DateLe`, multi-match
/// `ValueOf`, unbound actor) raise `EvalError`, and dropping the
/// trace at exactly that moment would defeat the purpose of having
/// one.
///
/// Trace scope is statement-level. The trace shows which statement
/// failed, what bindings each statement produced, and which
/// invariant fired - it does **not** drill into expression
/// internals (which conjunct of an `And` was false, which branch of
/// a `Forall` matched). Expression-level tracing is a separate
/// evaluator refactor.
///
/// Both `propose` and `propose_with_trace` share a single execution
/// path internally; the only difference is the `TraceSink` passed
/// to the executor. Performance impact on the non-trace path is
/// zero (the sink is an `Off` no-op).
pub fn propose_with_trace(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
) -> TracedProposal {
    let mut entries: Vec<TraceEntry> = vec![];
    let result = {
        let mut sink = TraceSink::On(&mut entries);
        propose_inner(transformation, transition, pre_state, invariants, &mut sink)
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

/// Shared executor for `propose` and `propose_with_trace`. The
/// `trace` sink is `Off` for the former and `On(&mut Vec)` for the
/// latter; every other line of execution is identical.
pub(crate) fn propose_inner(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
    trace: &mut TraceSink<'_>,
) -> Result<Outcome, EvalError> {
    if transformation.name != transition.transformation_name {
        return Err(EvalError::TypeMismatch(format!(
            "transition names transformation `{}` but Transformation passed is `{}`",
            transition.transformation_name, transformation.name,
        )));
    }
    if !matches!(transition.actor, EvalValue::Subject(_)) {
        return Err(EvalError::TypeMismatch(
            "transition actor must be a subject".to_string(),
        ));
    }
    if transition.args.len() != transformation.parameters.len() {
        return Err(EvalError::TypeMismatch(format!(
            "transformation `{}` expects {} args, got {}",
            transformation.name,
            transformation.parameters.len(),
            transition.args.len(),
        )));
    }

    let mut bindings = Bindings::new();
    for (name, val) in transformation
        .parameters
        .iter()
        .zip(transition.args.iter().cloned())
    {
        bindings.insert(name.clone(), val);
    }

    let mut asserted: Vec<ClaimInstance> = vec![];
    let mut retracted: Vec<ClaimInstance> = vec![];
    let mut emitted: Vec<IntentInstance> = vec![];

    let actor = Some(&transition.actor);
    for stmt in &transformation.body {
        match execute_stmt(
            stmt,
            pre_state,
            &mut bindings,
            actor,
            &mut asserted,
            &mut retracted,
            &mut emitted,
            trace,
        )? {
            StmtOutcome::Continue => {}
            StmtOutcome::Rejected(reason) => return Ok(Outcome::Rejected { reason }),
        }
    }

    let candidate = build_candidate_state(pre_state, &asserted, &retracted);

    for inv in invariants {
        // Pass both pre_state and candidate. Invariants that contain
        // `Expr::Pre` flip into pre-state lookup for the wrapped
        // subtree; invariants that don't are unaffected.
        let held = eval_invariant(inv, &candidate, Some(pre_state))?;
        if trace.is_on() {
            trace.push(TraceEntry::InvariantCheck {
                name: inv.name.clone(),
                expression: format::format_expr_inline(&inv.body),
                held,
            });
        }
        if !held {
            return Ok(Outcome::Rejected {
                reason: format!("invariant `{}` violated", inv.name),
            });
        }
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
    stmt: &Stmt,
    pre_state: &State,
    bindings: &mut Bindings,
    actor: Option<&EvalValue>,
    asserted: &mut Vec<ClaimInstance>,
    retracted: &mut Vec<ClaimInstance>,
    emitted: &mut Vec<IntentInstance>,
    trace: &mut TraceSink<'_>,
) -> Result<StmtOutcome, EvalError> {
    match stmt {
        Stmt::Require(expr) => {
            // Transformation bodies read pre-state as the only state in
            // scope - there is no post to flip back from. `Expr::Pre`
            // inside a `require` therefore surfaces as
            // `EvalError::PreStateUnavailable`. The `None` here is what
            // enforces that doctrine.
            let matches = find_matches(expr, pre_state, None, bindings, actor)?;
            if matches.is_empty() {
                // The rejection path renders the expression for the
                // reason string regardless of tracing (existing
                // behaviour from PR B); reuse the same rendering for
                // the trace entry rather than calling
                // format_expr_inline twice.
                let rendered = format::format_expr_inline(expr);
                let reason = format!("require failed: {rendered} did not hold over pre-state");
                if trace.is_on() {
                    let failing = find_failing_subexpr(expr, pre_state, None, bindings, actor);
                    trace.push(TraceEntry::Require {
                        expression: rendered,
                        outcome: RequireOutcome::Rejected {
                            reason: reason.clone(),
                            failing_sub_expression: failing,
                        },
                    });
                }
                Ok(StmtOutcome::Rejected(reason))
            } else {
                if trace.is_on() {
                    trace.push(TraceEntry::Require {
                        expression: format::format_expr_inline(expr),
                        outcome: RequireOutcome::Held {
                            match_count: matches.len(),
                        },
                    });
                }
                Ok(StmtOutcome::Continue)
            }
        }
        Stmt::BindOne(expr) => {
            // Single-path deterministic unique lookup. See `Stmt::BindOne`
            // rustdoc for the multi-outcome contract. Crucially, on a
            // unique match we *replace* the binding context with the
            // returned match rather than extending.
            //
            // For 0 / N>1 branches, the rejection reason / error
            // message renders the expression regardless of tracing
            // (existing behaviour from PR B); the trace entry reuses
            // that single rendering rather than calling
            // format_expr_inline a second time.
            let mut matches = find_matches(expr, pre_state, None, bindings, actor)?;
            match matches.len() {
                0 => {
                    let rendered = format::format_expr_inline(expr);
                    let reason = format!("bind_one failed: {rendered} matched no candidates");
                    if trace.is_on() {
                        let failing = find_failing_subexpr(expr, pre_state, None, bindings, actor);
                        trace.push(TraceEntry::BindOne {
                            expression: rendered,
                            outcome: BindOneOutcome::NoMatch {
                                failing_sub_expression: failing,
                            },
                        });
                    }
                    Ok(StmtOutcome::Rejected(reason))
                }
                1 => {
                    let new_bindings = matches.swap_remove(0);
                    if trace.is_on() {
                        let mut sorted: Vec<(String, EvalValue)> = new_bindings
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        sorted.sort_by(|a, b| a.0.cmp(&b.0));
                        trace.push(TraceEntry::BindOne {
                            expression: format::format_expr_inline(expr),
                            outcome: BindOneOutcome::Bound { bindings: sorted },
                        });
                    }
                    *bindings = new_bindings;
                    Ok(StmtOutcome::Continue)
                }
                n => {
                    let rendered = format::format_expr_inline(expr);
                    let err_msg = format!(
                        "bind_one matched {n} candidates; expected exactly one: {rendered}"
                    );
                    if trace.is_on() {
                        trace.push(TraceEntry::BindOne {
                            expression: rendered,
                            outcome: BindOneOutcome::MultipleMatches { count: n },
                        });
                    }
                    Err(EvalError::TypeMismatch(err_msg))
                }
            }
        }
        Stmt::Let { name, value } => {
            let v = eval_value(value, pre_state, None, bindings, actor)?;
            if trace.is_on() {
                trace.push(TraceEntry::Let {
                    name: name.clone(),
                    value: v.clone(),
                });
            }
            bindings.insert(name.clone(), v);
            Ok(StmtOutcome::Continue)
        }
        Stmt::LetNewSubject { name } => {
            let id = uuid::Uuid::now_v7().to_string();
            let subject = EvalValue::Subject(id);
            if trace.is_on() {
                trace.push(TraceEntry::LetNewSubject {
                    name: name.clone(),
                    subject: subject.clone(),
                });
            }
            bindings.insert(name.clone(), subject);
            Ok(StmtOutcome::Continue)
        }
        Stmt::Assert(claim) => {
            let instance = resolve_claim(claim, bindings, actor)?;
            if trace.is_on() {
                trace.push(TraceEntry::Assert {
                    claim: instance.clone(),
                });
            }
            asserted.push(instance);
            Ok(StmtOutcome::Continue)
        }
        Stmt::Retract { predicate, args } => {
            // Branch on trace.is_on() to keep the non-trace path
            // streaming clones directly into `retracted` (its
            // pre-PR-D shape). On the trace path, build an
            // intermediate Vec so the trace entry can carry the
            // actual retracted claims rather than a count.
            if trace.is_on() {
                let mut retracted_here: Vec<ClaimInstance> = vec![];
                for claim in pre_state.claims_for(predicate) {
                    if claim.args.len() != args.len() {
                        continue;
                    }
                    if unify_args(args, &claim.args, bindings, actor).is_some() {
                        retracted_here.push(claim.clone());
                    }
                }
                trace.push(TraceEntry::Retract {
                    predicate: predicate.clone(),
                    retracted: retracted_here.clone(),
                });
                retracted.extend(retracted_here);
            } else {
                for claim in pre_state.claims_for(predicate) {
                    if claim.args.len() != args.len() {
                        continue;
                    }
                    if unify_args(args, &claim.args, bindings, actor).is_some() {
                        retracted.push(claim.clone());
                    }
                }
            }
            Ok(StmtOutcome::Continue)
        }
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            let coll_val = eval_value(collection, pre_state, None, bindings, actor)?;
            let items = match coll_val {
                EvalValue::Collection(v) => v,
                _ => return Err(EvalError::TypeMismatch("For expects a collection".into())),
            };
            // Iteration scope (see PR B): snapshot outer bindings,
            // reset per iteration, restore on exit.
            //
            // Branched on `trace.is_on()` to keep the non-trace path
            // tight: no per-iteration `iter_entries` allocation, no
            // `item.clone()` (the value moves directly into bindings),
            // no `iterations` Vec. The trace path opts into all of
            // those for diagnostic completeness.
            let outer = bindings.clone();
            if trace.is_on() {
                let mut iterations: Vec<ForIterationTrace> = vec![];
                for item in items {
                    bindings.clone_from(&outer);
                    let item_for_trace = item.clone();
                    bindings.insert(binding.clone(), item);
                    let mut iter_entries: Vec<TraceEntry> = vec![];
                    // Labeled block scopes the iter_sink so its borrow
                    // on iter_entries ends before we move iter_entries
                    // into ForIterationTrace.
                    let iter_result: Result<Option<String>, EvalError> = 'inner: {
                        let mut iter_sink = TraceSink::On(&mut iter_entries);
                        for inner in body {
                            match execute_stmt(
                                inner,
                                pre_state,
                                bindings,
                                actor,
                                asserted,
                                retracted,
                                emitted,
                                &mut iter_sink,
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
                    bindings.clone_from(&outer);
                    bindings.insert(binding.clone(), item);
                    for inner in body {
                        match execute_stmt(
                            inner, pre_state, bindings, actor, asserted, retracted, emitted,
                            &mut off,
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
            let instance = resolve_intent(intent, bindings, actor)?;
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
    claim: &Claim,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<ClaimInstance, EvalError> {
    let mut args = Vec::with_capacity(claim.args.len());
    for t in &claim.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in assert".into(),
            ));
        }
        args.push(resolve_term(t, bindings, actor)?);
    }
    Ok(ClaimInstance {
        predicate: claim.predicate.clone(),
        args,
    })
}

pub(crate) fn resolve_intent(
    intent: &Intent,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<IntentInstance, EvalError> {
    let mut args = Vec::with_capacity(intent.args.len());
    for t in &intent.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in emit".into(),
            ));
        }
        args.push(resolve_term(t, bindings, actor)?);
    }
    Ok(IntentInstance {
        name: intent.name.clone(),
        args,
    })
}

pub(crate) fn build_candidate_state(
    pre: &State,
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> State {
    let mut claims = pre.claims().to_vec();
    claims.retain(|f| !retracted.iter().any(|r| r == f));
    for a in asserted {
        if !claims.iter().any(|f| f == a) {
            claims.push(a.clone());
        }
    }
    State::from_claims(claims)
}
