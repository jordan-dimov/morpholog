//! Static analyses over the IR. The PostgreSQL read path uses them to load
//! only the claims a derived claim or transformation needs; other callers use
//! them to inspect a programme's predicates without running it.
//!
//! Structural descent goes through [`crate::fold`]; the matches here judge
//! nodes and match exhaustively (no `_` arm), so a new variant cannot slip
//! through and leave the read path loading too few claims.

use std::collections::BTreeSet;

use crate::definitions::DefinitionTable;
use crate::fold::{Node, walk_prop, walk_stmt, walk_value};
use crate::ir::{
    CompareOp, Definition, DefinitionName, DerivedClaim, OrderedDomain, PredicateArgKind,
    PredicateName, Program, Prop, Stmt, Term, TransformationName, ValueExpr, Var,
};
use crate::validate::ValidatedProgram;

/// Return the set of predicate names a proposition references anywhere in
/// its tree, including through defined calls. The PostgreSQL read path uses
/// it to load only the claims it needs instead of the whole
/// `morpholog.claims` table.
pub fn predicates_referenced_by_prop(
    prop: &Prop,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    prop_refs(
        prop,
        DefinitionTable::new(definitions),
        &mut BTreeSet::new(),
        out,
    );
}

/// Worker for [`predicates_referenced_by_prop`]. `seen` walks each
/// definition body once, so the walk also terminates on cyclic,
/// unvalidated IR.
pub(crate) fn prop_refs(
    prop: &Prop,
    definitions: DefinitionTable<'_>,
    seen: &mut BTreeSet<DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    walk_prop(prop, &mut |n| collect_refs(n, definitions, seen, out));
}

/// The predicates one node reads. A call reads whatever its definition's
/// body reads, transitively: otherwise the body would be evaluated
/// against claims never loaded. A retraction reads its pattern's
/// predicate: the pattern is matched against pre-state. An admission
/// only writes.
///
/// Every variant is named, so a new one does not compile until it says
/// whether it reads: a missed read would load too few claims.
fn collect_refs(
    node: Node<'_>,
    definitions: DefinitionTable<'_>,
    seen: &mut BTreeSet<DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match node {
        Node::Prop(prop) => match prop {
            Prop::Claim { predicate, .. } => {
                out.insert(predicate.clone());
            }
            Prop::Defined { name, .. } => {
                if seen.insert(name.clone())
                    && let Some(def) = definitions.get(name)
                {
                    prop_refs(&def.body, definitions, seen, out);
                }
            }
            Prop::In(_, _)
            | Prop::And(_)
            | Prop::Or(_)
            | Prop::Implies { .. }
            | Prop::Xor(_, _)
            | Prop::Not(_)
            | Prop::Pre(_)
            | Prop::Exists { .. }
            | Prop::Forall { .. }
            | Prop::Eq(_, _)
            | Prop::Neq(_, _)
            | Prop::Compare { .. } => {}
        },
        Node::Value(value) => match value {
            ValueExpr::ValueOf { predicate, .. } => {
                out.insert(predicate.clone());
            }
            ValueExpr::Term(_)
            | ValueExpr::Arith { .. }
            | ValueExpr::Sum { .. }
            | ValueExpr::Extremum { .. }
            | ValueExpr::Cond { .. }
            | ValueExpr::Call { .. } => {}
        },
        Node::Stmt(stmt) => match stmt {
            Stmt::Retract { predicate, .. } => {
                out.insert(predicate.clone());
            }
            Stmt::Require { .. }
            | Stmt::BindOne { .. }
            | Stmt::Let { .. }
            | Stmt::LetNewSubject { .. }
            | Stmt::Assert(_)
            | Stmt::For { .. }
            | Stmt::Emit(_) => {}
        },
        Node::Slot(_) | Node::Binder(_) => {}
    }
}

/// Return the set of predicate names a value expression references anywhere
/// in its tree. The value companion to [`predicates_referenced_by_prop`].
pub(crate) fn predicates_referenced_by_value(
    expr: &ValueExpr,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    let definitions = DefinitionTable::new(definitions);
    let seen = &mut BTreeSet::new();
    walk_value(expr, &mut |n| collect_refs(n, definitions, seen, out));
}

/// Return the predicate names `enumerate_derived(derived, state)` reads from
/// `state`: those referenced by the `domain` and by every value expression.
///
/// The derived claim's own `predicate` is excluded. It is the output, never
/// read from state.
pub fn predicates_referenced_by_derived(
    derived: &DerivedClaim,
    definitions: &[Definition],
) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    predicates_referenced_by_prop(&derived.domain, definitions, &mut out);
    for v in &derived.values {
        predicates_referenced_by_value(&v.expr, definitions, &mut out);
    }
    out
}

/// Return every predicate name a statement **reads from pre-state**,
/// through `for` bodies. The PG adapter uses it to load only the
/// predicates a transformation consults.
pub fn predicates_read_by_stmt(
    stmt: &Stmt,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    let definitions = DefinitionTable::new(definitions);
    let seen = &mut BTreeSet::new();
    walk_stmt(stmt, &mut |n| collect_refs(n, definitions, seen, out));
}

/// The predicates a statement admits, through `for` bodies. The loaded
/// pre-state must include them for the effective delta to be exact: admitting
/// a claim already present changes nothing, and only loaded state can tell.
pub fn predicates_asserted_by_stmt(stmt: &Stmt, out: &mut BTreeSet<PredicateName>) {
    walk_stmt(stmt, &mut |n| match n {
        Node::Stmt(Stmt::Assert(claim)) => {
            out.insert(claim.predicate.clone());
        }
        Node::Stmt(
            Stmt::Require { .. }
            | Stmt::BindOne { .. }
            | Stmt::Let { .. }
            | Stmt::LetNewSubject { .. }
            | Stmt::Retract { .. }
            | Stmt::For { .. }
            | Stmt::Emit(_),
        )
        | Node::Prop(_)
        | Node::Value(_)
        | Node::Slot(_)
        | Node::Binder(_) => {}
    });
}

/// Return the names of every transformation in `program` whose body asserts
/// `predicate` (including inside `for` bodies), in declaration order. The
/// explanation engine uses it to name who could supply a missing claim.
///
/// A match is only a *candidate* supplier: its own gates may still stop it
/// from supplying a given claim. This is one hop, with no instance matching.
pub fn transformations_asserting(program: &Program, predicate: &str) -> Vec<String> {
    program
        .transformations
        .iter()
        .filter(|t| {
            let mut asserted = BTreeSet::new();
            predicates_asserted_by(t, &mut asserted);
            asserted.iter().any(|p| p.as_str() == predicate)
        })
        .map(|t| t.name.to_string())
        .collect()
}

/// The predicates some transformation in this programme asserts. Stored
/// state can still hold claims of other predicates, so absence here is an
/// authoring signal, not proof a predicate is empty.
///
/// Derived claims do not count: they are read-side only, never admitted.
pub(crate) fn declared_supplier_predicates(program: &Program) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    for t in &program.transformations {
        predicates_asserted_by(t, &mut out);
    }
    out
}

/// Every predicate a transformation's body admits or retracts, including
/// inside `for` bodies. Unlike the assert-only walkers, retractions count:
/// they change shared state too.
pub fn predicates_written_by(
    transformation: &crate::ir::Transformation,
) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    for stmt in &transformation.body {
        walk_stmt(stmt, &mut |n| match n {
            Node::Stmt(Stmt::Assert(claim)) => {
                out.insert(claim.predicate.clone());
            }
            Node::Stmt(Stmt::Retract { predicate, .. }) => {
                out.insert(predicate.clone());
            }
            Node::Stmt(
                Stmt::Require { .. }
                | Stmt::BindOne { .. }
                | Stmt::Let { .. }
                | Stmt::LetNewSubject { .. }
                | Stmt::For { .. }
                | Stmt::Emit(_),
            )
            | Node::Prop(_)
            | Node::Value(_)
            | Node::Slot(_)
            | Node::Binder(_) => {}
        });
    }
    out
}

/// Whether the transformation has a top-level `require` or `bind`. A gate
/// inside a `for` guards one item, not the whole transformation; this
/// matches what the control matrix counts.
pub fn has_admission_gate(transformation: &crate::ir::Transformation) -> bool {
    transformation
        .body
        .iter()
        .any(|stmt| matches!(stmt, Stmt::Require { .. } | Stmt::BindOne { .. }))
}

/// Every predicate a transformation's body asserts (`admit`), descending
/// into `for` bodies. Used by the control matrix to decide which
/// invariants a transformation could trigger.
pub(crate) fn predicates_asserted_by(
    transformation: &crate::ir::Transformation,
    out: &mut BTreeSet<PredicateName>,
) {
    for stmt in &transformation.body {
        predicates_asserted_by_stmt(stmt, out);
    }
}

/// The unsupplied predicates that stop `prop` from binding on a fresh
/// ledger, or `None` if it could bind there. A blocker is a predicate the
/// programme never admits that `prop` truly requires: a required conjunct,
/// or every branch of an `or`. Only those are named, so a diagnostic points
/// at a real cause.
///
/// This says nothing about state already stored. Negation, implication,
/// `forall`, and value comparisons can still hold on a fresh ledger, so only
/// claim positions are examined.
pub(crate) fn undeclared_blockers(
    prop: &Prop,
    declared: &BTreeSet<PredicateName>,
    definitions: DefinitionTable<'_>,
) -> Option<BTreeSet<PredicateName>> {
    undeclared_blockers_inner(prop, declared, definitions, &mut BTreeSet::new())
}

fn undeclared_blockers_inner(
    prop: &Prop,
    declared: &BTreeSet<PredicateName>,
    definitions: DefinitionTable<'_>,
    seen: &mut BTreeSet<DefinitionName>,
) -> Option<BTreeSet<PredicateName>> {
    match prop {
        Prop::Claim { predicate, .. } => {
            if declared.contains(predicate) {
                None
            } else {
                Some(BTreeSet::from([predicate.clone()]))
            }
        }
        // A call blocks iff its body does.
        Prop::Defined { name, .. } => definitions.enter(name, seen, |def_, seen| {
            undeclared_blockers_inner(&def_.body, declared, definitions, seen)
        }),
        // A conjunction is blocked if any conjunct is; only the blocked
        // conjuncts are the cause.
        Prop::And(props) => {
            let mut blockers = BTreeSet::new();
            for p in props {
                if let Some(b) = undeclared_blockers_inner(p, declared, definitions, seen) {
                    blockers.extend(b);
                }
            }
            (!blockers.is_empty()).then_some(blockers)
        }
        // A disjunction binds if any branch can; it is blocked only when
        // every branch is, and then all of them are the cause.
        Prop::Or(props) => {
            let mut blockers = BTreeSet::new();
            for p in props {
                blockers.extend(undeclared_blockers_inner(p, declared, definitions, seen)?);
            }
            (!blockers.is_empty()).then_some(blockers)
        }
        Prop::Xor(left, right) => {
            let l = undeclared_blockers_inner(left, declared, definitions, seen)?;
            let mut blockers = undeclared_blockers_inner(right, declared, definitions, seen)?;
            blockers.extend(l);
            Some(blockers)
        }
        Prop::Exists { body, .. } | Prop::Pre(body) => {
            undeclared_blockers_inner(body, declared, definitions, seen)
        }
        Prop::Not(_)
        | Prop::Implies { .. }
        | Prop::Forall { .. }
        | Prop::Eq(_, _)
        | Prop::Neq(_, _)
        | Prop::Compare { .. }
        | Prop::In(_, _) => None,
    }
}

// ============================================================
// The effective-time vacuity lint: selecting the version in force at
// a date, and the invariant that guarantees one exists.
// ============================================================

/// Is this a date or timestamp comparison between two plain variables?
/// Computed operands do not count: the pattern compares dates as stored.
fn temporal_var_pair(prop: &Prop) -> Option<(CompareOp, &Var, &Var)> {
    let Prop::Compare {
        op,
        domain: OrderedDomain::Date | OrderedDomain::Timestamp,
        left,
        right,
    } = prop
    else {
        return None;
    };
    match (left.as_ref(), right.as_ref()) {
        (ValueExpr::Term(Term::Var(l)), ValueExpr::Term(Term::Var(r))) => Some((*op, l, r)),
        _ => None,
    }
}

/// One `not exists` excluder: the claimed predicate, the variables its
/// inner claim and binder carry, and the strict temporal comparisons inside.
type Excluder<'a> = (
    &'a PredicateName,
    BTreeSet<&'a Var>,
    Vec<(&'a Var, &'a Var)>,
);

/// Evidence gathered over one `and` scope: positive claims with their
/// variables, temporal variable comparisons, and `not exists` excluders.
#[derive(Default)]
struct SelectionEvidence<'a> {
    claims: Vec<(&'a PredicateName, BTreeSet<&'a Var>)>,
    /// Temporal variable pairs as (earlier, later), however they were
    /// spelled.
    nonstrict: Vec<(&'a Var, &'a Var)>,
    strict: Vec<(&'a Var, &'a Var)>,
    excluders: Vec<Excluder<'a>>,
}

fn claim_vars(args: &[Term]) -> BTreeSet<&Var> {
    args.iter()
        .filter_map(|t| match t {
            Term::Var(v) => Some(v),
            Term::Wildcard | Term::Literal(_) | Term::Actor => None,
        })
        .collect()
}

/// The predicates `P` that `prop` (an implication antecedent) selects "the
/// version in force at a date" for. That needs, in one `and` scope, a `P`
/// claim whose date variable is:
/// - on or before some bound (`<=`), and
/// - strictly compared with a `P` claim inside a `not exists` (no later one).
///
/// Each `or` branch is its own scope. Definition bodies are expanded without
/// renaming variables, so a name clash between caller and body could forge a
/// link; acceptable for a hint.
///
/// Deliberately incomplete: `implies`, `xor`, and `forall` are opaque, and a
/// selection with no date bound does not fire. Missing an odd spelling beats
/// flagging ordinary logic. The bound's direction matters; the strict
/// comparison's does not, since picking the latest or the earliest is
/// equally vacuous over an empty window.
pub(crate) fn governing_selections(
    prop: &Prop,
    definitions: DefinitionTable<'_>,
) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    selections_in_scope(prop, definitions, &mut out);
    out
}

fn selections_in_scope(
    prop: &Prop,
    definitions: DefinitionTable<'_>,
    out: &mut BTreeSet<PredicateName>,
) {
    let mut ev = SelectionEvidence::default();
    gather_selection_evidence(prop, definitions, &mut BTreeSet::new(), &mut ev, out);
    for (predicate, pvars) in &ev.claims {
        for (excluded, evars, strict) in &ev.excluders {
            if excluded != predicate {
                continue;
            }
            let candidate_var = strict.iter().find_map(|(a, b)| {
                if evars.contains(a) && pvars.contains(b) {
                    Some(*b)
                } else if evars.contains(b) && pvars.contains(a) {
                    Some(*a)
                } else {
                    None
                }
            });
            let Some(v) = candidate_var else { continue };
            // Only candidate <= bound counts. Candidate on or after a date
            // is not "the version in force".
            if ev.nonstrict.iter().any(|(earlier, _)| *earlier == v) {
                out.insert((*predicate).clone());
            }
        }
    }
}

fn gather_selection_evidence<'a>(
    prop: &'a Prop,
    definitions: DefinitionTable<'a>,
    seen: &mut BTreeSet<DefinitionName>,
    ev: &mut SelectionEvidence<'a>,
    out: &mut BTreeSet<PredicateName>,
) {
    match prop {
        Prop::Claim { predicate, args } => ev.claims.push((predicate, claim_vars(args))),
        Prop::Compare { .. } => {
            // Stored as (earlier, later) whichever way it was spelled; the
            // direction matters later.
            if let Some((op, l, r)) = temporal_var_pair(prop) {
                match op {
                    CompareOp::Le => ev.nonstrict.push((l, r)),
                    CompareOp::Ge => ev.nonstrict.push((r, l)),
                    CompareOp::Lt => ev.strict.push((l, r)),
                    CompareOp::Gt => ev.strict.push((r, l)),
                }
            }
        }
        Prop::And(props) => {
            for p in props {
                gather_selection_evidence(p, definitions, seen, ev, out);
            }
        }
        Prop::Pre(body) => gather_selection_evidence(body, definitions, seen, ev, out),
        Prop::Defined { name, .. } => definitions.enter(name, seen, |def_, seen| {
            gather_selection_evidence(&def_.body, definitions, seen, ev, out);
        }),
        Prop::Not(inner) => {
            if let Prop::Exists { binding, body } = inner.as_ref() {
                let mut inner_ev = SelectionEvidence::default();
                gather_selection_evidence(body, definitions, seen, &mut inner_ev, out);
                for (predicate, vars) in inner_ev.claims {
                    let mut with_binder = vars;
                    with_binder.insert(binding);
                    ev.excluders
                        .push((predicate, with_binder, inner_ev.strict.clone()));
                }
            }
        }
        // Each `or` branch is its own scope; evidence must not combine
        // across branches into a pattern no branch contains.
        Prop::Or(props) => {
            for p in props {
                selections_in_scope(p, definitions, out);
            }
        }
        // A positive `exists` is likewise a sub-scope of its own.
        Prop::Exists { body, .. } => selections_in_scope(body, definitions, out),
        Prop::Implies { .. }
        | Prop::Xor(_, _)
        | Prop::Forall { .. }
        | Prop::Eq(_, _)
        | Prop::Neq(_, _)
        | Prop::In(_, _) => {}
    }
}

/// The predicates an invariant body guarantees a dated witness for: "some
/// `P` in effect by a date", not just "some `P`". The witness is an `exists`
/// holding a `P` claim whose variable is on the earlier side of a temporal
/// comparison.
///
/// Only implication consequents count, found through `and`, `forall`, and
/// definitions. Inside a consequent, `and` unions and `or` intersects (only
/// one branch need hold); `implies`, `not`, `pre`, `xor`, and `forall` add
/// nothing. Whether the witness date matches a given selection's is not
/// checked.
pub(crate) fn guaranteed_dated_witnesses(
    invariant_body: &Prop,
    definitions: DefinitionTable<'_>,
) -> BTreeSet<PredicateName> {
    fn top(
        prop: &Prop,
        definitions: DefinitionTable<'_>,
        seen: &mut BTreeSet<DefinitionName>,
    ) -> BTreeSet<PredicateName> {
        match prop {
            Prop::Implies { right, .. } => algebra(right, definitions, seen),
            Prop::And(props) => props
                .iter()
                .flat_map(|p| top(p, definitions, seen))
                .collect(),
            Prop::Forall { body, .. } => top(body, definitions, seen),
            Prop::Defined { name, .. } => {
                definitions.enter(name, seen, |def_, seen| top(&def_.body, definitions, seen))
            }
            Prop::Claim { .. }
            | Prop::Or(_)
            | Prop::Exists { .. }
            | Prop::Not(_)
            | Prop::Pre(_)
            | Prop::Xor(_, _)
            | Prop::Eq(_, _)
            | Prop::Neq(_, _)
            | Prop::Compare { .. }
            | Prop::In(_, _) => BTreeSet::new(),
        }
    }
    fn algebra(
        prop: &Prop,
        definitions: DefinitionTable<'_>,
        seen: &mut BTreeSet<DefinitionName>,
    ) -> BTreeSet<PredicateName> {
        match prop {
            Prop::And(props) => props
                .iter()
                .flat_map(|p| algebra(p, definitions, seen))
                .collect(),
            Prop::Or(props) => {
                let mut branches = props.iter().map(|p| algebra(p, definitions, seen));
                let Some(first) = branches.next() else {
                    return BTreeSet::new();
                };
                branches.fold(first, |acc, b| acc.intersection(&b).cloned().collect())
            }
            Prop::Exists { binding: _, body } => {
                let mut ev = SelectionEvidence::default();
                let mut scratch = BTreeSet::new();
                gather_selection_evidence(body, definitions, seen, &mut ev, &mut scratch);
                let earlier_side: BTreeSet<&Var> = ev
                    .nonstrict
                    .iter()
                    .chain(ev.strict.iter())
                    .map(|(earlier, _)| *earlier)
                    .collect();
                // One of the claim's own variables must be on the earlier
                // side. A `P` dated after the bound fills no gap before it.
                ev.claims
                    .iter()
                    .filter(|(_, vars)| vars.iter().any(|v| earlier_side.contains(v)))
                    .map(|(p, _)| (*p).clone())
                    .collect()
            }
            Prop::Defined { name, .. } => definitions.enter(name, seen, |def_, seen| {
                algebra(&def_.body, definitions, seen)
            }),
            Prop::Claim { .. }
            | Prop::Not(_)
            | Prop::Implies { .. }
            | Prop::Pre(_)
            | Prop::Xor(_, _)
            | Prop::Forall { .. }
            | Prop::Eq(_, _)
            | Prop::Neq(_, _)
            | Prop::Compare { .. }
            | Prop::In(_, _) => BTreeSet::new(),
        }
    }
    top(invariant_body, definitions, &mut BTreeSet::new())
}

// ============================================================
// Argument kinds per transformation: the embedder's input contract.
// ============================================================

/// The resolved kind of one transformation parameter, from every position
/// it is used in across the body. Each variant asks the embedder for
/// different handling, so they are kept apart.
///
/// `Ambiguous` exists because a programme can validate with a parameter
/// used at different kinds in different `or` branches, and the runtime
/// then takes whichever branch fits the input. Refusing a schema would be
/// too strict; naming one kind would be false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamKind {
    /// A specific kind. The embedder can derive a typed input field directly.
    Concrete(PredicateArgKind),
    /// The parameter is used only at positions declared
    /// `PredicateArgKind::Any`. The kind cannot be narrowed; the embedder
    /// should accept input but flag the missing constraint.
    Polymorphic,
    /// The parameter is never used where a kind is known: likely dead or a
    /// modelling mistake. The embedder should surface it.
    Unconstrained,
    /// The parameter is used at two or more concrete kinds in separate
    /// scopes (typically `or` branches). Lists each kind once, in
    /// `PredicateArgKind` declaration order. The embedder can render it as
    /// JSON Schema `anyOf` or report it as a modelling issue.
    Ambiguous(Vec<PredicateArgKind>),
    /// A collection iterated by `for` / `forall` whose element kind is
    /// known from how the loop variable is used, e.g.
    /// `Collection(Concrete(Subject))` for a list of subjects. Elements may
    /// nest. If the loop variable's kind is never known, the parameter
    /// stays `Concrete(Collection)` instead.
    Collection(Box<ParamKind>),
}

/// Errors from per-transformation argument-kind analysis. Validation errors
/// cannot occur: the API takes a [`ValidatedProgram`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnalysisError {
    /// No transformation declared with that name.
    #[error("unknown transformation `{name}`")]
    UnknownTransformation { name: TransformationName },
}

/// The embedder's input contract for one transformation: the resolved
/// [`ParamKind`] of every parameter, in declaration order (never hash
/// order; generated forms and clients depend on it).
///
/// The static checker observes it while it walks: every position a
/// parameter reaches, through `let` aliases, `or` branches, `require`,
/// loop bindings and definition calls, records the kind it was used as,
/// and the parameter's kind set projects to a [`ParamKind`]. There is
/// no second walk.
///
/// Takes a [`ValidatedProgram`], so the programme is known valid and is
/// not re-validated here.
pub fn transformation_param_kinds(
    program: &ValidatedProgram<'_>,
    name: &TransformationName,
) -> Result<Vec<(Var, ParamKind)>, AnalysisError> {
    let inner = program.as_program();
    let mut checked = crate::check::check_program(inner);
    checked
        .param_kinds
        .remove(name.as_str())
        .ok_or_else(|| AnalysisError::UnknownTransformation { name: name.clone() })
}

/// Turn a parameter's observed kinds into a [`ParamKind`]. Seen only in `Any`
/// slots means `Polymorphic`; conflicting concrete kinds mean `Ambiguous`, in the
/// `BTreeSet`'s `PredicateArgKind` declaration order.
pub(crate) fn project(observations: &BTreeSet<PredicateArgKind>) -> ParamKind {
    let has_any = observations.contains(&PredicateArgKind::Any);
    let concrete: Vec<PredicateArgKind> = observations
        .iter()
        .filter(|k| **k != PredicateArgKind::Any)
        .cloned()
        .collect();
    match (concrete.len(), has_any) {
        (0, false) => ParamKind::Unconstrained,
        (0, true) => ParamKind::Polymorphic,
        (1, _) => ParamKind::Concrete(concrete[0].clone()),
        _ => ParamKind::Ambiguous(concrete),
    }
}

#[cfg(test)]
mod supplier_tests {
    use super::*;
    use crate::ir::DerivedClaim;
    use crate::ir_builder::{assert_, claim, program, transformation};

    // A derived claim is read-side only, never admitted, so it is not a
    // supplier.
    #[test]
    fn a_derived_only_predicate_is_not_an_admitted_supplier() {
        let prog = program("p")
            .transformations(vec![transformation(
                "capture",
                vec![],
                vec![assert_("Trade", vec![])],
            )])
            .derived_claims(vec![DerivedClaim {
                predicate: "TradeTotal".into(),
                keys: vec![],
                values: vec![],
                domain: claim("Trade", vec![]),
            }])
            .build();
        let suppliers = declared_supplier_predicates(&prog);
        assert!(suppliers.contains(&PredicateName::from("Trade")));
        assert!(!suppliers.contains(&PredicateName::from("TradeTotal")));
    }
}

#[cfg(test)]
mod blocker_tests {
    use super::*;
    use crate::ir::Prop;
    use crate::ir_builder::{and, claim, exists, forall, implies, not, or, pre, value_of, xor};

    fn pset(names: &[&str]) -> BTreeSet<PredicateName> {
        names.iter().map(|n| PredicateName::from(*n)).collect()
    }

    fn blockers(prop: &Prop, supplied: &[&str]) -> Option<BTreeSet<PredicateName>> {
        undeclared_blockers(prop, &pset(supplied), DefinitionTable::new(&[]))
    }

    fn p(name: &str) -> Prop {
        claim(name, vec![])
    }

    #[test]
    fn unsupplied_claim_is_a_blocker() {
        assert_eq!(blockers(&p("Foo"), &[]), Some(pset(&["Foo"])));
    }

    #[test]
    fn supplied_claim_may_bind() {
        assert_eq!(blockers(&p("Foo"), &["Foo"]), None);
    }

    #[test]
    fn and_collects_only_dead_conjuncts() {
        let prop = and(vec![p("Dead"), p("Live")]);
        assert_eq!(blockers(&prop, &["Live"]), Some(pset(&["Dead"])));
    }

    #[test]
    fn and_with_an_optional_dead_or_names_only_the_mandatory_conjunct() {
        let prop = and(vec![p("Dead"), or(vec![p("Maybe"), p("Live")])]);
        assert_eq!(blockers(&prop, &["Live"]), Some(pset(&["Dead"])));
    }

    #[test]
    fn or_blocks_only_when_every_branch_is_unsupplied() {
        assert_eq!(
            blockers(&or(vec![p("A"), p("B")]), &[]),
            Some(pset(&["A", "B"]))
        );
        assert_eq!(blockers(&or(vec![p("A"), p("Live")]), &["Live"]), None);
    }

    #[test]
    fn xor_blocks_only_when_neither_side_may_bind() {
        assert_eq!(blockers(&xor(p("A"), p("Live")), &["Live"]), None);
        assert_eq!(blockers(&xor(p("A"), p("B")), &[]), Some(pset(&["A", "B"])));
    }

    #[test]
    fn exists_and_pre_propagate_their_body() {
        assert_eq!(blockers(&exists("x", p("A")), &[]), Some(pset(&["A"])));
        assert_eq!(blockers(&pre(p("A")), &[]), Some(pset(&["A"])));
    }

    #[test]
    fn negation_implication_and_forall_never_block() {
        assert_eq!(blockers(&not(p("A")), &[]), None);
        assert_eq!(blockers(&implies(p("A"), p("B")), &[]), None);
        assert_eq!(blockers(&forall("x", p("A"), p("B")), &[]), None);
    }

    #[test]
    fn value_position_predicates_are_never_blockers() {
        let prop = and(vec![
            p("Dead"),
            Prop::Eq(
                Box::new(value_of("ValA", vec![])),
                Box::new(value_of("ValB", vec![])),
            ),
        ]);
        assert_eq!(blockers(&prop, &[]), Some(pset(&["Dead"])));
    }
}
