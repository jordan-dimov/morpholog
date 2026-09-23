//! The explanation engine: a deterministic, structured account of why a
//! proposed transition was admitted or rejected.
//!
//! Not a second evaluator: it runs [`propose_with_trace`], maps the trace
//! onto an [`Explanation`], and names candidate suppliers via
//! [`transformations_asserting`]. The words come only from predicate and
//! transformation names plus fixed templates, so an explanation is
//! reproducible.
//!
//! It looks one step deep: the positive claims a failing gate needed and
//! did not find. They are called claims, not evidence, because some are
//! authority, standing or prior use. It does not explain a blocker that is
//! present (`not X` where `X` holds), a failed comparison, or `exists` /
//! `or` alternatives, and it suggests no repairs. Those rejections carry
//! the reason with an empty `directly_missing_claims`.

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

/// Why a transition was rejected: a `require` / `bind_one` gate did not
/// hold ([`Rejection::Gate`]), the candidate state would violate an
/// invariant ([`Rejection::Invariant`]), or the kernel raised an error
/// first ([`Rejection::Error`], e.g. a multi-match `bind_one`, a type
/// mismatch, an unknown transformation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rejection {
    Gate(GateRejection),
    Invariant(InvariantRejection),
    Error(ErrorRejection),
}

/// A `require` or `bind_one` gate that did not hold. `gate` is the
/// rendered gate expression; `statement_kind` says which of the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRejection {
    pub gate: String,
    /// The gate's stable name, when its author gave it one. Unlike `gate`,
    /// it survives rewording. A dry run has no rejection envelope to carry
    /// it, so it lives here too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    pub statement_kind: GateKind,
    /// The first positive claim the gate needed and did not find. It is
    /// not every claim that would make the gate pass. Empty when the gate
    /// failed on something other than a missing positive claim (a present
    /// blocker, a comparison).
    pub directly_missing_claims: Vec<MissingClaim>,
}

/// Which kind of gate rejected.
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
    /// Transformations that admit this predicate. A candidate only: it may
    /// not be able to supply this exact claim for this actor, arguments or
    /// state, since it has gates of its own.
    pub candidate_supplier_transformations: Vec<String>,
}

/// Explain why `transition` is admissible or rejected against
/// `pre_state`. Pure: it runs the kernel in memory and reads the trace.
pub fn explain(program: &Program, transition: &Transition, pre_state: &State) -> Explanation {
    let transition_ref = TransitionRef {
        transformation: transition.transformation_name.to_string(),
        args: transition.args.iter().map(render_eval_value).collect(),
        actor: transition.actor.to_string(),
    };

    let Some(transformation) = program.transformation(transition.transformation_name.as_str())
    else {
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

    let traced = propose_with_trace(
        transformation,
        transition,
        pre_state,
        &program.invariants,
        &program.definitions,
    );
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
        } => verdict_from_rejection(program, &reason.to_string(), &trace),
    };

    Explanation {
        transition: transition_ref,
        verdict,
    }
}

impl Explanation {
    /// Render this explanation as deterministic prose, built only from
    /// names and fixed templates.
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

/// Map the failing trace entry onto a structured rejection. The kernel
/// stops at the first failure, so there is one. `reason` is a fallback
/// for the case where none is found, which should not happen.
fn verdict_from_rejection(program: &Program, reason: &str, trace: &[TraceEntry]) -> Verdict {
    match failing_entry(trace) {
        Some(TraceEntry::Require {
            expression,
            name,
            outcome:
                RequireOutcome::Rejected {
                    directly_missing_claims,
                    ..
                },
        }) => gate_verdict(
            program,
            expression,
            name.as_deref(),
            GateKind::Require,
            directly_missing_claims,
        ),
        Some(TraceEntry::BindOne {
            expression,
            name,
            outcome:
                BindOneOutcome::NoMatch {
                    directly_missing_claims,
                    ..
                },
        }) => gate_verdict(
            program,
            expression,
            name.as_deref(),
            GateKind::BindOne,
            directly_missing_claims,
        ),
        Some(TraceEntry::InvariantCheck {
            name, expression, ..
        }) => Verdict::Rejected(Rejection::Invariant(InvariantRejection {
            name: name.to_string(),
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
    rule: Option<&str>,
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
        rule: rule.map(ToString::to_string),
        statement_kind,
        directly_missing_claims,
    }))
}

/// Find the trace entry that caused a rejection: a rejecting `Require`, a
/// no-match `BindOne`, or a failed `InvariantCheck`. Scans from the end,
/// where the failure was recorded, and into the last iteration of a `For`.
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
