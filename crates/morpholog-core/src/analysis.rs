//! Static analyses over the IR. The PostgreSQL read path uses them to load
//! only the claims a derived claim or transformation needs; other callers use
//! them to inspect a programme's predicates without running it.
//!
//! Every walker matches exhaustively (no `_` arm), so a new `Prop`,
//! `ValueExpr`, or `Stmt` variant cannot slip through and leave the read path
//! loading too few claims.

use std::collections::{BTreeSet, HashMap};

use crate::definitions::DefinitionTable;
use crate::ir::{
    ArgDecl, ArithOp, Builtin, CompareOp, Definition, DefinitionName, DerivedClaim, OrderedDomain,
    PredicateArgKind, PredicateName, Program, Prop, Stmt, Term, TransformationName, ValueExpr, Var,
};
use crate::validate::ValidatedProgram;

/// Return the set of predicate names a proposition references anywhere in
/// its tree, including through defined calls. The PostgreSQL read path uses
/// it to load only the claims it needs instead of the whole
/// `morpholog.claims` table.
///
/// A missed variant would load too few claims and give wrong answers, so the
/// match is exhaustive: a new `Prop` variant does not compile until handled.
/// `In` reads only terms and contributes nothing.
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

/// Recursive worker for [`predicates_referenced_by_prop`]. `seen` walks each
/// definition body once, so the walk also terminates on cyclic,
/// unvalidated IR.
pub(crate) fn prop_refs(
    prop: &Prop,
    definitions: DefinitionTable<'_>,
    seen: &mut BTreeSet<DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match prop {
        // A call reads whatever its definition's body reads, transitively.
        // Otherwise the body would be evaluated against claims never loaded.
        Prop::Defined { name, .. } => {
            if seen.insert(name.clone())
                && let Some(def) = definitions.get(name)
            {
                prop_refs(&def.body, definitions, seen, out);
            }
        }
        Prop::Claim { predicate, .. } => {
            out.insert(predicate.clone());
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            prop_refs(left, definitions, seen, out);
            prop_refs(right, definitions, seen, out);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                prop_refs(p, definitions, seen, out);
            }
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => {
            prop_refs(p, definitions, seen, out);
        }
        Prop::Eq(l, r)
        | Prop::Neq(l, r)
        | Prop::Compare {
            left: l, right: r, ..
        } => {
            value_refs(l, definitions, seen, out);
            value_refs(r, definitions, seen, out);
        }
        Prop::Forall { source, body, .. } => {
            prop_refs(source, definitions, seen, out);
            prop_refs(body, definitions, seen, out);
        }
        Prop::In(_, _) => {
            // No predicate references; operates on Terms only.
        }
    }
}

/// Return the set of predicate names a value expression references anywhere
/// in its tree. The value companion to [`predicates_referenced_by_prop`];
/// the two recurse into each other (`Sum`'s body is a `Prop`). Exhaustive
/// for the same reason.
pub(crate) fn predicates_referenced_by_value(
    expr: &ValueExpr,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    value_refs(
        expr,
        DefinitionTable::new(definitions),
        &mut BTreeSet::new(),
        out,
    );
}

/// Recursive worker for [`predicates_referenced_by_value`].
fn value_refs(
    expr: &ValueExpr,
    definitions: DefinitionTable<'_>,
    seen: &mut BTreeSet<DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match expr {
        ValueExpr::ValueOf {
            predicate, default, ..
        } => {
            out.insert(predicate.clone());
            if let Some(d) = default {
                value_refs(d, definitions, seen, out);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            value_refs(left, definitions, seen, out);
            value_refs(right, definitions, seen, out);
        }
        ValueExpr::Sum { value, body, .. } => {
            value_refs(value, definitions, seen, out);
            prop_refs(body, definitions, seen, out);
        }
        ValueExpr::Extremum { body, .. } => {
            prop_refs(body, definitions, seen, out);
        }
        // Only one branch runs, but we cannot know which statically, so
        // load what the condition and both branches read.
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            prop_refs(when, definitions, seen, out);
            value_refs(then, definitions, seen, out);
            value_refs(otherwise, definitions, seen, out);
        }
        // A builtin reads nothing itself; its footprint is its arguments'.
        ValueExpr::Call { args, .. } => {
            for a in args {
                value_refs(a, definitions, seen, out);
            }
        }
        ValueExpr::Term(_) => {
            // No predicate references; operates on a Term only.
        }
    }
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

/// Return every predicate name a statement **reads from pre-state**. The PG
/// adapter uses it to load only the predicates a transformation consults.
///
/// - `Require` / `BindOne` / `Let` value / `For` collection: read.
/// - `Retract`: read. Its pattern is matched against pre-state to find
///   the claims to retract.
/// - `Assert`: not read. The claim is only written.
/// - `Emit` / `LetNewSubject`: nothing.
/// - `For` body: recurses.
///
/// Exhaustive, so a new `Stmt` variant must declare its reads.
pub fn predicates_read_by_stmt(
    stmt: &Stmt,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    match stmt {
        Stmt::Require { prop: p, .. } | Stmt::BindOne { prop: p, .. } => {
            predicates_referenced_by_prop(p, definitions, out)
        }
        Stmt::Let { value, .. } => predicates_referenced_by_value(value, definitions, out),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(_) => {
            // Written, not read.
        }
        Stmt::Retract { predicate, .. } => {
            // The pattern is matched against pre-state, so load it.
            out.insert(predicate.clone());
        }
        Stmt::For {
            collection, body, ..
        } => {
            predicates_referenced_by_value(collection, definitions, out);
            for inner in body {
                predicates_read_by_stmt(inner, definitions, out);
            }
        }
        Stmt::Emit(_) => {}
    }
}

/// The predicates a statement admits, through `For` bodies. The loaded
/// pre-state must include them for the effective delta to be exact: admitting
/// a claim already present changes nothing, and only loaded state can tell.
pub fn predicates_asserted_by_stmt(stmt: &Stmt, out: &mut BTreeSet<PredicateName>) {
    match stmt {
        Stmt::Assert(claim) => {
            out.insert(claim.predicate.clone());
        }
        Stmt::For { body, .. } => {
            for inner in body {
                predicates_asserted_by_stmt(inner, out);
            }
        }
        Stmt::Require { .. }
        | Stmt::BindOne { .. }
        | Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Retract { .. }
        | Stmt::Emit(_) => {}
    }
}

/// Return the names of every transformation in `program` whose body asserts
/// `predicate` (including inside `For` bodies), in declaration order. The
/// explanation engine uses it to name who could supply a missing claim.
///
/// A match is only a *candidate* supplier: its own gates may still stop it
/// from supplying a given claim. This is one hop, with no instance matching.
pub fn transformations_asserting(program: &Program, predicate: &str) -> Vec<String> {
    program
        .transformations
        .iter()
        .filter(|t| t.body.iter().any(|s| stmt_asserts(s, predicate)))
        .map(|t| t.name.to_string())
        .collect()
}

/// Whether a statement (or, for `For`, its body) asserts `predicate`.
/// Exhaustive, so a new variant cannot hide a supplier from `explain`.
fn stmt_asserts(stmt: &Stmt, predicate: &str) -> bool {
    match stmt {
        Stmt::Assert(claim) => claim.predicate.as_str() == predicate,
        Stmt::For { body, .. } => body.iter().any(|s| stmt_asserts(s, predicate)),
        Stmt::Require { .. }
        | Stmt::BindOne { .. }
        | Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Retract { .. }
        | Stmt::Emit(_) => false,
    }
}

/// The predicates some transformation in this programme asserts. Stored
/// state can still hold claims of other predicates, so absence here is an
/// authoring signal, not proof a predicate is empty.
///
/// Derived claims do not count: they are read-side only, never admitted.
pub(crate) fn declared_supplier_predicates(program: &Program) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    for t in &program.transformations {
        for s in &t.body {
            collect_asserted(s, &mut out);
        }
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
        collect_written(stmt, &mut out);
    }
    out
}

fn collect_written(stmt: &Stmt, out: &mut BTreeSet<PredicateName>) {
    match stmt {
        Stmt::Assert(claim) => {
            out.insert(claim.predicate.clone());
        }
        Stmt::Retract { predicate, .. } => {
            out.insert(predicate.clone());
        }
        Stmt::For { body, .. } => {
            for s in body {
                collect_written(s, out);
            }
        }
        Stmt::Require { .. }
        | Stmt::BindOne { .. }
        | Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Emit(_) => {}
    }
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
        collect_asserted(stmt, out);
    }
}

/// Every predicate a statement asserts (descending into `For` bodies).
fn collect_asserted(stmt: &Stmt, out: &mut BTreeSet<PredicateName>) {
    match stmt {
        Stmt::Assert(claim) => {
            out.insert(claim.predicate.clone());
        }
        Stmt::For { body, .. } => {
            for s in body {
                collect_asserted(s, out);
            }
        }
        Stmt::Require { .. }
        | Stmt::BindOne { .. }
        | Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Retract { .. }
        | Stmt::Emit(_) => {}
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

/// Compute the embedder's input contract for one transformation: the
/// resolved [`ParamKind`] of every parameter, in declaration order (never
/// hash order; generated forms and clients depend on it).
///
/// This is not the static checker's walk. The checker keeps bindings made
/// inside `require`, `or`, `sum`, and `for` local, which is right at
/// runtime. For an input contract it is wrong: a parameter used only
/// inside `require` is still supplied from outside and still has a kind.
/// So this walk collects kinds from every position into one flat scope.
///
/// Kinds are tracked for all variables, so a parameter can pick one up
/// through a `let` alias. `x = literal` does not give `x` a kind.
///
/// Takes a [`ValidatedProgram`], so the programme is known valid and is
/// not re-validated here.
pub fn transformation_param_kinds(
    program: &ValidatedProgram<'_>,
    name: &TransformationName,
) -> Result<Vec<(Var, ParamKind)>, AnalysisError> {
    let inner = program.as_program();
    let transformation = inner
        .transformation(name.as_str())
        .ok_or_else(|| AnalysisError::UnknownTransformation { name: name.clone() })?;

    let mut collector = ParamCollector::new(inner);
    for stmt in &transformation.body {
        collector.walk_stmt(stmt);
    }

    // Observations already reached every alias when made, so this is a
    // direct lookup.
    Ok(transformation
        .parameters
        .iter()
        .map(|param| {
            let observed = collector
                .observations
                .get(param)
                .cloned()
                .unwrap_or_default();
            // A collection iterated with a known element kind reports that
            // element kind. Without one it stays `Concrete(Collection)`. Used
            // as both a collection and a scalar, it is `Ambiguous`.
            let kind = if observed.len() == 1
                && observed.contains(&PredicateArgKind::Collection)
                && let Some(element) = collector.collection_elements.get(param)
            {
                ParamKind::Collection(Box::new(project(element.clone())))
            } else {
                project(observed)
            };
            (param.clone(), kind)
        })
        .collect())
}

/// Turn a parameter's observed kinds into a [`ParamKind`]. Seen only in `Any`
/// slots means `Polymorphic`; conflicting concrete kinds mean `Ambiguous`, in the
/// `BTreeSet`'s `PredicateArgKind` declaration order.
fn project(observations: BTreeSet<PredicateArgKind>) -> ParamKind {
    let has_any = observations.contains(&PredicateArgKind::Any);
    let concrete: Vec<PredicateArgKind> = observations
        .into_iter()
        .filter(|k| *k != PredicateArgKind::Any)
        .collect();
    match (concrete.len(), has_any) {
        (0, false) => ParamKind::Unconstrained,
        (0, true) => ParamKind::Polymorphic,
        (1, _) => ParamKind::Concrete(concrete[0].clone()),
        _ => ParamKind::Ambiguous(concrete),
    }
}

/// Walker state for [`transformation_param_kinds`]. Collects kinds for every
/// variable in one flat scope; parameters are picked out at the end.
///
/// Each variable keeps a *set* of kinds. Keeping only one would yield a
/// schema that rejects valid inputs for another `or` branch.
///
/// `Eq` / `Neq` do not pass a literal's kind to a variable. A parameter
/// used only as `param = literal` comes out `Unconstrained`, which tells
/// the embedder the model leans on an unstated assumption.
struct ParamCollector<'a> {
    predicates: HashMap<&'a str, &'a [ArgDecl]>,
    intents: HashMap<&'a str, &'a [ArgDecl]>,
    /// Kinds inferred per definition parameter from its body, computed
    /// callees first. A call argument observes every kind in its
    /// parameter's set.
    definition_params: HashMap<String, Vec<BTreeSet<PredicateArgKind>>>,
    observations: HashMap<Var, BTreeSet<PredicateArgKind>>,
    /// Element kinds per collection variable: in `for x in coll` (or
    /// `forall`), the kinds seen for `x` are `coll`'s element kinds.
    /// Feeds [`ParamKind::Collection`].
    collection_elements: HashMap<Var, BTreeSet<PredicateArgKind>>,
    /// Current alias class per variable. Only `let x = y` creates an alias.
    /// Rebinding a name drops it from its class first, so later
    /// observations do not reach its old aliases. An observation reaches
    /// every member of the class at the time it is made.
    ///
    /// A variable absent here is its own singleton class. Stored classes
    /// have at least two members.
    current_class: HashMap<Var, BTreeSet<Var>>,
}

impl<'a> ParamCollector<'a> {
    fn new(program: &'a Program) -> Self {
        let predicates = program
            .predicates
            .iter()
            .map(|d| (d.name.as_str(), d.args.as_slice()))
            .collect();
        let intents = program
            .intents
            .iter()
            .map(|d| (d.name.as_str(), d.args.as_slice()))
            .collect();
        let mut collector = Self {
            predicates,
            intents,
            definition_params: HashMap::new(),
            observations: HashMap::new(),
            collection_elements: HashMap::new(),
            current_class: HashMap::new(),
        };
        // Walk each definition body, callees first, to learn its
        // parameters' kinds. On a cycle (invalid IR) the map stays empty
        // and calls contribute nothing.
        if let Ok(order) = crate::definitions::definition_topo_order(&program.definitions) {
            for i in order {
                let def = &program.definitions[i];
                let mut sub = Self {
                    predicates: collector.predicates.clone(),
                    intents: collector.intents.clone(),
                    definition_params: collector.definition_params.clone(),
                    observations: HashMap::new(),
                    collection_elements: HashMap::new(),
                    current_class: HashMap::new(),
                };
                sub.walk_prop(&def.body);
                let param_sets = def
                    .parameters
                    .iter()
                    .map(|param| sub.observations.get(param).cloned().unwrap_or_default())
                    .collect();
                collector
                    .definition_params
                    .insert(def.name.to_string(), param_sets);
            }
        }
        collector
    }

    /// The kind of a value expression when it is plain: a literal's kind, or
    /// a variable seen at exactly one kind so far. `None` otherwise. Used
    /// only to decide whether arithmetic forces the other operand's kind.
    fn shallow_value_kind(&self, v: &ValueExpr) -> Option<PredicateArgKind> {
        match v {
            ValueExpr::Term(Term::Literal(lit)) => Some(match lit {
                crate::ir::Value::Subject(_) => PredicateArgKind::Subject,
                crate::ir::Value::Decimal(_) => PredicateArgKind::Decimal,
                crate::ir::Value::Date(_) => PredicateArgKind::Date,
                crate::ir::Value::Timestamp(_) => PredicateArgKind::Timestamp,
                crate::ir::Value::Duration(_) => PredicateArgKind::Duration,
                crate::ir::Value::CalendarSpan(_) => PredicateArgKind::CalendarSpan,
                crate::ir::Value::Quantity { unit, .. } => PredicateArgKind::Quantity(unit.clone()),
            }),
            ValueExpr::Term(Term::Var(name)) => {
                let observed = self.observations.get(name)?;
                if observed.len() == 1 {
                    observed.iter().next().cloned()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Observe `name` at `kind`, and every variable currently aliased to it.
    fn observe(&mut self, name: &Var, kind: PredicateArgKind) {
        let members: Vec<Var> = match self.current_class.get(name) {
            Some(class) => class.iter().cloned().collect(),
            None => vec![name.clone()],
        };
        for member in members {
            self.observations
                .entry(member)
                .or_default()
                .insert(kind.clone());
        }
    }

    /// Remove `name` from its alias class; the rest stay aliased. Called on
    /// rebinding, so the old aliases do not see the new binding's kinds.
    fn invalidate(&mut self, name: &Var) {
        let Some(mut class) = self.current_class.remove(name) else {
            return;
        };
        class.remove(name);
        match class.len() {
            0 | 1 => {
                // A lone member needs no stored class.
                if let Some(only) = class.into_iter().next() {
                    self.current_class.remove(&only);
                }
            }
            _ => {
                for member in &class {
                    self.current_class.insert(member.clone(), class.clone());
                }
            }
        }
    }

    /// Merge `name` and `alias`, with their classes, into one class. Called
    /// for `let name = alias`, after `invalidate(name)`.
    fn add_alias(&mut self, name: &Var, alias: &Var) {
        let mut merged: BTreeSet<Var> = BTreeSet::new();
        merged.insert(name.clone());
        merged.insert(alias.clone());
        if let Some(c) = self.current_class.get(name) {
            merged.extend(c.iter().cloned());
        }
        if let Some(c) = self.current_class.get(alias) {
            merged.extend(c.iter().cloned());
        }
        for member in &merged {
            self.current_class.insert(member.clone(), merged.clone());
        }
    }

    /// Walk a statement. Exhaustive, so a new `Stmt` variant must declare
    /// what it observes.
    fn walk_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Require { prop, .. } | Stmt::BindOne { prop, .. } => self.walk_prop(prop),
            Stmt::Let { name, value } => {
                // Rebinding makes a new variable: drop old aliases first.
                self.invalidate(name);
                if let ValueExpr::Term(Term::Var(alias)) = value {
                    self.add_alias(name, alias);
                }
                self.walk_value(value, None);
            }
            Stmt::LetNewSubject { name } => {
                // Rebinding, as for `Let`; a new subject is a Subject.
                self.invalidate(name);
                self.observe(name, PredicateArgKind::Subject);
            }
            Stmt::Assert(claim) => self.observe_claim_args(claim.predicate.as_str(), &claim.args),
            Stmt::Retract { predicate, args } => {
                self.observe_claim_args(predicate.as_str(), args);
            }
            Stmt::For {
                binding,
                collection,
                body,
            } => {
                self.walk_value(collection, Some(PredicateArgKind::Collection));
                // The loop binding shadows any outer variable of the same
                // name, such as a parameter. Save its outer state, walk the
                // body, then restore, so its loop kinds do not leak out.
                // Kinds seen for other outer variables inside the body stay.
                let saved_obs = self.observations.get(binding).cloned();
                let saved_class = self.current_class.get(binding).cloned();
                self.invalidate(binding);
                self.observations.remove(binding);

                for inner in body {
                    self.walk_stmt(inner);
                }

                // The binding's kinds are the collection's element kinds.
                // Record them if the collection is a plain variable.
                if let ValueExpr::Term(Term::Var(coll_var)) = collection
                    && let Some(elem_obs) = self.observations.get(binding)
                {
                    let elem_obs = elem_obs.clone();
                    self.collection_elements
                        .entry(coll_var.clone())
                        .or_default()
                        .extend(elem_obs);
                }

                // Discard the body's effects on the binding's state.
                self.invalidate(binding);
                self.observations.remove(binding);

                // Restore outer state.
                if let Some(obs) = saved_obs {
                    self.observations.insert(binding.clone(), obs);
                }
                if let Some(class) = saved_class {
                    for member in &class {
                        self.current_class.insert(member.clone(), class.clone());
                    }
                }
            }
            Stmt::Emit(intent) => self.observe_intent_args(intent.name.as_str(), &intent.args),
        }
    }

    /// Walk a proposition. Exhaustive over `Prop`.
    fn walk_prop(&mut self, prop: &Prop) {
        match prop {
            Prop::Claim { predicate, args } => {
                self.observe_claim_args(predicate.as_str(), args);
            }
            Prop::Defined { name, args } => {
                if let Some(param_sets) = self.definition_params.get(name.as_str()).cloned() {
                    for (arg, kinds) in args.iter().zip(param_sets) {
                        if let Term::Var(v) = arg {
                            for kind in kinds {
                                self.observe(v, kind);
                            }
                        }
                    }
                }
            }
            Prop::And(items) | Prop::Or(items) => {
                for item in items {
                    self.walk_prop(item);
                }
            }
            Prop::Xor(left, right) | Prop::Implies { left, right } => {
                self.walk_prop(left);
                self.walk_prop(right);
            }
            Prop::Not(inner) | Prop::Pre(inner) | Prop::Exists { body: inner, .. } => {
                self.walk_prop(inner);
            }
            Prop::Forall {
                binding,
                source,
                body,
            } => {
                // `forall x in xs` has source `In(x, xs)`, which observes
                // `xs` as a collection.
                self.walk_prop(source);

                // Same shadowing as `Stmt::For`: save, clear, walk, record
                // the element kind, restore.
                let saved_obs = self.observations.get(binding).cloned();
                let saved_class = self.current_class.get(binding).cloned();
                self.invalidate(binding);
                self.observations.remove(binding);

                self.walk_prop(body);

                // The binding's kinds are the source collection's element
                // kinds.
                if let Prop::In(_, Term::Var(coll)) = source.as_ref()
                    && let Some(elem_obs) = self.observations.get(binding)
                {
                    let elem_obs = elem_obs.clone();
                    self.collection_elements
                        .entry(coll.clone())
                        .or_default()
                        .extend(elem_obs);
                }

                self.invalidate(binding);
                self.observations.remove(binding);
                if let Some(obs) = saved_obs {
                    self.observations.insert(binding.clone(), obs);
                }
                if let Some(class) = saved_class {
                    for member in &class {
                        self.current_class.insert(member.clone(), class.clone());
                    }
                }
            }
            Prop::Compare {
                domain,
                left,
                right,
                ..
            } => {
                let kind = match domain {
                    // If either side is known to carry a unit, both sides
                    // take that quantity kind: `settled <= due` gives
                    // `settled` the unit of `due`. Otherwise bare decimal.
                    OrderedDomain::Decimal => {
                        match (
                            self.shallow_value_kind(left),
                            self.shallow_value_kind(right),
                        ) {
                            (Some(q @ PredicateArgKind::Quantity(_)), _)
                            | (_, Some(q @ PredicateArgKind::Quantity(_))) => q,
                            _ => PredicateArgKind::Decimal,
                        }
                    }
                    OrderedDomain::Date => PredicateArgKind::Date,
                    OrderedDomain::Timestamp => PredicateArgKind::Timestamp,
                    OrderedDomain::Duration => PredicateArgKind::Duration,
                };
                self.walk_value(left, Some(kind.clone()));
                self.walk_value(right, Some(kind));
            }
            Prop::Eq(left, right) | Prop::Neq(left, right) => {
                // One operand does not pass its kind to the other.
                self.walk_value(left, None);
                self.walk_value(right, None);
            }
            Prop::In(_element, collection) => {
                // The element is a binder whose kind comes from its uses
                // in the body; here only the collection is observed.
                if let Term::Var(name) = collection {
                    self.observe(name, PredicateArgKind::Collection);
                }
            }
        }
    }

    /// Walk a value expression. `expected` is the kind the surrounding
    /// position requires, if any. A bare variable is observed at it;
    /// anything else recurses.
    fn walk_value(&mut self, expr: &ValueExpr, expected: Option<PredicateArgKind>) {
        match expr {
            ValueExpr::Term(Term::Var(name)) => {
                if let Some(kind) = expected {
                    self.observe(name, kind);
                }
            }
            ValueExpr::Term(_) => {}
            ValueExpr::Arith { op, left, right } => {
                // If one side's kind is known and only one arithmetic rule
                // fits, the other side takes the matching kind: `turn_time`
                // in `tendered_at + turn_time` becomes a Duration. If several
                // rules fit (`Timestamp - x`), nothing is assumed. With
                // neither side known, `*`, `/`, `%` default to bare decimal,
                // as the checker does; `+` and `-` stay open.
                let l_known = self.shallow_value_kind(left);
                let r_known = self.shallow_value_kind(right);
                let (l_exp, r_exp) = match (l_known, r_known) {
                    (Some(k), None) => (
                        None,
                        crate::ir::arith_unique_counterpart(*op, &k, true)
                            .map(|(expected, _)| expected),
                    ),
                    (None, Some(k)) => (
                        crate::ir::arith_unique_counterpart(*op, &k, false)
                            .map(|(expected, _)| expected),
                        None,
                    ),
                    (None, None) if matches!(op, ArithOp::Mul | ArithOp::Div | ArithOp::Mod) => (
                        Some(PredicateArgKind::Decimal),
                        Some(PredicateArgKind::Decimal),
                    ),
                    _ => (None, None),
                };
                self.walk_value(left, l_exp);
                self.walk_value(right, r_exp);
            }
            ValueExpr::Sum {
                value,
                body,
                seed: _,
            } => {
                self.walk_prop(body);
                // The target's kind comes from the body, not from where
                // the sum sits; walk it only for what it contains.
                self.walk_value(value, None);
            }
            // Like a sum, the kind is observed inside the body.
            ValueExpr::Extremum { value, body, .. } => {
                let _ = value;
                self.walk_prop(body);
            }
            ValueExpr::ValueOf {
                predicate,
                args,
                extract: _,
                default,
            } => {
                self.observe_claim_args(predicate.as_str(), args);
                if let Some(d) = default {
                    self.walk_value(d, expected);
                }
            }
            // Each builtin decides what its argument slots expect.
            ValueExpr::Call { builtin, args } => self.walk_builtin(*builtin, args, expected),
            // Both branches carry the expected kind. A parameter seen at
            // two kinds across them is `Ambiguous`, as with `or`.
            ValueExpr::Cond {
                when,
                then,
                otherwise,
            } => {
                self.walk_prop(when);
                self.walk_value(then, expected.clone());
                self.walk_value(otherwise, expected);
            }
        }
    }

    /// What each builtin expects of its arguments. `abs`, `min`, and `max`
    /// pass the surrounding expectation down; the rest fix their slots.
    fn walk_builtin(
        &mut self,
        builtin: Builtin,
        args: &[ValueExpr],
        expected: Option<PredicateArgKind>,
    ) {
        match builtin {
            // Kind-preserving.
            Builtin::Abs => {
                for a in args {
                    self.walk_value(a, expected.clone());
                }
            }
            // Both operands share one kind, so a known side fixes the
            // other: `min(x, 100)` makes `x` a decimal.
            Builtin::Min | Builtin::Max => {
                let left_known = self.shallow_value_kind(&args[0]);
                let right_known = self.shallow_value_kind(&args[1]);
                let (left_expected, right_expected) = match (left_known, right_known) {
                    (Some(k), None) => (expected.clone(), Some(k)),
                    (None, Some(k)) => (Some(k), expected.clone()),
                    _ => (expected.clone(), expected),
                };
                self.walk_value(&args[0], left_expected);
                self.walk_value(&args[1], right_expected);
            }
            // Decimal-only in v0: both positions force Decimal.
            Builtin::Round => {
                for a in args {
                    self.walk_value(a, Some(PredicateArgKind::Decimal));
                }
            }
            // Each slot pins its own kind; the result is Decimal
            // regardless of `expected`.
            Builtin::PeriodIndex => {
                for (a, kind) in args.iter().zip([
                    PredicateArgKind::Date,
                    PredicateArgKind::CalendarSpan,
                    PredicateArgKind::Date,
                ]) {
                    self.walk_value(a, Some(kind));
                }
            }
            // Each slot pins its own kind; the result is Date
            // regardless of `expected`.
            Builtin::PeriodStartOf => {
                for (a, kind) in args.iter().zip([
                    PredicateArgKind::Date,
                    PredicateArgKind::CalendarSpan,
                    PredicateArgKind::Decimal,
                ]) {
                    self.walk_value(a, Some(kind));
                }
            }
        }
    }

    /// Observe a claim's variable arguments at the predicate's declared
    /// kinds. An undeclared predicate contributes nothing (the checker
    /// flags it).
    fn observe_claim_args(&mut self, predicate: &str, args: &[Term]) {
        let Some(decl_args) = self.predicates.get(predicate) else {
            return;
        };
        for (arg, decl_arg) in args.iter().zip(decl_args.iter()) {
            if let Term::Var(name) = arg {
                self.observe(name, decl_arg.kind.clone());
            }
        }
    }

    /// As [`Self::observe_claim_args`] but against the intent
    /// vocabulary.
    fn observe_intent_args(&mut self, intent: &str, args: &[Term]) {
        let Some(decl_args) = self.intents.get(intent) else {
            return;
        };
        for (arg, decl_arg) in args.iter().zip(decl_args.iter()) {
            if let Term::Var(name) = arg {
                self.observe(name, decl_arg.kind.clone());
            }
        }
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
