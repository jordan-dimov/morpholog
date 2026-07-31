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

use crate::definitions::DefinitionIndex;
use crate::derive::eval_invariant;
use crate::eval::{
    EvalContext, EvalError, RenderedClaim, eval_value, find_failing_subexpr, find_matches,
    matching_claims, resolve_term, unsatisfied_positive_claims,
};
use crate::format;
use crate::ir::{
    Claim, Definition, Intent, Invariant, InvariantName, PredicateName, RuleName, Stmt, Subject,
    Term, Transformation, TransformationName, Var,
};
use crate::state::{Bindings, ClaimInstance, EvalValue, IntentInstance, State};

/// A proposed state transition. Evaluated, accepted-or-rejected, and
/// persisted to the audit log on acceptance. Bundles:
///
/// - `transformation_name`: which named transformation is being proposed.
///   Must match the `name` of the [`Transformation`] passed to [`propose`].
/// - `args`: the per-call positional arguments, matching the
///   transformation's declared `parameters`.
/// - `actor`: the [`Subject`] under whose authority the transition is
///   proposed. Carried as transition context, not a transformation
///   parameter, so domain payloads stay free of plumbing. Persists and
///   renders as a tagged [`EvalValue::Subject`] (see [`crate::actor_repr`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub transformation_name: TransformationName,
    pub args: Vec<EvalValue>,
    #[serde(with = "crate::actor_repr")]
    pub actor: Subject,
}

/// The result of proposing a transformation. Either the candidate state is
/// admissible (Accepted) or some predicate or invariant rejected it.
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
/// The values are the offending ones, so a reader is told *which* subject
/// broke the rule and not only that the rule broke. Carried structurally
/// rather than rendered into the reason string: the reason string is a
/// pinned wire format, and an embedder that wants to show the account it
/// refused should read a value, not parse prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessBinding {
    pub var: Var,
    pub value: EvalValue,
}

/// Why a proposal was rejected, structured at the source. Every consumer
/// that needs prose (envelopes, trace entries, the operational rejection
/// log's `reason` column) renders through [`std::fmt::Display`], whose
/// output is the pinned wire string - consumers that need the rule name
/// or kind match the variant instead of parsing display text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionReason {
    /// An invariant did not hold over the candidate state. Carries the
    /// version checked because the rejecting site is the only place
    /// that knows it; Display deliberately omits it.
    ///
    /// `witness` is the binding assignment the failure was diagnosed
    /// under, empty when no single iteration can be blamed. Display omits
    /// it too - the pinned string is unchanged by this field's presence.
    Invariant {
        name: InvariantName,
        version: u32,
        witness: Vec<WitnessBinding>,
    },
    /// A `require` gate found no witness over the pre-state.
    ///
    /// `name` is the gate's optional identifier. Present, it is what a
    /// caller should hold on to: `rendered` changes the moment anyone
    /// rewords the expression, so it reads well and identifies nothing.
    Require {
        name: Option<RuleName>,
        rendered: String,
    },
    /// A `bind` lookup matched no candidates. (Multi-match is an
    /// [`EvalError`], not a rejection.)
    BindNone {
        name: Option<RuleName>,
        rendered: String,
    },
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectionReason::Invariant { name, .. } => {
                write!(f, "invariant `{name}` violated")
            }
            RejectionReason::Require { name, rendered } => match name {
                Some(n) => write!(
                    f,
                    "require `{n}` failed: {rendered} did not hold over pre-state"
                ),
                None => write!(f, "require failed: {rendered} did not hold over pre-state"),
            },
            RejectionReason::BindNone { name, rendered } => match name {
                Some(n) => write!(f, "bind `{n}` failed: {rendered} matched no candidates"),
                None => write!(f, "bind_one failed: {rendered} matched no candidates"),
            },
        }
    }
}

pub(crate) enum StmtOutcome {
    Continue,
    Rejected(RejectionReason),
}

// ===========================================================================
// Trace: per-statement diagnostic record produced by `propose_with_trace`
// ===========================================================================

/// Structured outcome of `propose_with_trace`. Mirrors `propose`'s
/// success/error split but carries a [`Vec<TraceEntry>`] on **both**
/// paths, so the worst debugging cases (multi-match `BindOne`,
/// type-mismatch `DateLe`, multi-match `ValueOf`, unbound actor) do not
/// silently discard the run-up to the failure.
///
/// Trace is statement-level plus a failure-walk on rejection paths:
/// each statement and invariant check produces one entry, and a
/// rejecting `require`/`bind_one` carries a `failing_sub_expression`
/// (see [`RequireOutcome`]).
#[must_use = "a traced proposal carries the outcome (a dropped `Rejected` silently treats a refused change as committed) and the diagnostic trace"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracedProposal {
    /// The transformation ran to a normal outcome (Accepted or
    /// Rejected). `trace` contains every statement that ran plus
    /// every invariant that was checked.
    Completed {
        outcome: Outcome,
        trace: Vec<TraceEntry>,
    },
    /// The transformation surfaced a kernel-level error (bad arguments,
    /// evaluator failure, multi-match `BindOne`, etc.). `trace` contains
    /// every statement that ran before the error - the surface a plain
    /// `Result<_, EvalError>` would drop.
    Errored {
        error: EvalError,
        trace: Vec<TraceEntry>,
    },
}

/// One step in the trace produced by `propose_with_trace`: one entry
/// per statement and one per invariant check. `For` is nested - its
/// `iterations` carry a sub-trace per loop iteration.
///
/// Variants that record an expression render it via
/// [`crate::format::format_prop_inline`]; the exact string format is
/// not pinned by type, so formatter improvements propagate here.
///
/// Serde derives carry the wire format the CLI's `--trace` flag emits.
/// The internally-tagged shape (`{ "kind": "...", ... }`) keeps each
/// entry distinguishable in a flat JSON array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEntry {
    Require {
        expression: String,
        /// The gate's name, when it has one - so a trace assertion can hold
        /// an identifier the author chose instead of a statement position.
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
    /// Carries the **actual retracted claims**, not just a count: a
    /// wildcard retract that takes out more than expected is invisible
    /// if only the count is recorded.
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
    /// One invariant check. The expression string lets the trace
    /// show which invariant body was evaluated; `held` records the
    /// outcome. A failing invariant produces this entry plus an
    /// `Outcome::Rejected` in the surrounding `TracedProposal`.
    InvariantCheck {
        name: InvariantName,
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
    /// The require's expression admitted at least one matching binding
    /// extension. `match_count` is the cardinality of `find_matches`'s
    /// return; `require` does not export these bindings (that is
    /// `BindOne`'s job), but the count explains downstream behaviour.
    Held { match_count: usize },
    Rejected {
        reason: String,
        /// The most specific sub-expression responsible for the
        /// rejection, rendered via `format_prop_inline`, when the kernel
        /// can identify one (see [`crate::EvalError`] and the
        /// `find_failing_subexpr` drill-down rules). `None` for `Exists`,
        /// `Not`, `Or`, and leaf expressions. Carries only the rendered
        /// expression, never prose - distinct from `reason`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failing_sub_expression: Option<String>,
        /// The positive claim conjuncts directly responsible for the
        /// rejection, structurally (see
        /// [`crate::eval::RenderedClaim`] and
        /// `unsatisfied_positive_claims`). Empty unless the gate is a
        /// top-level claim or an `And` whose chain-killing conjunct is a
        /// positive claim - so present blockers and comparator failures
        /// carry nothing here. Feeds the explanation engine's
        /// directly-missing-claims list.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        directly_missing_claims: Vec<RenderedClaim>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BindOneOutcome {
    /// The bind_one's expression matched exactly one binding set.
    /// `bindings` records the **full** new binding context the matcher
    /// returned (sorted by variable name for stable serialisation):
    /// `BindOne` replaces the current context with the returned set, so
    /// the trace records the full set, not a delta.
    Bound {
        /// The binding set the lookup produced, sorted by variable. Shaped
        /// like a refusal's witness because it is the same idea - a
        /// variable and the value it took - and one vocabulary beats two.
        bindings: Vec<WitnessBinding>,
    },
    NoMatch {
        /// The most specific sub-expression responsible for the
        /// failed match, when the kernel can identify one. Same
        /// semantics as `RequireOutcome::Rejected.failing_sub_expression`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failing_sub_expression: Option<String>,
        /// The positive claim conjuncts directly responsible for the
        /// failed match. Same semantics as
        /// `RequireOutcome::Rejected.directly_missing_claims`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        directly_missing_claims: Vec<RenderedClaim>,
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

/// Propose a transformation against a pre-state. Stages
/// asserts/retracts/intents, builds the candidate state, evaluates every
/// invariant against it, and returns Accepted iff all invariants hold.
/// No PostgreSQL, audit, or outbox: this is the pure semantic loop.
///
/// The proposal is a [`Transition`] bundling the transformation name
/// (verified against `transformation.name`), the arguments, and the
/// proposing actor.
pub fn propose(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<Outcome, EvalError> {
    // Input validation (transformation-name / arg-count matching) lives
    // in `propose_inner` so both `propose` and `propose_with_trace` share
    // a single source of truth and can't drift if one gate is updated.
    // The actor is a `Subject` by type; no runtime kind check is needed.
    propose_inner(
        transformation,
        transition,
        pre_state,
        invariants,
        definitions,
        &mut TraceSink::Off,
    )
}

/// `propose` with structured per-statement and per-invariant trace
/// recording. Returns a [`TracedProposal`] carrying the trace on both
/// success and error paths.
///
/// Both functions share one execution path; the only difference is the
/// `TraceSink` passed to the executor, so the non-trace path pays
/// nothing (the sink is an `Off` no-op).
pub fn propose_with_trace(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> TracedProposal {
    let mut entries: Vec<TraceEntry> = vec![];
    let result = {
        let mut sink = TraceSink::On(&mut entries);
        propose_inner(
            transformation,
            transition,
            pre_state,
            invariants,
            definitions,
            &mut sink,
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

/// Shared executor for `propose` and `propose_with_trace`. The
/// `trace` sink is `Off` for the former and `On(&mut Vec)` for the
/// latter; every other line of execution is identical.
pub(crate) fn propose_inner(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
    trace: &mut TraceSink<'_>,
) -> Result<Outcome, EvalError> {
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
    // Calendar spans are expression-only. No parameter can be declared
    // to carry one, so a span among the supplied arguments is always a
    // caller error - refused here so it cannot smuggle into a claim
    // through an `Any`-kinded position or a collection element.
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
        bindings.insert(name.clone(), val);
    }

    let mut asserted: Vec<ClaimInstance> = vec![];
    let mut retracted: Vec<ClaimInstance> = vec![];
    let mut emitted: Vec<IntentInstance> = vec![];

    let actor = Some(&transition.actor);
    let definition_index = DefinitionIndex::new(definitions);
    for stmt in &transformation.body {
        match execute_stmt(
            stmt,
            pre_state,
            &mut bindings,
            actor,
            definition_index,
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
        // `Prop::Pre` flip into pre-state lookup for the wrapped
        // subtree; invariants that don't are unaffected.
        let held = eval_invariant(inv, &candidate, Some(pre_state), definitions)?;
        if trace.is_on() {
            trace.push(TraceEntry::InvariantCheck {
                name: inv.name.clone(),
                expression: format::format_prop_inline(&inv.body),
                held,
            });
        }
        if !held {
            // Diagnosed only now: the accepting path never pays for it.
            let witness =
                crate::derive::invariant_witness(inv, &candidate, Some(pre_state), definitions)?;
            return Ok(Outcome::Rejected {
                reason: RejectionReason::Invariant {
                    name: inv.name.clone(),
                    version: inv.version,
                    witness,
                },
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
    actor: Option<&Subject>,
    definitions: DefinitionIndex<'_>,
    asserted: &mut Vec<ClaimInstance>,
    retracted: &mut Vec<ClaimInstance>,
    emitted: &mut Vec<IntentInstance>,
    trace: &mut TraceSink<'_>,
) -> Result<StmtOutcome, EvalError> {
    match stmt {
        Stmt::Require { prop: expr, name } => {
            // Transformation bodies read pre-state as the only state in
            // scope. Passing `None` for pre_state is what makes
            // `Prop::Pre` inside a `require` surface as
            // `EvalError::PreStateUnavailable`.
            let ctx = EvalContext::new(pre_state, None, bindings, actor, definitions);
            let matches = find_matches(expr, &ctx)?;
            if matches.is_empty() {
                // Render once; reused for both the reason and the
                // trace entry.
                let rendered = format::format_prop_inline(expr);
                if trace.is_on() {
                    let failing = find_failing_subexpr(expr, &ctx);
                    let directly_missing_claims = unsatisfied_positive_claims(expr, &ctx);
                    trace.push(TraceEntry::Require {
                        expression: rendered.clone(),
                        name: name.as_ref().map(ToString::to_string),
                        outcome: RequireOutcome::Rejected {
                            reason: RejectionReason::Require {
                                name: name.clone(),
                                rendered: rendered.clone(),
                            }
                            .to_string(),
                            failing_sub_expression: failing,
                            directly_missing_claims,
                        },
                    });
                }
                Ok(StmtOutcome::Rejected(RejectionReason::Require {
                    name: name.clone(),
                    rendered,
                }))
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
            // Deterministic unique lookup (see the `bind_one` rustdoc for
            // the multi-outcome contract). On a unique match we *replace*
            // the binding context with the returned match, not extend.
            // The expression is rendered once per branch and reused for
            // both the reason/error string and the trace entry.
            let ctx = EvalContext::new(pre_state, None, bindings, actor, definitions);
            let mut matches = find_matches(expr, &ctx)?;
            match matches.len() {
                0 => {
                    let rendered = format::format_prop_inline(expr);
                    if trace.is_on() {
                        let failing = find_failing_subexpr(expr, &ctx);
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
                    }))
                }
                1 => {
                    let new_bindings = matches.swap_remove(0);
                    if trace.is_on() {
                        let mut sorted: Vec<WitnessBinding> = new_bindings
                            .iter()
                            .map(|(k, v)| WitnessBinding {
                                var: k.clone(),
                                value: v.clone(),
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
            let ctx = EvalContext::new(pre_state, None, bindings, actor, definitions);
            let v = eval_value(value, &ctx)?;
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
            let subject = EvalValue::Subject(id.into());
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
            // The matched claims are the same set the trace entry needs,
            // so compute them once (indexed by ground args, shared with
            // the read path) and only the trace push is conditional.
            let ctx = EvalContext::new(pre_state, None, bindings, actor, definitions);
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
            let coll_ctx = EvalContext::new(pre_state, None, bindings, actor, definitions);
            let coll_val = eval_value(collection, &coll_ctx)?;
            let EvalValue::Collection(items) = coll_val else {
                return Err(EvalError::TypeMismatch("For expects a collection".into()));
            };
            // Iteration scope: snapshot outer bindings, reset per
            // iteration, restore on exit. Branched on `trace.is_on()` so
            // the non-trace path skips the per-iteration allocations,
            // the `item.clone()`, and the `iterations` Vec that the
            // trace path needs for diagnostic completeness.
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
                    let iter_result: Result<Option<RejectionReason>, EvalError> = 'inner: {
                        let mut iter_sink = TraceSink::On(&mut iter_entries);
                        for inner in body {
                            match execute_stmt(
                                inner,
                                pre_state,
                                bindings,
                                actor,
                                definitions,
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
                            inner,
                            pre_state,
                            bindings,
                            actor,
                            definitions,
                            asserted,
                            retracted,
                            emitted,
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
    actor: Option<&Subject>,
) -> Result<ClaimInstance, EvalError> {
    let mut args = Vec::with_capacity(claim.args.len());
    for t in &claim.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in assert".into(),
            ));
        }
        let value = resolve_term(t, bindings, actor)?;
        if value.contains_calendar_span() {
            return Err(EvalError::TypeMismatch(format!(
                "a calendar span cannot be admitted into claim `{}`: it shifts a \
                 date inside an expression and is never itself a governed value",
                claim.predicate
            )));
        }
        args.push(value);
    }
    Ok(ClaimInstance {
        predicate: claim.predicate.clone(),
        args,
    })
}

pub(crate) fn resolve_intent(
    intent: &Intent,
    bindings: &Bindings,
    actor: Option<&Subject>,
) -> Result<IntentInstance, EvalError> {
    let mut args = Vec::with_capacity(intent.args.len());
    for t in &intent.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in emit".into(),
            ));
        }
        let value = resolve_term(t, bindings, actor)?;
        if value.contains_calendar_span() {
            return Err(EvalError::TypeMismatch(format!(
                "a calendar span cannot be emitted in intent `{}`: it shifts a \
                 date inside an expression and is never itself a governed value",
                intent.name
            )));
        }
        args.push(value);
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
