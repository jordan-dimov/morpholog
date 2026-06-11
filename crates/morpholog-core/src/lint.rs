//! Lint-grade hints: findings that deserve an author's attention but
//! are not errors, because the flagged shape has a deliberate reading.
//! Surfaced by `morpholog check` as hints; `--strict` promotes them to
//! errors. Distinct from [`crate::ValidationError`] on purpose - an
//! error means the programme cannot mean what it says, a lint means it
//! says something that is usually, but not always, a mistake.
//!
//! The first lint is the gate-vs-invariant doctrine made mechanical:
//! with append-only and current-pointer classes declared as
//! disciplines, the revocation-rewrites-history shape - an invariant
//! conditioning permanent records on a retractable pointer's presence
//! - is detectable at check time.

use std::collections::BTreeSet;

use crate::definitions::DefinitionIndex;
use crate::disciplines::append_only_predicates;
use crate::ir::{Discipline, PredicateName, Program, Prop, ValueExpr};

/// One lint finding. See the module doc for the error-vs-lint line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lint {
    /// An invariant whose antecedent positively references an
    /// append-only predicate and whose consequent positively requires
    /// a current-pointer predicate. Retracting that pointer would make
    /// already-admitted records violate the rule - blocking the
    /// retraction or forcing history to be rewritten. The deliberate
    /// reading exists (continuous-compliance models re-check standing
    /// over admitted records on purpose), which is why this is a hint:
    /// keep it knowingly, or move the check into the admitting
    /// transformation's gate.
    ///
    /// Forward direction only. The reverse - a pointer's antecedent
    /// requiring an append-only consequent ("the pointer names a
    /// figure that exists") - is correct doctrine: retracting the
    /// pointer makes it vacuous, never violated.
    GateVsInvariant {
        invariant: String,
        append_only: String,
        pointer: String,
    },
}

impl std::fmt::Display for Lint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lint::GateVsInvariant {
                invariant,
                append_only,
                pointer,
            } => write!(
                f,
                "invariant `{invariant}` conditions append-only `{append_only}` \
                 records on the presence of retractable pointer `{pointer}`; \
                 retracting `{pointer}` would make already-admitted \
                 `{append_only}` records violate this rule - blocking the \
                 retraction or forcing history to be rewritten. If continuous \
                 re-checking is intended (a compliance shape), keep it \
                 deliberately; otherwise the check belongs in the admitting \
                 transformation's gate"
            ),
        }
    }
}

/// Collect every lint finding for a programme. Pure and deterministic:
/// invariants in declaration order, implications in body order.
pub fn lints(program: &Program) -> Vec<Lint> {
    let append_only = append_only_predicates(program);
    let pointers: BTreeSet<PredicateName> = program
        .predicates
        .iter()
        .filter(|d| {
            d.disciplines
                .iter()
                .any(|disc| matches!(disc, Discipline::CurrentPointerBy { .. }))
        })
        .map(|d| d.name.clone())
        .collect();
    if append_only.is_empty() || pointers.is_empty() {
        return Vec::new();
    }

    let definitions = DefinitionIndex::new(&program.definitions);
    let mut out = Vec::new();
    for inv in &program.invariants {
        let mut implications = Vec::new();
        collect_implications(
            &inv.body,
            true,
            definitions,
            &mut BTreeSet::new(),
            &mut implications,
        );
        for (antecedent, consequent) in implications {
            let mut antecedent_refs = BTreeSet::new();
            positive_claims(
                antecedent,
                true,
                definitions,
                &mut BTreeSet::new(),
                &mut antecedent_refs,
            );
            let mut consequent_refs = BTreeSet::new();
            positive_claims(
                consequent,
                true,
                definitions,
                &mut BTreeSet::new(),
                &mut consequent_refs,
            );
            for a in antecedent_refs.iter().filter(|p| append_only.contains(*p)) {
                for q in consequent_refs.iter().filter(|p| pointers.contains(*p)) {
                    out.push(Lint::GateVsInvariant {
                        invariant: inv.name.to_string(),
                        append_only: a.to_string(),
                        pointer: q.to_string(),
                    });
                }
            }
        }
    }
    out
}

/// Every `Implies` node the invariant actually ASSERTS - collected
/// only at positive polarity, because a negated implication
/// (`not (A implies B)` is `A and not B`) and an implication sitting
/// in another implication's antecedent enforce nothing of the shape
/// the lint reads. Enclosing `And`/`Or`/quantifiers preserve polarity;
/// `Not` flips it; an `Implies` flips its own left side. `Defined`
/// calls descend into their bodies (seen-set against cycles, the
/// walker red line): an implication hidden behind a named condition
/// is still an implication the invariant asserts. The collected
/// antecedent/consequent references may therefore point into a
/// definition's body, where variables are the definition's
/// parameters - free from the caller's perspective, which is exactly
/// the "does this bind for ANY arguments" reading both consumers
/// (the lint and coverage) want.
pub(crate) fn collect_implications<'a>(
    prop: &'a Prop,
    positive: bool,
    definitions: DefinitionIndex<'a>,
    seen: &mut BTreeSet<crate::ir::DefinitionName>,
    out: &mut Vec<(&'a Prop, &'a Prop)>,
) {
    match prop {
        Prop::Implies { left, right } => {
            if positive {
                out.push((left, right));
            }
            collect_implications(left, !positive, definitions, seen, out);
            collect_implications(right, positive, definitions, seen, out);
        }
        Prop::Defined { name, .. } => {
            if seen.insert(name.clone())
                && let Some(def) = definitions.get(name)
            {
                collect_implications(&def.body, positive, definitions, seen, out);
            }
        }
        Prop::Claim { .. } | Prop::In(_, _) => {}
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                collect_implications(p, positive, definitions, seen, out);
            }
        }
        Prop::Xor(left, right) => {
            collect_implications(left, positive, definitions, seen, out);
            collect_implications(right, positive, definitions, seen, out);
        }
        Prop::Not(p) => collect_implications(p, !positive, definitions, seen, out),
        Prop::Exists { body: p, .. } | Prop::Pre(p) => {
            collect_implications(p, positive, definitions, seen, out);
        }
        Prop::Forall { source, body, .. } => {
            collect_implications(source, positive, definitions, seen, out);
            collect_implications(body, positive, definitions, seen, out);
        }
        Prop::Eq(_, _) | Prop::Neq(_, _) | Prop::Compare { .. } => {}
    }
}

/// Predicates referenced at POSITIVE polarity: required to hold, not
/// required absent. `Not` flips polarity; a nested `Implies` flips its
/// left side (an implication is satisfied by its antecedent failing);
/// everything else preserves it. Negative-polarity references are
/// dropped - `implies not Pointer(...)` gets STRONGER when the pointer
/// is retracted, which is the opposite of the bug. `Defined` calls
/// descend into their bodies (with a seen-set, mirroring the analysis
/// walkers), since a named condition hides its claims behind the call.
fn positive_claims(
    prop: &Prop,
    positive: bool,
    definitions: DefinitionIndex<'_>,
    seen: &mut BTreeSet<crate::ir::DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match prop {
        Prop::Claim { predicate, .. } => {
            if positive {
                out.insert(predicate.clone());
            }
        }
        Prop::Defined { name, .. } => {
            if seen.insert(name.clone())
                && let Some(def) = definitions.get(name)
            {
                positive_claims(&def.body, positive, definitions, seen, out);
            }
        }
        Prop::Not(inner) => positive_claims(inner, !positive, definitions, seen, out),
        Prop::Implies { left, right } => {
            positive_claims(left, !positive, definitions, seen, out);
            positive_claims(right, positive, definitions, seen, out);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                positive_claims(p, positive, definitions, seen, out);
            }
        }
        Prop::Xor(left, right) => {
            // Exactly-one holds each side in both polarities; treat
            // both as referenced at the current polarity (the
            // conservative reading for a hint).
            positive_claims(left, positive, definitions, seen, out);
            positive_claims(right, positive, definitions, seen, out);
        }
        Prop::Exists { body, .. } | Prop::Pre(body) => {
            positive_claims(body, positive, definitions, seen, out);
        }
        Prop::Forall { source, body, .. } => {
            positive_claims(source, positive, definitions, seen, out);
            positive_claims(body, positive, definitions, seen, out);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            positive_value_claims(l, positive, out);
            positive_value_claims(r, positive, out);
        }
        Prop::Compare { left, right, .. } => {
            positive_value_claims(left, positive, out);
            positive_value_claims(right, positive, out);
        }
        Prop::In(_, _) => {}
    }
}

/// Value-sort companion. A defaultless `value Pred(...)` lookup
/// *requires* a claim to be readable, so it counts at the enclosing
/// polarity; one with a `default` tolerates absence and contributes
/// only what its default expression carries. `sum` bodies tolerate
/// zero matches, so they contribute nothing.
fn positive_value_claims(expr: &ValueExpr, positive: bool, out: &mut BTreeSet<PredicateName>) {
    match expr {
        ValueExpr::Term(_) => {}
        ValueExpr::ValueOf {
            predicate, default, ..
        } => {
            if positive && default.is_none() {
                out.insert(predicate.clone());
            }
            if let Some(d) = default {
                positive_value_claims(d, positive, out);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            positive_value_claims(left, positive, out);
            positive_value_claims(right, positive, out);
        }
        // A sum tolerates zero matches; its body does not REQUIRE the
        // claims, so it contributes nothing.
        ValueExpr::Sum { .. } => {}
    }
}
