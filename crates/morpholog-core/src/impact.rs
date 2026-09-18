//! Which cases of an invariant a transition's delta can affect: the
//! plan is built once beside the programme, then applied to each
//! delta. It knows nothing of SQL; the PostgreSQL compiler renders a
//! bounded answer as a disjunction over its antecedent columns, and the
//! kernel will apply the same bindings to its own evaluation, so both
//! evaluators bound a check to exactly the same cases.
//!
//! Sound by widening: an occurrence that cannot constrain a case
//! variable widens the answer toward the whole invariant, never past a
//! touched case. The shapes that bound are exactly those the compiled
//! differential proves; a non-empty delta against a body carrying a
//! construct outside them (a defined call, `pre`, `or`, `xor`,
//! membership, or any value form but a term and a term-targeted sum)
//! widens to the whole invariant. An empty delta touches nothing,
//! whatever the body: the candidate is the pre-state.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use rust_decimal::Decimal;

use crate::fold::{any_prop_node, any_value_node};
use crate::ir::{Invariant, PredicateName, Prop, Term, Value, ValueExpr, Var};
use crate::state::{ClaimInstance, EvalValue};

/// How much of an invariant a delta touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Impact {
    /// The delta is disjoint from every claim pattern in the body: the
    /// invariant's truth over the candidate is its truth over the
    /// pre-state.
    Untouched,
    /// The touched cases, each a partial binding of the invariant's case
    /// variables; a case matching any of them may have changed. Empty
    /// when every touched occurrence bound contradictory values.
    Bounded(Vec<BTreeMap<Var, EvalValue>>),
    /// Touched, and not boundable to the case variables.
    Unbounded,
}

/// A claim pattern in the body: which delta claims can affect it, and
/// which case variables their arguments fix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Occurrence {
    predicate: PredicateName,
    /// Literal arguments: a delta claim mismatching one cannot affect
    /// this occurrence.
    guards: Vec<(usize, Value)>,
    /// Argument position to the case variable it binds.
    var_map: Vec<(usize, Var)>,
}

/// An invariant's impact plan, built once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactPlan {
    occurrences: Vec<Occurrence>,
    /// The body holds a construct the bounding proof does not cover.
    conservative: bool,
}

impl ImpactPlan {
    pub fn new(inv: &Invariant) -> Self {
        let case_vars = case_variables(&inv.body);
        let occurrences = RefCell::new(Vec::new());
        let unproved_value = any_value_node(&inv.body, &|v| match v {
            ValueExpr::Term(_) => false,
            ValueExpr::Sum { value, .. } => !matches!(**value, ValueExpr::Term(_)),
            _ => true,
        });
        let conservative = unproved_value
            || any_prop_node(&inv.body, &|p| match p {
                Prop::Claim { predicate, args } => {
                    let mut guards = Vec::new();
                    let mut var_map = Vec::new();
                    for (i, term) in args.iter().enumerate() {
                        match term {
                            Term::Literal(v) => guards.push((i, v.clone())),
                            Term::Var(v) if case_vars.contains(v) => var_map.push((i, v.clone())),
                            Term::Var(_) | Term::Wildcard | Term::Actor => {}
                        }
                    }
                    occurrences.borrow_mut().push(Occurrence {
                        predicate: predicate.clone(),
                        guards,
                        var_map,
                    });
                    false
                }
                Prop::Defined { .. }
                | Prop::Pre(_)
                | Prop::Or(_)
                | Prop::Xor(_, _)
                | Prop::In(_, _) => true,
                _ => false,
            });
        Self {
            occurrences: occurrences.into_inner(),
            conservative,
        }
    }

    /// The cases the delta can affect.
    pub fn classify(&self, asserted: &[ClaimInstance], retracted: &[ClaimInstance]) -> Impact {
        // Nothing changed, nothing affected: the first law, before any
        // uncertainty about the body.
        if asserted.is_empty() && retracted.is_empty() {
            return Impact::Untouched;
        }
        if self.conservative {
            return Impact::Unbounded;
        }
        // Occurrence order, deduplicated through the set so a large delta
        // stays linear in its distinct cases.
        let mut cases: Vec<BTreeMap<Var, EvalValue>> = Vec::new();
        let mut seen: HashSet<BTreeMap<Var, EvalValue>> = HashSet::new();
        let mut touched = false;
        for claim in asserted.iter().chain(retracted) {
            for occ in &self.occurrences {
                if occ.predicate != claim.predicate {
                    continue;
                }
                if !occ.guards.iter().all(|(pos, lit)| {
                    claim
                        .args
                        .get(*pos)
                        .is_some_and(|ev| literal_matches(lit, ev))
                }) {
                    continue;
                }
                touched = true;
                if occ.var_map.is_empty() {
                    return Impact::Unbounded;
                }
                let mut case = BTreeMap::new();
                let mut contradictory = false;
                for (pos, var) in &occ.var_map {
                    let Some(ev) = claim.args.get(*pos) else {
                        return Impact::Unbounded;
                    };
                    match case.insert(var.clone(), ev.clone()) {
                        Some(earlier) if earlier != *ev => contradictory = true,
                        _ => {}
                    }
                }
                if !contradictory && seen.insert(case.clone()) {
                    cases.push(case);
                }
            }
        }
        if !touched {
            return Impact::Untouched;
        }
        Impact::Bounded(cases)
    }
}

/// The case variables: those the top-level antecedent's claim patterns
/// bind, reached through conjunction only. A negated top-level body
/// binds through its inner conjunction; any other shape has none.
fn case_variables(body: &Prop) -> BTreeSet<Var> {
    fn through_and(p: &Prop, out: &mut BTreeSet<Var>) {
        match p {
            Prop::Claim { args, .. } => {
                for term in args {
                    if let Term::Var(v) = term {
                        out.insert(v.clone());
                    }
                }
            }
            Prop::And(ps) => {
                for q in ps {
                    through_and(q, out);
                }
            }
            _ => {}
        }
    }
    let mut vars = BTreeSet::new();
    match body {
        Prop::Implies { left, .. } => through_and(left, &mut vars),
        Prop::Forall { source, .. } => through_and(source, &mut vars),
        Prop::Not(inner) => through_and(inner, &mut vars),
        _ => {}
    }
    vars
}

/// A literal guard against a delta value. Kinds the bounding proof does
/// not compare are treated as matching, which only widens the touched
/// set.
fn literal_matches(lit: &Value, ev: &EvalValue) -> bool {
    match (lit, ev) {
        (Value::Subject(a), EvalValue::Subject(b)) => a == b,
        (Value::Decimal(a), EvalValue::Decimal(b)) => a.parse::<Decimal>().is_ok_and(|a| a == *b),
        _ => true,
    }
}
