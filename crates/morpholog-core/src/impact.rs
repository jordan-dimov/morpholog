//! Which cases of an invariant a transition's delta can affect. The plan is
//! built once per programme, then applied to each delta. The kernel and the
//! PostgreSQL compiler use the same plan, so both check exactly the same
//! cases.
//!
//! When in doubt, the answer widens toward the whole invariant; it never
//! misses a touched case. A delta against a body holding `pre`, `or`,
//! `xor`, membership, a value lookup, a conditional value or a sum over
//! an expression checks the whole invariant, and so does a defined call
//! unless the plan was built with the definitions to follow. Arithmetic,
//! an extremum or a builtin over terms reads nothing, so it widens
//! nothing. A delta that touches no predicate a state rule reads leaves
//! the rule's truth where it was, whatever its shape; a rule over `pre`
//! judges the change itself, so only an empty delta escapes it.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use rust_decimal::Decimal;

use crate::analysis::predicates_referenced_by_prop;
use crate::definitions::DefinitionTable;
use crate::fold::{Node, mentions_pre, walk_prop};
use crate::ir::{
    Definition, DefinitionName, Invariant, PredicateName, Prop, Term, Value, ValueExpr, Var,
};
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

/// The one claim pattern an invariant's case is bounded to, and the
/// positions a case variable or a literal the bounding proof compares
/// fixes in it.
pub(crate) struct BoundedOccurrence<'a> {
    pub(crate) predicate: &'a PredicateName,
    pub(crate) constrained: BTreeSet<usize>,
}

/// An invariant's impact plan, built once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactPlan {
    occurrences: Vec<Occurrence>,
    /// The body holds a construct the bounding proof does not cover.
    conservative: bool,
    /// Every predicate the body reads, through its calls, when a delta
    /// outside it can be dismissed: `None` when a call was not followed,
    /// so the set is not known, or the body reads `pre`, so the rule
    /// judges the change and not the state.
    footprint: Option<BTreeSet<PredicateName>>,
}

impl ImpactPlan {
    /// The plan with no definitions to follow: a defined call checks the
    /// whole invariant.
    pub fn new(inv: &Invariant) -> Self {
        let case_vars = candidate_case_variables(&inv.body);
        let mut occurrences = Vec::new();
        let mut conservative = false;
        let mut calls = false;
        walk_prop(&inv.body, &mut |n| match classify_node(&n) {
            NodeKind::Claim(predicate, args) => {
                let mut guards = Vec::new();
                let mut var_map = Vec::new();
                for (i, term) in args.iter().enumerate() {
                    match term {
                        Term::Literal(v) => guards.push((i, v.clone())),
                        Term::Var(v) if case_vars.contains(v) => var_map.push((i, v.clone())),
                        Term::Var(_) | Term::Wildcard | Term::Actor => {}
                    }
                }
                occurrences.push(Occurrence {
                    predicate: predicate.clone(),
                    guards,
                    var_map,
                });
            }
            NodeKind::Call(..) => {
                conservative = true;
                calls = true;
            }
            NodeKind::Widens => conservative = true,
            NodeKind::Inert => {}
        });
        Self {
            occurrences,
            conservative,
            footprint: if calls { None } else { footprint_of(inv, &[]) },
        }
    }

    /// The plan following the body's defined calls. A call carries a case
    /// variable or a literal from its argument into the claim patterns of
    /// its body; it creates no case variable, and a body variable it does
    /// not carry one into binds nothing, so an occurrence reached only
    /// through such variables widens to the whole invariant when touched.
    pub fn with_definitions(inv: &Invariant, definitions: &[Definition]) -> Self {
        let frame: Frame = candidate_case_variables(&inv.body)
            .into_iter()
            .map(|v| (v.clone(), Traced::Case(v)))
            .collect();
        let mut out = Self {
            occurrences: Vec::new(),
            conservative: false,
            footprint: footprint_of(inv, definitions),
        };
        let table = DefinitionTable::new(definitions);
        follow(&inv.body, &frame, table, &mut BTreeSet::new(), &mut out);
        out
    }

    /// The variables a bounded case can carry: those a claim pattern in
    /// the body holds at some position. A check bounded to a case seeks
    /// on their columns.
    pub fn case_variables(&self) -> BTreeSet<Var> {
        self.occurrences
            .iter()
            .flat_map(|occ| occ.var_map.iter().map(|(_, var)| var.clone()))
            .collect()
    }

    /// The plan's one claim pattern, when admission bounds every delta
    /// that touches the invariant to cases of that pattern alone: the
    /// plan is not conservative, the body holds exactly one claim
    /// pattern, and the pattern binds a case variable (one that binds
    /// none leaves a touching delta unbounded).
    pub(crate) fn single_bounded_occurrence(&self) -> Option<BoundedOccurrence<'_>> {
        if self.conservative {
            return None;
        }
        let [occ] = self.occurrences.as_slice() else {
            return None;
        };
        if occ.var_map.is_empty() {
            return None;
        }
        Some(BoundedOccurrence {
            predicate: &occ.predicate,
            constrained: occ
                .guards
                .iter()
                .filter(|(_, lit)| literal_narrows(lit))
                .map(|(pos, _)| *pos)
                .chain(occ.var_map.iter().map(|(pos, _)| *pos))
                .collect(),
        })
    }

    /// The cases the delta can affect.
    pub fn classify(&self, asserted: &[ClaimInstance], retracted: &[ClaimInstance]) -> Impact {
        // Nothing changed, so nothing is affected, whatever the body holds.
        if asserted.is_empty() && retracted.is_empty() {
            return Impact::Untouched;
        }
        if self.conservative {
            // A delta outside everything the body reads changes no claim
            // the body can see, so the truth it had over the pre-state is
            // its truth over the candidate, whatever the shape.
            let outside = self.footprint.as_ref().is_some_and(|footprint| {
                asserted
                    .iter()
                    .chain(retracted)
                    .all(|claim| !footprint.contains(&claim.predicate))
            });
            return if outside {
                Impact::Untouched
            } else {
                Impact::Unbounded
            };
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

/// What a variable is known to hold where a definition body reads it,
/// traced through the calls that led there.
#[derive(Debug, Clone)]
enum Traced {
    Case(Var),
    Literal(Value),
}

/// A body's variables that trace to a case variable or a literal. A
/// variable absent here traces to nothing: a body's own variable, or a
/// parameter whose argument was a wildcard or a non-case variable.
type Frame = BTreeMap<Var, Traced>;

fn follow(
    body: &Prop,
    frame: &Frame,
    table: DefinitionTable<'_>,
    seen: &mut BTreeSet<DefinitionName>,
    out: &mut ImpactPlan,
) {
    let mut calls = Vec::new();
    walk_prop(body, &mut |n| match classify_node(&n) {
        NodeKind::Claim(predicate, args) => {
            let mut guards = Vec::new();
            let mut var_map = Vec::new();
            for (i, term) in args.iter().enumerate() {
                match term {
                    Term::Literal(v) => guards.push((i, v.clone())),
                    Term::Var(v) => match frame.get(v) {
                        Some(Traced::Case(case)) => var_map.push((i, case.clone())),
                        Some(Traced::Literal(lit)) => guards.push((i, lit.clone())),
                        None => {}
                    },
                    Term::Wildcard | Term::Actor => {}
                }
            }
            out.occurrences.push(Occurrence {
                predicate: predicate.clone(),
                guards,
                var_map,
            });
        }
        NodeKind::Call(name, args) => calls.push((name.clone(), args.to_vec())),
        NodeKind::Widens => out.conservative = true,
        NodeKind::Inert => {}
    });
    for (name, args) in calls {
        let followed = table.enter(&name, seen, |def, seen| {
            let callee: Frame = def
                .parameters
                .iter()
                .zip(&args)
                .filter_map(|(param, arg)| {
                    let traced = match arg {
                        Term::Literal(v) => Traced::Literal(v.clone()),
                        Term::Var(v) => frame.get(v)?.clone(),
                        Term::Wildcard | Term::Actor => return None,
                    };
                    Some((param.clone(), traced))
                })
                .collect();
            follow(&def.body, &callee, table, seen, out);
            true
        });
        // An undeclared or cyclic call contributes nothing to a walker
        // that only reads; here nothing would be unsound.
        if !followed {
            out.conservative = true;
            out.footprint = None;
        }
    }
}

/// What a node means to the plan.
enum NodeKind<'a> {
    Claim(&'a PredicateName, &'a [Term]),
    Call(&'a DefinitionName, &'a [Term]),
    /// A construct the bounding proof does not cover.
    Widens,
    Inert,
}

fn classify_node<'a>(n: &Node<'a>) -> NodeKind<'a> {
    match n {
        Node::Prop(Prop::Claim { predicate, args }) => NodeKind::Claim(predicate, args),
        Node::Prop(Prop::Defined { name, args }) => NodeKind::Call(name, args),
        Node::Prop(Prop::Pre(_) | Prop::Or(_) | Prop::Xor(_, _) | Prop::In(_, _)) => {
            NodeKind::Widens
        }
        Node::Prop(
            Prop::And(_)
            | Prop::Not(_)
            | Prop::Implies { .. }
            | Prop::Exists { .. }
            | Prop::Forall { .. }
            | Prop::Eq(_, _)
            | Prop::Neq(_, _)
            | Prop::Compare { .. },
        ) => NodeKind::Inert,
        // Arithmetic, an extremum and a builtin read nothing themselves;
        // their children are walked on their own.
        Node::Value(
            ValueExpr::Term(_)
            | ValueExpr::Arith { .. }
            | ValueExpr::Extremum { .. }
            | ValueExpr::Call { .. },
        ) => NodeKind::Inert,
        Node::Value(ValueExpr::Sum { value, .. }) => {
            if matches!(**value, ValueExpr::Term(_)) {
                NodeKind::Inert
            } else {
                NodeKind::Widens
            }
        }
        Node::Value(ValueExpr::ValueOf { .. } | ValueExpr::Cond { .. }) => NodeKind::Widens,
        Node::Stmt(_) | Node::Slot(_) | Node::Binder(_) => NodeKind::Inert,
    }
}

/// Every predicate a state rule reads, through its definitions; nothing
/// for a rule over `pre`, which no delta outside its reads escapes.
fn footprint_of(inv: &Invariant, definitions: &[Definition]) -> Option<BTreeSet<PredicateName>> {
    if mentions_pre(&inv.body) {
        return None;
    }
    let mut out = BTreeSet::new();
    predicates_referenced_by_prop(&inv.body, definitions, &mut out);
    Some(out)
}

/// The case variables: those the top-level antecedent's claim patterns
/// bind, reached through conjunction only. A negated top-level body
/// binds through its inner conjunction; any other shape has none.
fn candidate_case_variables(body: &Prop) -> BTreeSet<Var> {
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

/// Whether the bounding proof compares a literal of this kind against a
/// delta value. A literal of any other kind matches every value, so it
/// narrows nothing.
fn literal_narrows(lit: &Value) -> bool {
    matches!(lit, Value::Subject(_) | Value::Decimal(_))
}

/// A literal guard against a delta value. Kinds the bounding proof does
/// not compare are treated as matching, which only widens the touched
/// set.
fn literal_matches(lit: &Value, ev: &EvalValue) -> bool {
    if !literal_narrows(lit) {
        return true;
    }
    match (lit, ev) {
        (Value::Subject(a), EvalValue::Subject(b)) => a == b,
        (Value::Decimal(a), EvalValue::Decimal(b)) => a.parse::<Decimal>().is_ok_and(|a| a == *b),
        _ => true,
    }
}
