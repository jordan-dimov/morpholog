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
//! Deterministic and mechanical, like every legibility surface here:
//! the words come only from declared names and the formatter, never
//! generated prose. Deliberately shallow (v0): top-level gates in
//! body order, no reachability analysis, no cross-transformation
//! flow. Gates inside `for` bodies are iteration conditions, not
//! admission preconditions, and are deliberately not lifted out.

use serde::{Deserialize, Serialize};

use crate::analysis::predicates_referenced_by_prop;
use crate::format;
use crate::guarantees::{Guarantee, guarantees};
use crate::ir::{Program, Stmt};

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
    let transformations = program
        .transformations
        .iter()
        .map(|t| TransformationControls {
            transformation: t.name.to_string(),
            gates: t
                .body
                .iter()
                .filter_map(|stmt| match stmt {
                    Stmt::Require(prop) => Some(gate("require", prop, &program.definitions)),
                    Stmt::BindOne(prop) => Some(gate("bind", prop, &program.definitions)),
                    _ => None,
                })
                .collect(),
        })
        .collect();
    ControlMatrix {
        program: program.name.clone(),
        transformations,
        guarantees: guarantees(program),
    }
}

fn gate(form: &str, prop: &crate::ir::Prop, definitions: &[crate::ir::Definition]) -> GateControl {
    let mut consults = std::collections::BTreeSet::new();
    predicates_referenced_by_prop(prop, definitions, &mut consults);
    GateControl {
        form: form.to_string(),
        condition: format::format_prop_inline(prop),
        consults: consults.into_iter().map(|p| p.to_string()).collect(),
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
