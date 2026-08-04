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
//! The same links read from the invariant's side are the **front-line
//! coverage** ([`ControlMatrix::front_line_coverage`]), at implication-shape
//! granularity so partial coverage of a multi-implication invariant stays
//! visible. Each implication is one of three: front-loaded (a gate exists),
//! a **backstop** (a transformation can trigger it but no gate front-loads
//! it - caught only at commit), or **dormant** (no declared transformation
//! triggers it at all). That three-way reading is the honest answer to
//! "where is the front line for this standing rule, and where is there none?"
//!
//! Deterministic and mechanical, like every legibility surface here:
//! the words come only from declared names and the formatter, never
//! generated prose. Deliberately shallow (v0): top-level gates in
//! body order, no reachability analysis, no cross-transformation
//! flow, predicate-overlap as the "syntactic subsumption" (no Prop-level
//! entailment proving). Gates inside `for` bodies are iteration
//! conditions, not admission preconditions, and are deliberately not
//! lifted out.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::analysis::{predicates_asserted_by, predicates_referenced_by_prop};
use crate::compiled::CompiledProgram;
use crate::definitions::DefinitionTable;
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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct GateControl {
    /// The author's stable identifier for this gate, absent when it has
    /// none. The name a refusal reports, and what reads best here: a
    /// reviewer scanning the control matrix wants the rule's name, not its
    /// expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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

/// One transformation's admission preconditions, in body order. Gates
/// inside a `for` body are not among them - see `collect_gates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TransformationControls {
    pub transformation: String,
    pub gates: Vec<GateControl>,
}

/// One gate that front-loads an implication shape, named from the
/// invariant's side: the same honesty/debug fields as the gate-side
/// [`GateFrontLoad`], plus which transformation and gate they belong to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GateRef {
    pub transformation: String,
    /// `"require"` or `"bind"`.
    pub form: String,
    /// The gate's stable identifier, when it has one - so this view and
    /// the transformation-side view name the same rule the same way.
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

/// The invariant side of the front-loads relation, at **implication-shape
/// granularity** (an authored invariant with several implications yields
/// several rows - partial coverage stays visible). Three readings:
/// `front_loaded_by` non-empty = a front line exists; empty with
/// `triggered_by_transformations` non-empty = a **backstop** (a
/// transformation can trigger it, but no gate front-loads it - checked
/// only at commit); both empty = **dormant** (no declared transformation
/// currently triggers this implication shape at all).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InvariantFrontLoad {
    pub invariant: String,
    /// `<antecedent> and not (<consequent>)` - the forbidden state this
    /// implication shape rules out (matches the gate-side `failure_shape`).
    pub failure_shape: String,
    /// Transformations that admit a predicate the antecedent rests on -
    /// the ones that can make this implication relevant.
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
    /// One entry per authored implication-shaped invariant's implication
    /// (the front-loads relation's domain). See [`InvariantFrontLoad`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub front_line_coverage: Vec<InvariantFrontLoad>,
}

/// Derive the control matrix from a parsed programme. Pure and
/// mechanical: one entry per transformation in declaration order,
/// gates in body order, plus the invariant guarantees.
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

    // Invert the gate front-loads links into the invariant-side view, at
    // implication-shape granularity. Keyed by (invariant, failure shape) -
    // not the shape alone, so two invariants that happen to render the same
    // implication shape keep separate front-loaders. Partial coverage of a
    // multi-implication invariant stays visible: one row per implication.
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
    // Which transformations admit a predicate each implication's
    // antecedent rests on - the backstop-vs-dormant distinction.
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

fn authored_implications(program: &Program, defs: DefinitionTable<'_>) -> Vec<InvImplication> {
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

/// The transformation's admission preconditions: top-level `require` and
/// `bind` statements, in body order. A gate inside a `for` is deliberately
/// NOT lifted here - it is an iteration condition, and rendering it flat
/// would show a per-item condition as though it gated the whole
/// transformation. `controls.rs`'s doctrine pin holds that line.
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
        name,
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
