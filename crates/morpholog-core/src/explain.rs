//! The explanation engine: a deterministic, structured account of why a
//! proposed transition was admitted or rejected.
//!
//! `explain` is a read-side interpretation of the kernel's execution
//! data, not a second evaluator. It runs [`propose_with_trace`] (sharing
//! the one executor), then maps the rejection trace onto a structured
//! [`Explanation`] and attaches candidate suppliers via the static
//! [`transformations_asserting`] walker. There is no new IR primitive, no
//! surface syntax, and no natural-language generation: the words come
//! only from predicate and transformation names plus fixed templates, so
//! an explanation an auditor relies on is reproducible and faithful to
//! the exact failing claim.
//!
//! Scope (v0) is deliberately one-hop. The structured object speaks
//! Morpholog's internal truth - *positive claim-shaped gate conjuncts
//! that did not match under the current binding context* - which is why
//! the field is `directly_missing_claims`, not "missing evidence":
//! some unmatched claims are authority, standing, prior use, or
//! currentness, not evidence in the narrow sense. The renderer may say
//! "evidence" where that helps a reader; the model does not.
//!
//! Out of scope until a later tier (the moment we surface these we are
//! explaining the *semantics* of failure, not formatting the trace):
//! present blockers (`not X` where `X` holds), comparator failures,
//! existential or disjunctive remedies, bounded abduction or repairs,
//! and any claim of minimality. Those rejections render a faithful
//! reason with an empty `directly_missing_claims`.

use serde::{Deserialize, Serialize};

use crate::analysis::transformations_asserting;
use crate::eval::{RenderedClaim, render_eval_value};
use crate::ir::Program;
use crate::propose::{
    BindOneOutcome, Outcome, RequireOutcome, TraceEntry, TracedProposal, Transition,
    propose_with_trace,
};
use crate::state::State;

/// A structured, deterministic explanation of one proposed transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    pub transition: TransitionRef,
    pub verdict: Verdict,
}

/// The transition the explanation is about, with its arguments and actor
/// rendered to short human strings (subjects bare, decimals as text,
/// dates ISO-8601).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRef {
    pub transformation: String,
    pub args: Vec<String>,
    pub actor: String,
}

/// Admissible, or rejected with a structured reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Admissible,
    Rejected(Rejection),
}

/// Why a transition was rejected. A `require` / `bind_one` gate did not
/// hold ([`Rejection::Gate`]); the candidate state would violate an
/// invariant ([`Rejection::Invariant`]); or the kernel raised an error
/// before reaching a verdict ([`Rejection::Error`] - a multi-match
/// `bind_one`, a type mismatch, an unbound actor, an unknown
/// transformation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rejection {
    Gate(GateRejection),
    Invariant(InvariantRejection),
    Error(ErrorRejection),
}

/// A `require` or `bind_one` gate that did not hold. `gate` is the
/// rendered gate expression; `statement_kind` distinguishes the two so
/// the structured object is not purely string-shaped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRejection {
    pub gate: String,
    pub statement_kind: GateKind,
    pub directly_missing_claims: Vec<MissingClaim>,
}

/// Which binding-quartet gate rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Require,
    BindOne,
}

/// The candidate state would violate this invariant. `rule` is the
/// rendered invariant body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantRejection {
    pub name: String,
    pub rule: String,
}

/// The kernel raised an error before reaching a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorRejection {
    pub message: String,
}

/// A positive claim conjunct the failing gate is directly missing under
/// the current bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingClaim {
    pub predicate: String,
    pub rendered: String,
    /// Transformations that assert this predicate. **Candidate by output
    /// predicate only**: this does *not* imply the transformation can
    /// supply this specific claim instance under the current actor,
    /// arguments, dates, or state - it may carry its own `require`
    /// gates, authority, or windows. Honest candidate-supplier lookup,
    /// not instance matching or multi-hop reachability.
    pub candidate_supplier_transformations: Vec<String>,
}

/// Explain why `transition` is admissible or rejected against
/// `pre_state`, using `program`'s transformation, invariants, and
/// vocabulary. Pure and synchronous: it runs the kernel in-memory and
/// derives the explanation from the trace.
pub fn explain(program: &Program, transition: &Transition, pre_state: &State) -> Explanation {
    let transition_ref = TransitionRef {
        transformation: transition.transformation_name.clone(),
        args: transition.args.iter().map(render_eval_value).collect(),
        actor: render_eval_value(&transition.actor),
    };

    let Some(transformation) = program.transformation(&transition.transformation_name) else {
        return Explanation {
            transition: transition_ref,
            verdict: Verdict::Rejected(Rejection::Error(ErrorRejection {
                message: format!(
                    "no transformation named `{}` in program `{}`",
                    transition.transformation_name, program.name
                ),
            })),
        };
    };

    let traced = propose_with_trace(transformation, transition, pre_state, &program.invariants);
    let verdict = match traced {
        TracedProposal::Errored { error, .. } => {
            Verdict::Rejected(Rejection::Error(ErrorRejection {
                message: error.to_string(),
            }))
        }
        TracedProposal::Completed {
            outcome: Outcome::Accepted { .. },
            ..
        } => Verdict::Admissible,
        TracedProposal::Completed {
            outcome: Outcome::Rejected { reason },
            trace,
        } => verdict_from_rejection(program, &reason, &trace),
    };

    Explanation {
        transition: transition_ref,
        verdict,
    }
}

impl Explanation {
    /// Render this explanation as deterministic, claim-shaped prose. The
    /// same `Explanation` renders identically every time; the only inputs
    /// are predicate and transformation names plus fixed templates.
    pub fn render(&self) -> String {
        let head = format!(
            "{}({}) proposed by {}",
            self.transition.transformation,
            self.transition.args.join(", "),
            self.transition.actor,
        );
        let mut out = match &self.verdict {
            Verdict::Admissible => format!("Admissible: {head}"),
            Verdict::Rejected(rejection) => {
                let mut s = format!("Rejected: {head}\n");
                match rejection {
                    Rejection::Gate(gate) => {
                        s.push_str(&format!("\nGate not satisfied:\n  {}\n", gate.gate));
                        if !gate.directly_missing_claims.is_empty() {
                            s.push_str("\nDirectly missing claims:\n");
                            for claim in &gate.directly_missing_claims {
                                s.push_str(&format!("  - {}\n", claim.rendered));
                                if claim.candidate_supplier_transformations.is_empty() {
                                    s.push_str(&format!(
                                        "      (no transformation in this model asserts {})\n",
                                        claim.predicate,
                                    ));
                                } else {
                                    s.push_str("      candidate supplier transformations:\n");
                                    for supplier in &claim.candidate_supplier_transformations {
                                        s.push_str(&format!("        - {supplier}\n"));
                                    }
                                }
                            }
                        }
                    }
                    Rejection::Invariant(inv) => {
                        s.push_str(&format!(
                            "\nWould violate invariant `{}`:\n  {}\n",
                            inv.name, inv.rule,
                        ));
                    }
                    Rejection::Error(err) => {
                        s.push_str(&format!("\nError: {}\n", err.message));
                    }
                }
                s
            }
        };
        // One trailing newline is noise; callers add their own.
        while out.ends_with('\n') {
            out.pop();
        }
        out
    }
}

/// Map the failing trace entry onto a structured rejection. The failing
/// entry is unique (the kernel short-circuits at the first rejecting
/// gate or violated invariant); `failing_entry` finds it, recursing into
/// the last iteration of a `For`. The `reason` string is the fallback if
/// no failing entry is found, which should not happen on a rejection.
fn verdict_from_rejection(program: &Program, reason: &str, trace: &[TraceEntry]) -> Verdict {
    match failing_entry(trace) {
        Some(TraceEntry::Require {
            expression,
            outcome:
                RequireOutcome::Rejected {
                    directly_missing_claims,
                    ..
                },
        }) => gate_verdict(
            program,
            expression,
            GateKind::Require,
            directly_missing_claims,
        ),
        Some(TraceEntry::BindOne {
            expression,
            outcome:
                BindOneOutcome::NoMatch {
                    directly_missing_claims,
                    ..
                },
        }) => gate_verdict(
            program,
            expression,
            GateKind::BindOne,
            directly_missing_claims,
        ),
        Some(TraceEntry::InvariantCheck {
            name, expression, ..
        }) => Verdict::Rejected(Rejection::Invariant(InvariantRejection {
            name: name.clone(),
            rule: expression.clone(),
        })),
        _ => Verdict::Rejected(Rejection::Error(ErrorRejection {
            message: reason.to_string(),
        })),
    }
}

/// Build a [`Rejection::Gate`], attaching candidate suppliers to each
/// directly-missing claim by its predicate.
fn gate_verdict(
    program: &Program,
    gate: &str,
    statement_kind: GateKind,
    missing: &[RenderedClaim],
) -> Verdict {
    let directly_missing_claims = missing
        .iter()
        .map(|claim| MissingClaim {
            predicate: claim.predicate.clone(),
            rendered: claim.rendered.clone(),
            candidate_supplier_transformations: transformations_asserting(
                program,
                &claim.predicate,
            ),
        })
        .collect();
    Verdict::Rejected(Rejection::Gate(GateRejection {
        gate: gate.to_string(),
        statement_kind,
        directly_missing_claims,
    }))
}

/// Find the single trace entry responsible for a rejection: a rejecting
/// `Require`, a no-match `BindOne`, or a failed `InvariantCheck`. Scans
/// from the end (the failing entry is the last thing recorded before the
/// rejection bubbled up) and recurses into the last iteration of a `For`.
fn failing_entry(trace: &[TraceEntry]) -> Option<&TraceEntry> {
    for entry in trace.iter().rev() {
        match entry {
            TraceEntry::Require {
                outcome: RequireOutcome::Rejected { .. },
                ..
            }
            | TraceEntry::BindOne {
                outcome: BindOneOutcome::NoMatch { .. },
                ..
            }
            | TraceEntry::InvariantCheck { held: false, .. } => return Some(entry),
            TraceEntry::For { iterations, .. } => {
                if let Some(last) = iterations.last()
                    && let Some(inner) = failing_entry(&last.trace)
                {
                    return Some(inner);
                }
            }
            _ => {}
        }
    }
    None
}
