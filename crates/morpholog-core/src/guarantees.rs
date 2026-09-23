//! `inspect guarantees`: the states a programme makes impossible.
//!
//! Where [`crate::explain`] answers "why was this rejected?", this answers
//! "what does this model forbid, before it ever runs?" - the question a
//! controller or regulator asks first. One [`Guarantee`] per invariant: the
//! rendered rule, plus a `forbids` clause for a `not(...)` invariant, whose
//! inner expression is the forbidden state.
//!
//! Deliberately narrow: no proof search and no hand-written summaries. The
//! words come only from invariant names and the formatter.

use serde::{Deserialize, Serialize};

use crate::compiled::CompiledProgram;
use crate::format;
use crate::ir::Prop;

/// One guarantee derived from one invariant: the impossible state it
/// rules out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Guarantee {
    /// The invariant's name.
    pub invariant: String,
    /// The invariant body, rendered (the rule that must always hold).
    pub rule: String,
    /// For a `not(...)` invariant, the inner expression rendered - the
    /// state the model forbids outright. `None` when the forbidden state
    /// is not mechanically obvious from the invariant's shape (an
    /// `implies`, a comparator); the `rule` still carries the guarantee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbids: Option<String>,
    /// For a discipline-generated invariant, the declaration clause that
    /// implied it ("predicate CurrentFigure, current pointer by (owner)"), so
    /// a generated name in a rejection or audit row leads back to its source.
    /// `None` for authored invariants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Derive the guarantees a programme makes from its declared invariants,
/// in declaration order. Pure and mechanical: one [`Guarantee`] per
/// invariant.
pub fn guarantees(compiled: &CompiledProgram) -> Vec<Guarantee> {
    let program = compiled.program();
    let provenance = crate::disciplines::discipline_provenance(program);
    program
        .invariants
        .iter()
        .map(|inv| Guarantee {
            invariant: inv.name.to_string(),
            rule: format::format_prop_inline(&inv.body),
            // A `not(X)` invariant forbids exactly `X`; no other shape
            // names its bad state this directly.
            forbids: match &inv.body {
                Prop::Not(inner) => Some(format::format_prop_inline(inner)),
                _ => None,
            },
            from: provenance.get(inv.name.as_str()).cloned(),
        })
        .collect()
}

/// Render a programme's guarantees as deterministic prose - the
/// human-readable "what does this model make impossible?" view.
pub fn render_guarantees(program_name: &str, guarantees: &[Guarantee]) -> String {
    if guarantees.is_empty() {
        return format!(
            "`{program_name}` declares no invariants, so it makes nothing structurally impossible."
        );
    }
    let mut out = format!("Guarantees of `{program_name}` - states this model makes impossible:\n");
    for g in guarantees {
        out.push_str(&format!("\n  {}\n    rule: {}\n", g.invariant, g.rule));
        if let Some(forbids) = &g.forbids {
            out.push_str(&format!("    forbids: {forbids}\n"));
        }
        if let Some(from) = &g.from {
            out.push_str(&format!("    from: {from}\n"));
        }
    }
    // No trailing newline; callers add their own.
    out.truncate(out.trim_end().len());
    out
}
