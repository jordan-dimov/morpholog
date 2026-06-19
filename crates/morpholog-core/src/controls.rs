//! `inspect controls`: the control matrix an auditor reads.
//!
//! The two questions a controller or regulator asks of a rule set are
//! "what can never be true?" and "what must already be true before
//! each action?". [`crate::guarantees`] answers the first from the
//! declared invariants; this module answers the second from each
//! transformation's gates - its `require` conditions and its
//! `bind`-exactly-one lookups - and packages both as one
//! [`ControlMatrix`], the artefact a compliance mapping cites rule by
//! rule.
//!
//! It also draws the cross-link the two questions share: which gate
//! **front-loads** which invariant. A gate front-loads an invariant when
//! this transformation can trigger that invariant (it admits a predicate
//! the invariant's antecedent rests on) and the gate positively
//! references a predicate the invariant's consequent also references - so
//! the gate pre-checks, at action time, a condition the invariant
//! enforces over committed state. This is a syntactic *correspondence*,
//! not a proof of entailment: a gate checks bound arguments in the
//! pre-state, the invariant is the standing guarantee over the candidate
//! state and is checked at commit regardless, other transformations
//! exist, and a shared predicate name need not mean the same business
//! condition (`positive_claims` collects polarity-positive references,
//! including across an `or`, which is weaker than "requires"). The map
//! says "this `require` is the front line for that rule", never "this
//! gate makes that rule unbreakable" - the same honesty boundary as the
//! unsupplied-antecedent lint. Each link names both sides: the predicate
//! this transformation admits that triggers the invariant, and the shared
//! consequent predicate (surfaced so the reader sees the nuance - a gate
//! may be stronger or weaker than the invariant; a consequent with no
//! positive predicate - a `sum(..) <= ..` cap - is correctly left
//! unlinked). The failure mode each link front-loads against is rendered
//! mechanically as `<antecedent> and not (<consequent>)`.
//!
//! Deterministic and mechanical, like every legibility surface here:
//! the words come only from declared names and the formatter, never
//! generated prose. Deliberately shallow (v0): top-level gates in
//! body order, no reachability analysis, no cross-transformation
//! flow, predicate-overlap as the "syntactic subsumption" (no Prop-level
//! entailment proving). Gates inside `for` bodies are iteration
//! conditions, not admission preconditions, and are deliberately not
//! lifted out.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::analysis::{predicates_asserted_by, predicates_referenced_by_prop};
use crate::definitions::DefinitionIndex;
use crate::format;
use crate::guarantees::{Guarantee, guarantees};
use crate::ir::{InvariantOrigin, PredicateName, Program, Prop, Stmt};
use crate::lint::{collect_implications, positive_claims};

/// One invariant a gate **front-loads**: the gate pre-checks, at action
/// time, a condition the invariant enforces over committed state. This
/// is a syntactic *correspondence* through shared positive predicate
/// references, not a proof of entailment - the invariant is the standing
/// guarantee and is checked at commit regardless; the gate is the
/// front-line filter. See the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateFrontLoad {
    /// The invariant this gate front-loads.
    pub invariant: String,
    /// The predicates this transformation admits that the invariant's
    /// antecedent rests on - why the invariant is in play here at all,
    /// sorted. The antecedent side of the correspondence.
    pub triggered_by: Vec<String>,
    /// The predicates referenced positively by both the gate and the
    /// invariant's consequent - the overlap that makes the correspondence,
    /// sorted. The reader checks the nuance (a gate may be stronger or
    /// weaker than its invariant; sharing a predicate name is not proof
    /// the same business condition is meant).
    pub shared: Vec<String>,
    /// The forbidden state the invariant rules out, rendered mechanically
    /// from its implication as `<antecedent> and not (<consequent>)` - the
    /// failure mode this gate front-loads against. Always present (unlike
    /// [`Guarantee::forbids`], which only the `not(..)` shape populates).
    pub failure_shape: String,
}

/// One precondition on one transformation: a `require` gate or a
/// `bind` unique-lookup, rendered, with the predicates it consults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateControl {
    /// `"require"` or `"bind"` - the statement form the precondition
    /// takes. A `require` is a yes/no condition; a `bind` demands
    /// exactly one matching claim and refuses on zero (or several).
    pub form: String,
    /// The condition, rendered in surface syntax.
    pub condition: String,
    /// The claim predicates this precondition consults, sorted - the
    /// evidence an auditor checks the condition against.
    pub consults: Vec<String>,
    /// The invariants this gate front-loads (see [`GateFrontLoad`]).
    /// Empty for a gate with no standing-rule counterpart - e.g. an
    /// authority gate, where the doctrine is action-time-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub front_loads: Vec<GateFrontLoad>,
}

/// Every precondition of one transformation, in body order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransformationControls {
    pub transformation: String,
    pub gates: Vec<GateControl>,
}

/// The full control matrix: what must be true before each action
/// (per-transformation gates) and what can never be true (the
/// invariant guarantees).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlMatrix {
    pub program: String,
    pub transformations: Vec<TransformationControls>,
    pub guarantees: Vec<Guarantee>,
}

/// Derive the control matrix from a parsed programme. Pure and
/// mechanical: one entry per transformation in declaration order,
/// gates in body order, plus the invariant guarantees.
pub fn controls(program: &Program) -> ControlMatrix {
    let defs = DefinitionIndex::new(&program.definitions);
    let implications = authored_implications(program, defs);

    let transformations = program
        .transformations
        .iter()
        .map(|t| {
            let mut asserted = BTreeSet::new();
            predicates_asserted_by(t, &mut asserted);
            let gates = t
                .body
                .iter()
                .filter_map(|stmt| match stmt {
                    Stmt::Require(prop) => Some(("require", prop)),
                    Stmt::BindOne(prop) => Some(("bind", prop)),
                    _ => None,
                })
                .map(|(form, prop)| {
                    gate(
                        form,
                        prop,
                        &program.definitions,
                        defs,
                        &asserted,
                        &implications,
                    )
                })
                .collect();
            TransformationControls {
                transformation: t.name.to_string(),
                gates,
            }
        })
        .collect();
    ControlMatrix {
        program: program.name.clone(),
        transformations,
        guarantees: guarantees(program),
    }
}

/// One authored, implication-shaped invariant reduced to the predicate
/// footprints the protection match needs: what its antecedent positively
/// requires (so we know which transformations can trigger it) and what
/// its consequent positively requires (what a gate must overlap to be
/// front-loading it). Generated discipline invariants are excluded - a
/// gate does not front-load auto-generated uniqueness.
struct InvImplication {
    invariant: String,
    antecedent: BTreeSet<PredicateName>,
    consequent: BTreeSet<PredicateName>,
    /// `<antecedent> and not (<consequent>)`, rendered once.
    failure_shape: String,
}

fn authored_implications(program: &Program, defs: DefinitionIndex<'_>) -> Vec<InvImplication> {
    let mut out = Vec::new();
    for inv in &program.invariants {
        if inv.origin != InvariantOrigin::Authored {
            continue;
        }
        let mut collected = Vec::new();
        collect_implications(
            &inv.body,
            true,
            defs,
            &mut BTreeSet::new(),
            &mut Vec::new(),
            &mut collected,
        );
        for imp in collected {
            let mut antecedent = BTreeSet::new();
            positive_claims(
                imp.antecedent,
                true,
                defs,
                &mut BTreeSet::new(),
                &mut antecedent,
            );
            let mut consequent = BTreeSet::new();
            positive_claims(
                imp.consequent,
                true,
                defs,
                &mut BTreeSet::new(),
                &mut consequent,
            );
            out.push(InvImplication {
                invariant: inv.name.to_string(),
                antecedent,
                consequent,
                failure_shape: format!(
                    "{} and not ({})",
                    format::format_prop_inline(imp.antecedent),
                    format::format_prop_inline(imp.consequent)
                ),
            });
        }
    }
    out
}

fn gate(
    form: &str,
    prop: &Prop,
    definitions: &[crate::ir::Definition],
    defs: DefinitionIndex<'_>,
    asserted: &BTreeSet<PredicateName>,
    implications: &[InvImplication],
) -> GateControl {
    let mut consults = BTreeSet::new();
    predicates_referenced_by_prop(prop, definitions, &mut consults);

    // The predicates the gate references positively - the signature we
    // match against each triggerable invariant's consequent.
    let mut gate_sig = BTreeSet::new();
    positive_claims(prop, true, defs, &mut BTreeSet::new(), &mut gate_sig);

    // A gate front-loads an invariant when this transformation can trigger
    // it (admits a predicate the antecedent rests on) AND the gate
    // references a predicate the invariant's consequent also references.
    // One link per matched implication, naming both sides, in
    // invariant-declaration then discovery order.
    let front_loads = implications
        .iter()
        .filter_map(|imp| {
            let triggered_by: Vec<String> = imp
                .antecedent
                .intersection(asserted)
                .map(ToString::to_string)
                .collect();
            if triggered_by.is_empty() {
                return None;
            }
            let shared: Vec<String> = gate_sig
                .intersection(&imp.consequent)
                .map(ToString::to_string)
                .collect();
            if shared.is_empty() {
                return None;
            }
            Some(GateFrontLoad {
                invariant: imp.invariant.clone(),
                triggered_by,
                shared,
                failure_shape: imp.failure_shape.clone(),
            })
        })
        .collect();

    GateControl {
        form: form.to_string(),
        condition: format::format_prop_inline(prop),
        consults: consults.into_iter().map(|p| p.to_string()).collect(),
        front_loads,
    }
}

/// Render the control matrix as deterministic prose - the view an
/// auditor or a compliance mapping reads. Transformations first
/// (what must be true before each action), guarantees second
/// (what can never be true), mirroring how a control walkthrough
/// runs: actions, then standing rules.
pub fn render_controls(matrix: &ControlMatrix) -> String {
    let mut out = String::new();
    out.push_str(&format!("Controls for `{}`\n", matrix.program));
    out.push_str("\nBefore each action (gates):\n");
    for t in &matrix.transformations {
        out.push_str(&format!("\n  {} may commit only when:\n", t.transformation));
        if t.gates.is_empty() {
            out.push_str("    (no preconditions: admission is governed by the invariants alone)\n");
        }
        for g in &t.gates {
            match g.form.as_str() {
                "bind" => out.push_str(&format!(
                    "    - exactly one claim matches {}\n",
                    g.condition
                )),
                _ => out.push_str(&format!("    - {}\n", g.condition)),
            }
            if !g.consults.is_empty() {
                out.push_str(&format!("      consults: {}\n", g.consults.join(", ")));
            }
            for p in &g.front_loads {
                out.push_str(&format!("      front-loads invariant `{}`\n", p.invariant));
                out.push_str(&format!(
                    "        triggered by: {}\n",
                    p.triggered_by.join(", ")
                ));
                out.push_str(&format!("        shared: {}\n", p.shared.join(", ")));
                out.push_str(&format!("        failure shape: {}\n", p.failure_shape));
            }
        }
    }
    out.push_str("\nAlways (invariants):\n");
    for g in &matrix.guarantees {
        out.push_str(&format!("\n  {}:\n    {}\n", g.invariant, g.rule));
        if let Some(forbids) = &g.forbids {
            out.push_str(&format!("    forbids outright: {forbids}\n"));
        }
    }
    out
}
