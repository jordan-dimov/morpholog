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
use std::fmt;

use crate::analysis::predicates_referenced_by_prop;
use crate::definitions::DefinitionTable;
use crate::fold::{Node, mentions_pre, walk_prop, walk_value};
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

/// What stops the bounding proof, named as the source spells it: a
/// construct, which makes every delta inside the body's reads check it
/// whole, or a claim pattern binding no case variable, which widens
/// only the deltas that touch that pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Widening {
    Pre,
    Or,
    Xor,
    In,
    ValueLookup,
    ConditionalValue,
    SumOverExpression,
    UnfollowedCall(DefinitionName),
    /// A claim pattern binding no case variable: a delta touching it has
    /// no case to be bounded to.
    NoCaseVariable(PredicateName),
    /// A closed scope (`exists`, `forall`, `not`, a nested rule, a sum)
    /// reads a case variable's name before the conjunction around it has
    /// bound it. Inside the scope the name is the scope's own binding,
    /// not the rule's case, so a case bounded on it would miss what the
    /// scope sees.
    ScopeReadsBeforeBinding(Var),
}

impl fmt::Display for Widening {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Widening::Pre => f.write_str("`pre`"),
            Widening::Or => f.write_str("`or`"),
            Widening::Xor => f.write_str("`xor`"),
            Widening::In => f.write_str("`in`"),
            Widening::ValueLookup => f.write_str("`value`"),
            Widening::ConditionalValue => f.write_str("`if`"),
            Widening::SumOverExpression => f.write_str("a sum over an expression"),
            Widening::UnfollowedCall(name) => {
                write!(f, "`{name}` is a call that could not be followed")
            }
            Widening::NoCaseVariable(p) => write!(f, "`{p}` binds no case variable"),
            Widening::ScopeReadsBeforeBinding(v) => {
                write!(f, "`{v}` is read inside a scope before the rule binds it")
            }
        }
    }
}

/// Whether admission can bound every delta the rule sees to the cases it
/// touches, or some delta checks the rule whole. A property of the plan:
/// an execution route may still widen a bounded check on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bounding {
    Bound,
    Whole(Widening),
}

/// An invariant's impact plan, built once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactPlan {
    occurrences: Vec<Occurrence>,
    /// The first construct the bounding proof does not cover, when the
    /// body holds one.
    widening: Option<Widening>,
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
        let antecedent = antecedent_nodes(&inv.body);
        let mut occurrences = Vec::new();
        let mut widening = None;
        let mut calls = false;
        walk_prop(&inv.body, &mut |n| match classify_node(&n, &antecedent) {
            NodeKind::Claim(predicate, args) => {
                let mut guards = Vec::new();
                let mut var_map = Vec::new();
                for (i, term) in args.iter().enumerate() {
                    match term {
                        Term::Literal(v) => guards.push((i, v.clone())),
                        Term::Var(v) if case_vars.contains(v) => var_map.push((i, v.clone())),
                        Term::Var(_) | Term::Wildcard | Term::Actor | Term::DecidedAt => {}
                    }
                }
                occurrences.push(Occurrence {
                    predicate: predicate.clone(),
                    guards,
                    var_map,
                });
            }
            NodeKind::Call(name, _) | NodeKind::ValueCall(name, _) => {
                widening.get_or_insert_with(|| Widening::UnfollowedCall(name.clone()));
                calls = true;
            }
            NodeKind::Grid(binder) => {
                if let Term::Var(v) = binder
                    && case_vars.contains(v)
                {
                    widening.get_or_insert_with(|| Widening::ScopeReadsBeforeBinding(v.clone()));
                }
            }
            NodeKind::Widens(w) => {
                widening.get_or_insert(w);
            }
            NodeKind::Inert => {}
        });
        if widening.is_none() {
            let frame: Frame = case_vars
                .iter()
                .map(|v| (v.clone(), Traced::Case(v.clone())))
                .collect();
            if let Some(v) = antecedent_scope_read(&inv.body, &frame, None, &mut BTreeSet::new()) {
                widening = Some(Widening::ScopeReadsBeforeBinding(v));
            }
        }
        Self {
            occurrences,
            widening,
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
            widening: None,
            footprint: footprint_of(inv, definitions),
        };
        let table = DefinitionTable::new(definitions);
        follow(
            Body::Prop(&inv.body),
            &frame,
            table,
            &mut BTreeSet::new(),
            &mut out,
            &antecedent_nodes(&inv.body),
        );
        if out.widening.is_none()
            && let Some(v) =
                antecedent_scope_read(&inv.body, &frame, Some(table), &mut BTreeSet::new())
        {
            out.widening = Some(Widening::ScopeReadsBeforeBinding(v));
        }
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
        if self.widening.is_some() {
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

    /// Whether every delta the body sees is bounded to its cases, or some
    /// delta checks the rule whole, with one construct responsible: the
    /// first the plan met, or the first claim pattern binding no case
    /// variable. Derived from what `classify` consults, never stored
    /// apart from it.
    pub fn bounding(&self) -> Bounding {
        if let Some(w) = &self.widening {
            return Bounding::Whole(w.clone());
        }
        match self.occurrences.iter().find(|occ| occ.var_map.is_empty()) {
            Some(occ) => Bounding::Whole(Widening::NoCaseVariable(occ.predicate.clone())),
            None => Bounding::Bound,
        }
    }

    /// The cases the delta can affect.
    pub fn classify(&self, asserted: &[ClaimInstance], retracted: &[ClaimInstance]) -> Impact {
        // Nothing changed, so nothing is affected, whatever the body holds.
        if asserted.is_empty() && retracted.is_empty() {
            return Impact::Untouched;
        }
        if self.widening.is_some() {
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
        // Occurrence order, each case once. A delta names few cases, so
        // they are compared directly until a set pays for itself, which
        // keeps a large delta linear in its distinct cases.
        const SCAN_UP_TO: usize = 32;
        let mut cases: Vec<BTreeMap<Var, EvalValue>> = Vec::new();
        let mut seen: Option<HashSet<BTreeMap<Var, EvalValue>>> = None;
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
                if contradictory {
                    continue;
                }
                let fresh = match &mut seen {
                    Some(set) => set.insert(case.clone()),
                    None => !cases.contains(&case),
                };
                if fresh {
                    cases.push(case);
                    if seen.is_none() && cases.len() > SCAN_UP_TO {
                        seen = Some(cases.iter().cloned().collect());
                    }
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
    body: Body<'_>,
    frame: &Frame,
    table: DefinitionTable<'_>,
    seen: &mut BTreeSet<DefinitionName>,
    out: &mut ImpactPlan,
    antecedent: &HashSet<*const Prop>,
) {
    let mut calls = Vec::new();
    body.walk(&mut |n| {
        let here = match &n {
            Node::Prop(q) => Some(std::ptr::from_ref(*q)),
            _ => None,
        };
        match classify_node(&n, antecedent) {
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
                        Term::Wildcard | Term::Actor | Term::DecidedAt => {}
                    }
                }
                out.occurrences.push(Occurrence {
                    predicate: predicate.clone(),
                    guards,
                    var_map,
                });
            }
            NodeKind::Call(name, args) => {
                let in_antecedent = here.is_some_and(|p| antecedent.contains(&p));
                calls.push((name.clone(), args.to_vec(), in_antecedent));
            }
            NodeKind::ValueCall(name, args) => match call_terms(args) {
                Some(terms) => calls.push((name.clone(), terms, false)),
                None => {
                    out.widening
                        .get_or_insert_with(|| Widening::UnfollowedCall(name.clone()));
                    out.footprint = None;
                }
            },
            NodeKind::Grid(binder) => {
                if let Term::Var(v) = binder
                    && matches!(frame.get(v), Some(Traced::Case(_)))
                {
                    out.widening
                        .get_or_insert_with(|| Widening::ScopeReadsBeforeBinding(v.clone()));
                }
            }
            NodeKind::Widens(w) => {
                out.widening.get_or_insert(w);
            }
            NodeKind::Inert => {}
        }
    });
    for (name, args, in_antecedent) in calls {
        let followed = table.enter(&name, seen, |def, seen| {
            let callee: Frame = def
                .parameters
                .iter()
                .zip(&args)
                .filter_map(|(param, arg)| {
                    let traced = match arg {
                        Term::Literal(v) => Traced::Literal(v.clone()),
                        Term::Var(v) => frame.get(v)?.clone(),
                        Term::Wildcard | Term::Actor | Term::DecidedAt => return None,
                    };
                    Some((param.clone(), traced))
                })
                .collect();
            // A body called from the antecedent is antecedent whole; one
            // called elsewhere has its own antecedent, if it is a rule. A
            // figure's body has none.
            let inner = match &def.body {
                crate::ir::DefinitionBody::Prop(body) if in_antecedent => every_node(body),
                crate::ir::DefinitionBody::Prop(body) => antecedent_nodes(body),
                crate::ir::DefinitionBody::Value(_) => HashSet::new(),
            };
            follow(Body::of(&def.body), &callee, table, seen, out, &inner);
            true
        });
        // An undeclared or cyclic call contributes nothing to a walker
        // that only reads; here nothing would be unsound.
        if !followed {
            out.widening
                .get_or_insert_with(|| Widening::UnfollowedCall(name.clone()));
            out.footprint = None;
        }
    }
}

/// What a node means to the plan.
enum NodeKind<'a> {
    Claim(&'a PredicateName, &'a [Term]),
    Call(&'a DefinitionName, &'a [Term]),
    /// Ranging over a grid built from terms: inert, unless the binder
    /// takes a case variable's name, which the body would then read as
    /// the grid's hour and the plan as the case.
    Grid(&'a Term),
    /// A call to a figure: followed as a condition's call is when every
    /// argument is a term, since an expression carries no case variable
    /// into the body.
    ValueCall(&'a DefinitionName, &'a [ValueExpr]),
    /// A construct the bounding proof does not cover.
    Widens(Widening),
    Inert,
}

/// A definition body, whichever it names, for the walkers that follow
/// calls into it.
#[derive(Clone, Copy)]
enum Body<'a> {
    Prop(&'a Prop),
    Value(&'a ValueExpr),
}

impl<'a> Body<'a> {
    fn of(body: &'a crate::ir::DefinitionBody) -> Self {
        match body {
            crate::ir::DefinitionBody::Prop(p) => Body::Prop(p),
            crate::ir::DefinitionBody::Value(v) => Body::Value(v),
        }
    }

    fn walk(self, visit: &mut dyn FnMut(Node<'a>)) {
        match self {
            Body::Prop(p) => walk_prop(p, visit),
            Body::Value(v) => walk_value(v, visit),
        }
    }
}

/// The terms a value call carries, when every argument is one.
fn call_terms(args: &[ValueExpr]) -> Option<Vec<Term>> {
    args.iter()
        .map(|a| match a {
            ValueExpr::Term(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

/// Every proposition node inside the top-level antecedent, the one
/// place a disjunction takes part in which cases exist. An `or`
/// anywhere else is a truth test over the case's claims: its branches'
/// occurrences bind the case variables at their own positions, as a
/// conjunct's do, so it widens nothing.
fn antecedent_nodes(body: &Prop) -> HashSet<*const Prop> {
    match body {
        Prop::Implies { left, .. } => every_node(left),
        Prop::Forall { source, .. } => every_node(source),
        _ => HashSet::new(),
    }
}

fn every_node(p: &Prop) -> HashSet<*const Prop> {
    let mut out = HashSet::new();
    walk_prop(p, &mut |n| {
        if let Node::Prop(q) = n {
            out.insert(std::ptr::from_ref(q));
        }
    });
    out
}

fn classify_node<'a>(n: &Node<'a>, antecedent: &HashSet<*const Prop>) -> NodeKind<'a> {
    match n {
        Node::Prop(Prop::Claim { predicate, args }) => NodeKind::Claim(predicate, args),
        Node::Prop(Prop::Defined { name, args }) => NodeKind::Call(name, args),
        Node::Value(ValueExpr::Defined { name, args }) => NodeKind::ValueCall(name, args),
        Node::Prop(Prop::Pre(_)) => NodeKind::Widens(Widening::Pre),
        Node::Prop(q @ Prop::Or(_)) => {
            if antecedent.contains(&std::ptr::from_ref(*q)) {
                NodeKind::Widens(Widening::Or)
            } else {
                NodeKind::Inert
            }
        }
        Node::Prop(Prop::Xor(_, _)) => NodeKind::Widens(Widening::Xor),
        // A grid built from terms reads nothing and its binder is the
        // scope's own name, so ranging over it is inert; any other
        // collection is a value the plan cannot follow.
        Node::Prop(Prop::In(binder, source)) => match crate::check::grid::grid_call(source) {
            Some((from, until, step))
                if [from, until, step]
                    .iter()
                    .all(|a| matches!(a, ValueExpr::Term(_))) =>
            {
                NodeKind::Grid(binder)
            }
            _ => NodeKind::Widens(Widening::In),
        },
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
                NodeKind::Widens(Widening::SumOverExpression)
            }
        }
        Node::Value(ValueExpr::ValueOf { .. }) => NodeKind::Widens(Widening::ValueLookup),
        Node::Value(ValueExpr::Cond { .. }) => NodeKind::Widens(Widening::ConditionalValue),
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

/// The first case variable a closed scope in the rule's antecedent reads
/// before the conjunction has bound it, in evaluation order, through the
/// calls `table` follows. `None` when every scope reads only names bound
/// at its entry, which is what lets a case be seeded before the
/// antecedent is matched.
fn antecedent_scope_read(
    body: &Prop,
    frame: &Frame,
    table: Option<DefinitionTable<'_>>,
    seen: &mut BTreeSet<DefinitionName>,
) -> Option<Var> {
    let antecedent = match body {
        Prop::Implies { left, .. } => left,
        Prop::Forall { source, .. } => source,
        Prop::Not(inner) => inner,
        _ => return None,
    };
    let mut bound = BTreeSet::new();
    scope_read(antecedent, frame, &mut bound, table, seen)
}

/// Walk one conjunction in evaluation order, binding what its claim
/// patterns and followed calls bind, and asking of every closed scope
/// whether the case names it reads were bound before it.
fn scope_read(
    prop: &Prop,
    frame: &Frame,
    bound: &mut BTreeSet<Var>,
    table: Option<DefinitionTable<'_>>,
    seen: &mut BTreeSet<DefinitionName>,
) -> Option<Var> {
    match prop {
        Prop::And(ps) => ps
            .iter()
            .find_map(|p| scope_read(p, frame, bound, table, seen)),
        Prop::Claim { args, .. } => {
            for term in args {
                if let Term::Var(v) = term {
                    bound.insert(v.clone());
                }
            }
            None
        }
        Prop::Defined { name, args } => {
            // Without a table the call widens the plan on its own.
            let table = table?;
            let (found, exported) = table.enter(name, seen, |def, seen| {
                let mut callee_frame = Frame::new();
                let mut callee_bound = BTreeSet::new();
                for (param, arg) in def.parameters.iter().zip(args) {
                    match arg {
                        Term::Var(v) => {
                            if let Some(t) = frame.get(v) {
                                callee_frame.insert(param.clone(), t.clone());
                            }
                            if bound.contains(v) {
                                callee_bound.insert(param.clone());
                            }
                        }
                        Term::Literal(_) | Term::Actor | Term::DecidedAt => {
                            callee_bound.insert(param.clone());
                        }
                        Term::Wildcard => {}
                    }
                }
                let found = def
                    .body
                    .as_prop()
                    .and_then(|body| {
                        scope_read(body, &callee_frame, &mut callee_bound, Some(table), seen)
                    })
                    .and_then(|p| match callee_frame.get(&p) {
                        Some(Traced::Case(v)) => Some(v.clone()),
                        _ => None,
                    });
                // The caller's variables the body bound through its parameters.
                let exported: Vec<Var> = def
                    .parameters
                    .iter()
                    .zip(args)
                    .filter_map(|(param, arg)| match arg {
                        Term::Var(v) if callee_bound.contains(param) => Some(v.clone()),
                        _ => None,
                    })
                    .collect();
                (found, exported)
            });
            bound.extend(exported);
            found
        }
        Prop::Exists { .. } | Prop::Forall { .. } | Prop::Not(_) | Prop::Implies { .. } => {
            first_case_read(prop, frame, bound)
        }
        Prop::Eq(l, r)
        | Prop::Neq(l, r)
        | Prop::Compare {
            left: l, right: r, ..
        } => [l, r].into_iter().find_map(|e| {
            let mut found = None;
            crate::fold::walk_value(e, &mut |n| {
                if found.is_none()
                    && let Node::Value(v @ (ValueExpr::Sum { .. } | ValueExpr::Extremum { .. })) = n
                {
                    found = first_case_read_in_value(v, frame, bound);
                }
            });
            found
        }),
        // Each of these widens the plan on its own.
        Prop::Or(_) | Prop::Xor(_, _) | Prop::In(_, _) | Prop::Pre(_) => None,
    }
}

/// The first name under `scope` that traces to a case variable and is not
/// yet bound, binders included: a binder of a case name is a fresh
/// binding of it, which the plan would otherwise read as the case.
fn first_case_read(scope: &Prop, frame: &Frame, bound: &BTreeSet<Var>) -> Option<Var> {
    let mut found = None;
    walk_prop(scope, &mut |n| {
        if found.is_none() {
            found = case_read(&n, frame, bound);
        }
    });
    found
}

fn first_case_read_in_value(
    scope: &ValueExpr,
    frame: &Frame,
    bound: &BTreeSet<Var>,
) -> Option<Var> {
    let mut found = None;
    crate::fold::walk_value(scope, &mut |n| {
        if found.is_none() {
            found = case_read(&n, frame, bound);
        }
    });
    found
}

fn case_read(n: &Node<'_>, frame: &Frame, bound: &BTreeSet<Var>) -> Option<Var> {
    let name = match n {
        Node::Slot(Term::Var(v)) | Node::Value(ValueExpr::Term(Term::Var(v))) => v,
        Node::Binder(v) => v,
        _ => return None,
    };
    match frame.get(name) {
        Some(Traced::Case(case)) if !bound.contains(name) => Some(case.clone()),
        _ => None,
    }
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
    matches!(lit, Value::Subject(_) | Value::Bool(_) | Value::Decimal(_))
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
        (Value::Bool(a), EvalValue::Bool(b)) => a == b,
        (Value::Decimal(a), EvalValue::Decimal(b)) => a.value() == *b,
        _ => true,
    }
}
