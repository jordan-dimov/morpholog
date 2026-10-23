//! Rehearsing a candidate programme over committed history: every
//! recorded proposal re-decided by `propose`, against the state the
//! audit rows before it build in canonical order.
//!
//! The replayer is `propose` itself, so a reported difference reproduces
//! by proposing the row again over that prefix state. The prefix is the
//! recorded deltas applied in `(committed_at, transition_id)` order;
//! `committed_at` is the writer's transaction start, so a proposal that
//! started later may have committed earlier, and its prefix can hold a
//! claim it never saw. The report states the contract, not a snapshot.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::admission::{EffectiveDelta, effective_delta};
use crate::coverage::RefusalKind;
use crate::eval::EvalError;
use crate::ir::{Subject, TransformationName};
use crate::prepared::PreparedProgram;
use crate::propose::{Compared, Outcome, RejectionReason, Transition, WitnessBinding};
use crate::state::{ClaimInstance, IntentInstance, State};
use crate::{EvalValue, SEMANTICS_VERSION};

/// Bumped when the report shape changes incompatibly. New optional
/// fields do not bump it.
pub const REHEARSAL_FORMAT_VERSION: u32 = 1;
/// What each row was decided against: the recorded deltas applied in
/// canonical audit order, which for a serial history is the state the
/// proposal saw.
pub const REHEARSAL_CONTRACT: &str = "canonical_audit_prefix_v1";

/// One admitted proposal as its audit row records it.
#[derive(Debug, Clone, Copy)]
pub struct RecordedProposal<'a> {
    pub transition_id: &'a str,
    pub transformation: &'a str,
    pub args: &'a [EvalValue],
    pub actor: &'a Subject,
    pub asserted: &'a [ClaimInstance],
    pub retracted: &'a [ClaimInstance],
    pub emitted: &'a [IntentInstance],
    /// `None` on a row written before draws were recorded.
    pub drawn: Option<&'a [Subject]>,
    pub semantics_version: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RehearsalReport {
    pub rehearsal_format_version: u32,
    pub contract: String,
    pub program: String,
    pub program_hash: String,
    /// The semantics the binary decided every row under.
    pub semantics_version: u32,
    pub transitions_replayed: u64,
    pub totals: Totals,
    /// Refusals by the first rule that refused, in rule order.
    pub refusals: Vec<RuleRefusals>,
    /// Every row not decided the same way, in canonical order.
    pub differences: Vec<RowReport>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub same: u64,
    pub different_delta: u64,
    pub would_reject: u64,
    pub would_error: u64,
    pub unreplayable: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleRefusals {
    pub rule: String,
    pub kind: RefusalKind,
    pub would_refuse: u64,
    pub refused_transitions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RowReport {
    pub transition_id: String,
    pub transformation: String,
    /// The semantics the row was admitted under, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantics_version: Option<u32>,
    #[serde(flatten)]
    pub class: RowClass,
}

/// What the candidate would have decided, against what the record holds.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum RowClass {
    Same,
    DifferentDelta {
        recorded: Effect,
        candidate: Effect,
    },
    WouldReject {
        rule: String,
        kind: RefusalKind,
        /// Absent when the kernel could pin the refusal to no bindings.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        witness: Vec<WitnessBinding>,
        #[serde(skip_serializing_if = "Option::is_none")]
        compared: Option<Compared>,
    },
    WouldError {
        error: String,
    },
    Unreplayable {
        reason: String,
    },
}

/// The effective change of one admission: what it added and removed
/// against the prefix state, and what it emitted, in order.
#[derive(Debug, Clone, Serialize)]
pub struct Effect {
    pub asserted: Vec<ClaimInstance>,
    pub retracted: Vec<ClaimInstance>,
    pub emitted: Vec<IntentInstance>,
}

impl Effect {
    fn new(delta: EffectiveDelta, emitted: &[IntentInstance]) -> Self {
        Self {
            asserted: delta.asserted,
            retracted: delta.retracted,
            emitted: emitted.to_vec(),
        }
    }

    /// Claims as sets (an effective delta holds each once), intents in
    /// order with multiplicity.
    fn same_as(&self, other: &Effect) -> bool {
        same_set(&self.asserted, &other.asserted)
            && same_set(&self.retracted, &other.retracted)
            && self.emitted == other.emitted
    }
}

fn same_set(a: &[ClaimInstance], b: &[ClaimInstance]) -> bool {
    a.len() == b.len() && a.iter().all(|c| b.contains(c))
}

pub struct Rehearsal<'p> {
    program: &'p PreparedProgram,
    transitions: u64,
    totals: Totals,
    refusals: BTreeMap<(RefusalKind, String), Vec<String>>,
    differences: Vec<RowReport>,
}

impl<'p> Rehearsal<'p> {
    pub fn new(program: &'p PreparedProgram) -> Self {
        Self {
            program,
            transitions: 0,
            totals: Totals::default(),
            refusals: BTreeMap::new(),
            differences: Vec::new(),
        }
    }

    /// Decide one recorded proposal against `pre`, the prefix state
    /// before it. The caller applies the recorded delta afterwards.
    pub fn observe(&mut self, pre: &State, row: &RecordedProposal<'_>) {
        self.transitions += 1;
        let class = self.decide(pre, row);
        match &class {
            RowClass::Same => self.totals.same += 1,
            RowClass::DifferentDelta { .. } => self.totals.different_delta += 1,
            RowClass::WouldReject { rule, kind, .. } => {
                self.totals.would_reject += 1;
                self.refusals
                    .entry((*kind, rule.clone()))
                    .or_default()
                    .push(row.transition_id.to_string());
            }
            RowClass::WouldError { .. } => self.totals.would_error += 1,
            RowClass::Unreplayable { .. } => self.totals.unreplayable += 1,
        }
        if !matches!(class, RowClass::Same) {
            self.differences.push(RowReport {
                transition_id: row.transition_id.to_string(),
                transformation: row.transformation.to_string(),
                semantics_version: row.semantics_version,
                class,
            });
        }
    }

    fn decide(&self, pre: &State, row: &RecordedProposal<'_>) -> RowClass {
        let name = TransformationName::from(row.transformation.to_string());
        let Some(transformation) = self.program.transformation(&name) else {
            return RowClass::Unreplayable {
                reason: format!(
                    "the candidate has no transformation `{}`",
                    row.transformation
                ),
            };
        };
        if transformation.parameters.len() != row.args.len() {
            return RowClass::Unreplayable {
                reason: format!(
                    "`{}` takes {} arguments; the row records {}",
                    row.transformation,
                    transformation.parameters.len(),
                    row.args.len()
                ),
            };
        }
        let transition = Transition {
            transformation_name: name,
            args: row.args.to_vec(),
            actor: row.actor.clone(),
        };
        let mut draws = row.drawn.unwrap_or(&[]).iter().cloned();
        let outcome = match self.program.propose(&transition, pre, &mut draws) {
            Ok(Some(outcome)) => outcome,
            Ok(None) => unreachable!("the transformation was looked up above"),
            Err(EvalError::SubjectSourceExhausted) => {
                return RowClass::Unreplayable {
                    reason: if row.drawn.is_none() {
                        "the act draws a fresh subject and the row records no draws".to_string()
                    } else {
                        "the act draws more fresh subjects than the row records".to_string()
                    },
                };
            }
            Err(e) => {
                return RowClass::WouldError {
                    error: e.to_string(),
                };
            }
        };
        match outcome {
            Outcome::Rejected { reason } => refusal(reason),
            Outcome::Accepted {
                asserted_claims,
                retracted_claims,
                emitted_intents,
                ..
            } => {
                let recorded = Effect::new(
                    effective_delta(pre, row.asserted, row.retracted),
                    row.emitted,
                );
                let candidate = Effect::new(
                    effective_delta(pre, &asserted_claims, &retracted_claims),
                    &emitted_intents,
                );
                if recorded.same_as(&candidate) {
                    RowClass::Same
                } else {
                    RowClass::DifferentDelta {
                        recorded,
                        candidate,
                    }
                }
            }
        }
    }

    pub fn into_report(self) -> RehearsalReport {
        let program = self.program.validated();
        let program = program.as_program();
        RehearsalReport {
            rehearsal_format_version: REHEARSAL_FORMAT_VERSION,
            contract: REHEARSAL_CONTRACT.to_string(),
            program: program.name.clone(),
            program_hash: self.program.model_hash().to_string(),
            semantics_version: SEMANTICS_VERSION,
            transitions_replayed: self.transitions,
            totals: self.totals,
            refusals: self
                .refusals
                .into_iter()
                .map(|((kind, rule), ids)| RuleRefusals {
                    rule,
                    kind,
                    would_refuse: ids.len() as u64,
                    refused_transitions: ids,
                })
                .collect(),
            differences: self.differences,
        }
    }
}

/// The first refusing rule, as `propose` stops there.
fn refusal(reason: RejectionReason) -> RowClass {
    let (rule, kind, witness, compared) = match reason {
        RejectionReason::Invariant {
            name,
            witness,
            compared,
            ..
        } => (name.to_string(), RefusalKind::Invariant, witness, compared),
        RejectionReason::Require {
            name,
            rendered,
            witness,
            compared,
        } => (
            name.map_or(rendered, |n| n.to_string()),
            RefusalKind::Require,
            witness,
            compared,
        ),
        RejectionReason::BindNone {
            name,
            rendered,
            witness,
            compared,
        } => (
            name.map_or(rendered, |n| n.to_string()),
            RefusalKind::Bind,
            witness,
            compared,
        ),
    };
    RowClass::WouldReject {
        rule,
        kind,
        witness,
        compared,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir_builder::{
        assert_, claim, let_new_subject, not, params, predicate, program, require, transformation,
        var,
    };

    fn flag_program() -> crate::ir::Program {
        program("flags")
            .predicates(vec![
                predicate("Flag").subject("x").build(),
                predicate("Other").subject("x").build(),
                predicate("Fresh").subject("s").build(),
            ])
            .transformations(vec![
                transformation(
                    "raise",
                    params(&["x"]),
                    vec![assert_("Flag", vec![var("x")])],
                ),
                transformation(
                    "unless_flagged",
                    params(&["x"]),
                    vec![
                        require(not(claim("Flag", vec![var("x")]))),
                        assert_("Other", vec![var("x")]),
                    ],
                ),
                transformation(
                    "mint",
                    params(&[]),
                    vec![let_new_subject("s"), assert_("Fresh", vec![var("s")])],
                ),
            ])
            .build()
    }

    fn flag(x: &str) -> ClaimInstance {
        ClaimInstance {
            predicate: "Flag".into(),
            args: vec![EvalValue::Subject(Subject::from(x))],
        }
    }

    fn other(x: &str) -> ClaimInstance {
        ClaimInstance {
            predicate: "Other".into(),
            args: vec![EvalValue::Subject(Subject::from(x))],
        }
    }

    /// Two writers under SERIALIZABLE: B read no flag and committed
    /// first, A raised the flag and started first, so A sorts before B
    /// in the audit order. B's prefix then holds the flag it never saw,
    /// and the rehearsal refuses B under the very programme that
    /// admitted it. This is the limit the contract names. The history
    /// is constructed here, not produced by PostgreSQL: the test pins
    /// what the rehearsal says of such an order, not that the database
    /// produces it.
    #[test]
    fn the_prefix_is_the_audit_order_not_the_snapshot_a_writer_saw() {
        let prepared = PreparedProgram::new(flag_program()).expect("validates");
        let mut rehearsal = Rehearsal::new(&prepared);
        let desk = Subject::from("desk");
        let x = [EvalValue::Subject(Subject::from("x"))];
        let mut pre = State::from_claims(vec![]);
        let raised = [flag("x")];
        rehearsal.observe(
            &pre,
            &RecordedProposal {
                transition_id: "a",
                transformation: "raise",
                args: &x,
                actor: &desk,
                asserted: &raised,
                retracted: &[],
                emitted: &[],
                drawn: Some(&[]),
                semantics_version: None,
            },
        );
        pre.apply(&raised, &[]);
        let admitted = [other("x")];
        rehearsal.observe(
            &pre,
            &RecordedProposal {
                transition_id: "b",
                transformation: "unless_flagged",
                args: &x,
                actor: &desk,
                asserted: &admitted,
                retracted: &[],
                emitted: &[],
                drawn: Some(&[]),
                semantics_version: None,
            },
        );
        let report = rehearsal.into_report();
        assert_eq!(report.contract, REHEARSAL_CONTRACT);
        assert_eq!(report.totals.same, 1);
        assert_eq!(report.totals.would_reject, 1);
        assert!(
            matches!(
                &report.differences[0].class,
                RowClass::WouldReject {
                    kind: RefusalKind::Require,
                    ..
                }
            ),
            "{:?}",
            report.differences[0].class
        );
    }

    /// Two claims admitted in the other order are the same effect: the
    /// effective delta is compared as a set, the intents in order.
    #[test]
    fn claims_admitted_in_another_order_are_the_same_effect() {
        let prepared = PreparedProgram::new(
            program("pair")
                .predicates(vec![
                    predicate("Flag").subject("x").build(),
                    predicate("Other").subject("x").build(),
                ])
                .transformations(vec![transformation(
                    "both",
                    params(&["x"]),
                    vec![
                        assert_("Other", vec![var("x")]),
                        assert_("Flag", vec![var("x")]),
                    ],
                )])
                .build(),
        )
        .expect("validates");
        let mut rehearsal = Rehearsal::new(&prepared);
        let desk = Subject::from("desk");
        let x = [EvalValue::Subject(Subject::from("x"))];
        let recorded = [flag("x"), other("x")];
        rehearsal.observe(
            &State::from_claims(vec![]),
            &RecordedProposal {
                transition_id: "p",
                transformation: "both",
                args: &x,
                actor: &desk,
                asserted: &recorded,
                retracted: &[],
                emitted: &[],
                drawn: Some(&[]),
                semantics_version: None,
            },
        );
        let report = rehearsal.into_report();
        assert_eq!(report.totals.same, 1, "{:?}", report.differences);
    }

    /// A row from before draws were recorded: an act that draws is
    /// unreplayable on it, with the reason saying why; one that does
    /// not draw replays.
    #[test]
    fn a_row_without_recorded_draws_replays_only_an_act_that_draws_nothing() {
        let prepared = PreparedProgram::new(flag_program()).expect("validates");
        let mut rehearsal = Rehearsal::new(&prepared);
        let desk = Subject::from("desk");
        let pre = State::from_claims(vec![]);
        let fresh = [ClaimInstance {
            predicate: "Fresh".into(),
            args: vec![EvalValue::Subject(Subject::from("s1"))],
        }];
        rehearsal.observe(
            &pre,
            &RecordedProposal {
                transition_id: "m",
                transformation: "mint",
                args: &[],
                actor: &desk,
                asserted: &fresh,
                retracted: &[],
                emitted: &[],
                drawn: None,
                semantics_version: None,
            },
        );
        let x = [EvalValue::Subject(Subject::from("x"))];
        let raised = [flag("x")];
        rehearsal.observe(
            &pre,
            &RecordedProposal {
                transition_id: "r",
                transformation: "raise",
                args: &x,
                actor: &desk,
                asserted: &raised,
                retracted: &[],
                emitted: &[],
                drawn: None,
                semantics_version: None,
            },
        );
        let report = rehearsal.into_report();
        assert_eq!((report.totals.same, report.totals.unreplayable), (1, 1));
        assert!(
            matches!(&report.differences[0].class, RowClass::Unreplayable { reason } if reason.contains("records no draws")),
            "{:?}",
            report.differences[0].class
        );
    }
}
