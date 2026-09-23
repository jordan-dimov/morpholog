//! `inspect controls`: the control matrix an auditor reads.
//!
//! An auditor asks two things of a rule set: "what can never be true?"
//! and "what must be true before each action?". [`crate::guarantees`]
//! answers the first from the invariants. This module answers the second
//! from each transformation's `require` and `bind` gates, and packages
//! both as one [`ControlMatrix`].
//!
//! It also links the two: a gate **front-loads** an invariant when the
//! transformation admits a predicate the invariant's antecedent rests on,
//! and the gate positively references a predicate the consequent also
//! references. The gate then checks early what the invariant enforces at
//! commit. This is a name match, not a proof: the invariant is still
//! checked at commit, other transformations exist, and a shared
//! predicate may not mean the same business condition. Each link names
//! both predicates so the reader can judge. A consequent with no
//! positive predicate (a `sum(..) <= ..` cap) is left unlinked. The
//! failure each link guards against renders as
//! `<antecedent> and not (<consequent>)`.
//!
//! Read from the invariant's side ([`ControlMatrix::front_line_coverage`]),
//! each implication is front-loaded (a gate exists), a **backstop** (some
//! transformation can trigger it but no gate front-loads it, so only the
//! commit catches it), or **dormant** (no transformation triggers it).
//!
//! The output is mechanical: words come only from declared names and the
//! formatter. It is deliberately shallow: top-level gates in body order,
//! no reachability or cross-transformation analysis. Gates inside `for`
//! bodies are per-item conditions, so they are not listed.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::analysis::{predicates_asserted_by, predicates_referenced_by_prop};
use crate::compiled::CompiledProgram;
use crate::definitions::DefinitionTable;
use crate::format;
use crate::guarantees::{Guarantee, guarantees};
use crate::ir::{InvariantOrigin, PredicateName, Program, Prop, Stmt};
use crate::lint::{implications_of, positive_claims_of};

/// One invariant a gate **front-loads**: the gate checks early what the
/// invariant enforces at commit. A match on shared predicates, not a
/// proof; see the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GateFrontLoad {
    /// The invariant this gate front-loads.
    pub invariant: String,
    /// The predicates this transformation admits that the invariant's
    /// antecedent rests on, sorted.
    pub triggered_by: Vec<String>,
    /// The predicates referenced positively by both the gate and the
    /// invariant's consequent, sorted. A gate may still be stronger or
    /// weaker than its invariant.
    pub shared: Vec<String>,
    /// The forbidden state, rendered as `<antecedent> and not
    /// (<consequent>)`. Always present, unlike [`Guarantee::forbids`].
    pub failure_shape: String,
}

/// One precondition on one transformation: a `require` gate or a
/// `bind` unique-lookup, rendered, with the predicates it consults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GateControl {
    /// The gate's author-given name, if any: the name a refusal reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `"require"` (a yes/no condition) or `"bind"` (exactly one
    /// matching claim, refused on zero or several).
    pub form: String,
    /// The condition, rendered in surface syntax.
    pub condition: String,
    /// The claim predicates this precondition consults, sorted.
    pub consults: Vec<String>,
    /// The invariants this gate front-loads (see [`GateFrontLoad`]).
    /// Empty for a gate with no matching invariant, such as an authority
    /// check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub front_loads: Vec<GateFrontLoad>,
}

/// One transformation's admission preconditions, in body order. Gates
/// inside a `for` body are not among them - see `collect_gates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TransformationControls {
    pub transformation: String,
    pub gates: Vec<GateControl>,
}

/// One gate that front-loads an implication, seen from the invariant's
/// side: the fields of [`GateFrontLoad`] plus the transformation and gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GateRef {
    pub transformation: String,
    /// `"require"` or `"bind"`.
    pub form: String,
    /// The gate's author-given name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The gate condition, rendered in surface syntax.
    pub condition: String,
    /// The predicates this transformation admits that put the invariant
    /// in play (the antecedent side of the link).
    pub triggered_by: Vec<String>,
    /// The predicates the gate and the consequent both reference.
    pub shared: Vec<String>,
}

/// The invariant side of the front-loads relation, one row per
/// implication, so partial coverage stays visible. Three readings:
/// - `front_loaded_by` non-empty: a gate checks it early;
/// - only `triggered_by_transformations` non-empty: a **backstop**,
///   checked only at commit;
/// - both empty: **dormant**, no transformation triggers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InvariantFrontLoad {
    pub invariant: String,
    /// `<antecedent> and not (<consequent>)`: the forbidden state,
    /// matching the gate side's `failure_shape`.
    pub failure_shape: String,
    /// Transformations that admit a predicate the antecedent rests on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggered_by_transformations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub front_loaded_by: Vec<GateRef>,
}

/// The full control matrix: what must be true before each action
/// (per-transformation gates), what can never be true (the invariant
/// guarantees), and the invariant-side front-line coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ControlMatrix {
    pub program: String,
    pub transformations: Vec<TransformationControls>,
    pub guarantees: Vec<Guarantee>,
    /// One entry per implication of each authored invariant. See
    /// [`InvariantFrontLoad`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub front_line_coverage: Vec<InvariantFrontLoad>,
}

/// Derive the control matrix from a parsed programme: one entry per
/// transformation in declaration order, gates in body order, plus the
/// invariant guarantees.
pub fn controls(compiled: &CompiledProgram) -> ControlMatrix {
    let program = compiled.program();
    let defs = compiled.definition_table();
    let implications = authored_implications(program, defs);

    let transformations: Vec<TransformationControls> = program
        .transformations
        .iter()
        .map(|t| {
            let mut asserted = BTreeSet::new();
            predicates_asserted_by(t, &mut asserted);
            let mut collected = Vec::new();
            collect_gates(&t.body, &mut collected);
            let gates = collected
                .into_iter()
                .map(|(form, prop, name)| {
                    gate(
                        form,
                        prop,
                        name.as_ref().map(ToString::to_string),
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

    // Invert the gate links into the invariant-side view. Keyed by
    // (invariant, failure shape) so two invariants rendering the same
    // shape keep separate rows.
    let mut by_shape: BTreeMap<(&str, &str), Vec<GateRef>> = BTreeMap::new();
    for t in &transformations {
        for g in &t.gates {
            for link in &g.front_loads {
                by_shape
                    .entry((link.invariant.as_str(), link.failure_shape.as_str()))
                    .or_default()
                    .push(GateRef {
                        transformation: t.transformation.clone(),
                        form: g.form.clone(),
                        name: g.name.clone(),
                        condition: g.condition.clone(),
                        triggered_by: link.triggered_by.clone(),
                        shared: link.shared.clone(),
                    });
            }
        }
    }
    // Which transformations can trigger each implication: backstop or
    // dormant.
    let asserted_by: Vec<(String, BTreeSet<PredicateName>)> = program
        .transformations
        .iter()
        .map(|t| {
            let mut asserted = BTreeSet::new();
            predicates_asserted_by(t, &mut asserted);
            (t.name.to_string(), asserted)
        })
        .collect();
    let front_line_coverage: Vec<InvariantFrontLoad> = implications
        .iter()
        .map(|imp| InvariantFrontLoad {
            invariant: imp.invariant.clone(),
            failure_shape: imp.failure_shape.clone(),
            triggered_by_transformations: asserted_by
                .iter()
                .filter(|(_, asserted)| !imp.antecedent.is_disjoint(asserted))
                .map(|(name, _)| name.clone())
                .collect(),
            front_loaded_by: by_shape
                .get(&(imp.invariant.as_str(), imp.failure_shape.as_str()))
                .cloned()
                .unwrap_or_default(),
        })
        .collect();

    ControlMatrix {
        program: program.name.clone(),
        transformations,
        guarantees: guarantees(compiled),
        front_line_coverage,
    }
}

/// One implication of an authored invariant, reduced to the positive
/// predicates of its antecedent (who can trigger it) and consequent
/// (what a gate must overlap). Generated discipline invariants are left
/// out.
struct InvImplication {
    invariant: String,
    antecedent: BTreeSet<PredicateName>,
    consequent: BTreeSet<PredicateName>,
    /// `<antecedent> and not (<consequent>)`, rendered once.
    failure_shape: String,
}

fn authored_implications(program: &Program, defs: DefinitionTable<'_>) -> Vec<InvImplication> {
    let mut out = Vec::new();
    for inv in &program.invariants {
        if inv.origin != InvariantOrigin::Authored {
            continue;
        }
        let collected = implications_of(&inv.body, defs);
        for imp in collected {
            let antecedent = positive_claims_of(imp.antecedent, defs);
            let consequent = positive_claims_of(imp.consequent, defs);
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

/// The transformation's top-level `require` and `bind` statements, in
/// body order. A gate inside a `for` is a per-item condition; listing it
/// here would show it as gating the whole transformation.
fn collect_gates<'s>(body: &'s [Stmt], out: &mut Vec<(&'static str, &'s Prop, Option<String>)>) {
    for stmt in body {
        match stmt {
            Stmt::Require { prop, name } => {
                out.push(("require", prop, name.as_ref().map(ToString::to_string)));
            }
            Stmt::BindOne { prop, name } => {
                out.push(("bind", prop, name.as_ref().map(ToString::to_string)));
            }
            Stmt::For { .. }
            | Stmt::Let { .. }
            | Stmt::LetNewSubject { .. }
            | Stmt::Assert(_)
            | Stmt::Retract { .. }
            | Stmt::Emit(_) => {}
        }
    }
}

fn gate(
    form: &str,
    prop: &Prop,
    name: Option<String>,
    definitions: &[crate::ir::Definition],
    defs: DefinitionTable<'_>,
    asserted: &BTreeSet<PredicateName>,
    implications: &[InvImplication],
) -> GateControl {
    let mut consults = BTreeSet::new();
    predicates_referenced_by_prop(prop, definitions, &mut consults);

    // Matched against each triggerable invariant's consequent.
    let gate_sig = positive_claims_of(prop, defs);

    // One link per matched implication, in invariant-declaration then
    // discovery order.
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
        name,
        condition: format::format_prop_inline(prop),
        consults: consults.into_iter().map(|p| p.to_string()).collect(),
        front_loads,
    }
}

/// Render the control matrix as deterministic text: transformations
/// first (what must be true before each action), then guarantees (what
/// can never be true).
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

    if !matrix.front_line_coverage.is_empty() {
        out.push_str("\nFront-line coverage for authored implication-shaped invariants:\n");
        for inv in &matrix.front_line_coverage {
            out.push_str(&format!("\n  {}:\n", inv.invariant));
            out.push_str(&format!("    failure shape: {}\n", inv.failure_shape));
            if !inv.front_loaded_by.is_empty() {
                out.push_str("    front-loaded by:\n");
                for gate in &inv.front_loaded_by {
                    out.push_str(&format!(
                        "      - {} {} {}\n",
                        gate.transformation, gate.form, gate.condition
                    ));
                }
            } else if inv.triggered_by_transformations.is_empty() {
                out.push_str(
                    "    dormant: no declared transformation currently triggers \
                     this implication shape\n",
                );
            } else {
                out.push_str(&format!(
                    "    backstop: no gate front-loads this implication shape; checked \
                     only at commit (triggered by: {})\n",
                    inv.triggered_by_transformations.join(", ")
                ));
            }
        }
    }
    out
}
