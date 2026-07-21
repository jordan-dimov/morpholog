//! Static analyses over the IR. Used by the PostgreSQL adapter's read
//! path to load only the claims a derived-claim enumeration or
//! transformation body needs, and by callers that want to inspect a
//! programme's predicate vocabulary without running it.
//!
//! Every walker uses an **exhaustive** match (no `_` arm) so that a
//! future `Prop`, `ValueExpr`, or `Stmt` variant cannot silently fall
//! through and cause the read path to load an incomplete claim set.

use std::collections::{BTreeSet, HashMap};

use crate::definitions::DefinitionIndex;
use crate::ir::{
    ArgDecl, ArithOp, Definition, DefinitionName, DerivedClaim, OrderedDomain, PredicateArgKind,
    PredicateName, Program, Prop, Stmt, Term, TransformationName, ValueExpr, Var,
};
use crate::validate::ValidatedProgram;

/// Return the set of predicate names a proposition references anywhere
/// in its tree. Used by the PostgreSQL adapter's read path to load only
/// the claims a derived-claim enumeration needs, instead of fetching the
/// whole `morpholog.claims` table.
///
/// The match below is **exhaustive over `Prop` variants on purpose**
/// (no `_` arm). If a future PR adds a new `Prop` variant, the
/// compiler will refuse this function until the new variant is
/// handled. That compile-time check is what keeps the analysis
/// honest: a missed variant here would silently produce
/// wrong-answer bugs at runtime - the read path would skip claims
/// the kernel actually needs, and `enumerate_derived` would return
/// an answer computed against an incomplete state.
///
/// `In` takes only `Term`s (variables, wildcards, or literals), none of
/// which can reference a predicate; it contributes nothing. Comparator
/// operands are value expressions, walked by
/// [`predicates_referenced_by_value`].
pub fn predicates_referenced_by_prop(
    prop: &Prop,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    prop_refs(
        prop,
        DefinitionIndex::new(definitions),
        &mut BTreeSet::new(),
        out,
    );
}

/// Recursive worker for [`predicates_referenced_by_prop`]. `seen`
/// guards definition recursion: each definition body is walked once
/// per top-level call, which both avoids rework and keeps the walk
/// terminating on (invalid, cyclic) unvalidated IR.
fn prop_refs(
    prop: &Prop,
    definitions: DefinitionIndex<'_>,
    seen: &mut BTreeSet<DefinitionName>,
    out: &mut BTreeSet<PredicateName>,
) {
    match prop {
        // A call reads whatever its definition's body reads -
        // transitively, so the PG read path loads every predicate a
        // gate consults through any chain of named conditions. Missing
        // this recursion would be a silent wrong-answer bug: the kernel
        // would evaluate the body against claims that were never loaded.
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

/// Return the set of predicate names a value expression references
/// anywhere in its tree. The value-sort companion to
/// [`predicates_referenced_by_prop`]; the two recurse into each other
/// because the sorts are mutually recursive (`Sum`'s body is a `Prop`).
///
/// Exhaustive over `ValueExpr` for the same honesty reason as the
/// proposition walker. `Term` takes only a `Term` and contributes
/// nothing.
pub fn predicates_referenced_by_value(
    expr: &ValueExpr,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    value_refs(
        expr,
        DefinitionIndex::new(definitions),
        &mut BTreeSet::new(),
        out,
    );
}

/// Recursive worker for [`predicates_referenced_by_value`].
fn value_refs(
    expr: &ValueExpr,
    definitions: DefinitionIndex<'_>,
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
        ValueExpr::Sum { body, .. } => {
            prop_refs(body, definitions, seen, out);
        }
        ValueExpr::Abs(operand) => value_refs(operand, definitions, seen, out),
        ValueExpr::Term(_) => {
            // No predicate references; operates on a Term only.
        }
    }
}

/// Return the set of predicate names that `enumerate_derived(derived,
/// state)` will need to read out of `state`. Computed as the union of
/// the `domain` expression's referenced predicates and every
/// `DerivedValue.expr`'s referenced predicates.
///
/// The `predicate` field on the derived claim itself is **not**
/// included: that names the OUTPUT predicate of the enumeration,
/// which the kernel never reads from state. Including it would tell
/// callers to load claims they have no use for.
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

/// Return every predicate name a statement references in its tree.
/// Symmetric with [`predicates_referenced_by_prop`] but operates at
/// the statement level. This walker is the **broad** set; it includes
/// predicates the statement *reads from pre-state* (Require, BindOne,
/// Let-value, For-collection, Retract pattern) and predicates the
/// statement *writes* (Assert).
///
/// For scoped pre-state loading on the PG adapter's write path, use
/// [`predicates_read_by_stmt`] instead: it excludes Assert's output
/// predicate (which doesn't need pre-loading) but keeps Retract
/// (which pattern-matches against pre-state to find what to retract).
///
/// This broad walker stays available for callers that want every
/// predicate the statement *mentions* in either direction - dependency
/// tracing, docs generation, future tooling.
///
/// The match below is exhaustive over `Stmt` variants on purpose; a
/// future `Stmt` variant would break compilation here until handled.
///
/// `Stmt::Emit` contributes nothing - intents are not part of the
/// admitted-claim vocabulary.
///
/// `Stmt::LetNewSubject` contributes nothing - it mints a fresh
/// subject identifier without consulting state.
pub fn predicates_referenced_by_stmt(
    stmt: &Stmt,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => predicates_referenced_by_prop(p, definitions, out),
        Stmt::Let { value, .. } => predicates_referenced_by_value(value, definitions, out),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(c) => {
            out.insert(c.predicate.clone());
        }
        Stmt::Retract { predicate, .. } => {
            out.insert(predicate.clone());
        }
        Stmt::For {
            collection, body, ..
        } => {
            predicates_referenced_by_value(collection, definitions, out);
            for inner in body {
                predicates_referenced_by_stmt(inner, definitions, out);
            }
        }
        Stmt::Emit(_) => {}
    }
}

/// Return every predicate name a statement **reads from pre-state**.
/// Distinguished from [`predicates_referenced_by_stmt`] by excluding
/// `Stmt::Assert`'s output predicate, which is *written* to the
/// staged outcome and never read from pre-state.
///
/// This is the analysis the PG adapter's `propose_against_pg` and
/// `propose_against_pg_with_trace` use to scope `load_state`: only
/// the predicates this transformation will actually consult need to
/// be fetched from `morpholog.claims`, instead of the full table.
///
/// Variant treatment:
/// - `Require` / `BindOne` / `Let.value` / `For.collection` - read
///   (their expressions evaluate against pre-state).
/// - `Retract { predicate, args }` - **read**, despite being a
///   mutation. The retract pattern is matched against pre-state to
///   find which claims to retract; without loading the target
///   predicate, the pattern match has nothing to find.
/// - `Assert(claim)` - **not read**. The claim is staged as an
///   output; the predicate's existing pre-state has no bearing on
///   the assert.
/// - `Emit` / `LetNewSubject` - contribute nothing.
/// - `For` body - recurses (the body's own reads count).
///
/// Exhaustive match for the same reason as
/// `predicates_referenced_by_stmt`: a future `Stmt` variant must
/// declare its read behaviour explicitly.
pub fn predicates_read_by_stmt(
    stmt: &Stmt,
    definitions: &[Definition],
    out: &mut BTreeSet<PredicateName>,
) {
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => predicates_referenced_by_prop(p, definitions, out),
        Stmt::Let { value, .. } => predicates_referenced_by_value(value, definitions, out),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(_) => {
            // Write-only: the asserted claim is staged as output, not
            // looked up against pre-state. Excluded from the read set.
        }
        Stmt::Retract { predicate, .. } => {
            // Retract is a mutation, but its pattern is matched
            // against pre-state to find candidates - so the target
            // predicate must be loaded.
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

/// Return the names of every transformation in `program` whose body
/// asserts `predicate`, in declaration order. This is the one-hop
/// "what could supply this claim?" lookup the explanation engine uses
/// to name candidate suppliers for a directly-missing claim.
///
/// Deliberately predicate-level and structural: a transformation that
/// asserts `predicate` is a *candidate* supplier, not a guarantee it
/// can supply a specific claim instance under given bindings - it may
/// carry its own `require` gates, authority, or date windows. Honest
/// candidate-supplier lookup; not instance matching, not multi-hop
/// reachability (that is bounded model checking, deferred).
///
/// Recurses into `For` bodies: an assert nested in a loop still makes
/// the transformation a supplier of that predicate.
pub fn transformations_asserting(program: &Program, predicate: &str) -> Vec<String> {
    program
        .transformations
        .iter()
        .filter(|t| t.body.iter().any(|s| stmt_asserts(s, predicate)))
        .map(|t| t.name.to_string())
        .collect()
}

/// Whether a statement (or, for `For`, its body) asserts `predicate`.
///
/// Exhaustive over `Stmt` for the same reason as the predicate walkers:
/// a future variant that can assert a claim must declare itself here
/// rather than fall silently through a `_` arm and make a supplier
/// invisible to `explain`.
fn stmt_asserts(stmt: &Stmt, predicate: &str) -> bool {
    match stmt {
        Stmt::Assert(claim) => claim.predicate.as_str() == predicate,
        Stmt::For { body, .. } => body.iter().any(|s| stmt_asserts(s, predicate)),
        Stmt::Require(_)
        | Stmt::BindOne(_)
        | Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Retract { .. }
        | Stmt::Emit(_) => false,
    }
}

/// The predicates the current programme can admit into state: those
/// some transformation asserts. The scope is what the *source file* can
/// put into the candidate state an invariant checks against, not what
/// state may already hold - persisted, imported, or historically
/// admitted claims can populate a predicate outside this set, so its
/// absence here is an authoring signal, not a proof of emptiness.
///
/// Derived claims are excluded on purpose: they are read-side
/// projections, never enumerated into candidate state, so a predicate
/// produced only as a derived claim has no admitted supplier.
pub(crate) fn declared_supplier_predicates(program: &Program) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    for t in &program.transformations {
        for s in &t.body {
            collect_asserted(s, &mut out);
        }
    }
    out
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
        Stmt::Require(_)
        | Stmt::BindOne(_)
        | Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Retract { .. }
        | Stmt::Emit(_) => {}
    }
}

/// The predicates with no declared supplier that prevent `prop` from
/// binding on a fresh ledger, or `None` if it could bind there. A
/// blocker is a predicate the antecedent genuinely requires - a
/// mandatory conjunct, or every branch of a disjunction - that the
/// current programme never admits. The result names only predicates
/// that actually force the answer, so a diagnostic built from it points
/// at a true cause, not every undeclared predicate in the tree.
///
/// "On a fresh ledger" is the honest scope: a predicate with no supplier
/// is empty against an empty state the programme alone fills, but says
/// nothing about state already persisted. Negation, implication,
/// `forall`, and value comparisons stay satisfiable here, so this looks
/// at predicate positions only.
pub(crate) fn undeclared_blockers(
    prop: &Prop,
    declared: &BTreeSet<PredicateName>,
    definitions: DefinitionIndex<'_>,
) -> Option<BTreeSet<PredicateName>> {
    undeclared_blockers_inner(prop, declared, definitions, &mut BTreeSet::new())
}

fn undeclared_blockers_inner(
    prop: &Prop,
    declared: &BTreeSet<PredicateName>,
    definitions: DefinitionIndex<'_>,
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
        Prop::Defined { name, .. } => {
            // Recursion-stack guard; a call blocks iff its body does.
            if seen.insert(name.clone()) {
                let blockers = match definitions.get(name) {
                    Some(def) => undeclared_blockers_inner(&def.body, declared, definitions, seen),
                    None => None,
                };
                seen.remove(name);
                blockers
            } else {
                None
            }
        }
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
// Per-transformation argument-kind analysis: the embedder-facing
// input contract.
// ============================================================

/// The resolved kind for one transformation parameter, projected from
/// the union of every position the parameter is observed in across
/// the transformation body. The variants each map to genuinely
/// different embedder behaviour:
///
/// - `Concrete(Decimal)` is a single decimal input field.
/// - `Polymorphic` is "the embedder must accept input but cannot
///   narrow the kind" (the parameter flowed only through `Any` slots,
///   the declaration-time escape hatch).
/// - `Unconstrained` is "the parameter is never used" - likely dead
///   or a modelling smell.
/// - `Ambiguous` is "the parameter is observed at different concrete
///   kinds across separately-satisfiable code paths" - the static
///   checker walks `Or` branches (and `Require` / `Sum` / `For`
///   bodies) in cloned scopes whose refinements do not export, so a
///   programme can validate even when the same parameter has
///   different concrete kinds in different branches. The Or-of-
///   different-kinds shape is legitimate (the runtime picks the
///   branch that matches the actual input), so refusing to emit a
///   schema would be too strict; reporting one concrete kind would
///   be a lie. The vec lists the distinct kinds observed in
///   deterministic order (the `PredicateArgKind` declaration order).
///
/// Collapsing `Polymorphic` / `Unconstrained` to one state loses a
/// useful distinction (the embedder presents them differently);
/// collapsing `Ambiguous` to `Polymorphic` or to a silent
/// first-observation-wins reports a contract that does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamKind {
    /// A specific declared kind (Subject, Decimal, Date, Bool, Collection).
    /// The embedder can derive a typed input field directly.
    Concrete(PredicateArgKind),
    /// The parameter flows only through positions declared as
    /// `PredicateArgKind::Any` - the declaration-time kind escape
    /// hatch. The kernel cannot narrow the kind; the embedder
    /// should accept input but flag the lack of constraint.
    Polymorphic,
    /// The parameter is never observed at any kind-bearing
    /// position in the transformation body. Either dead code or a
    /// modelling smell; the embedder should surface it.
    Unconstrained,
    /// The parameter is observed at two or more distinct concrete
    /// kinds across cloned scopes the checker does not refine across
    /// (typically `Or` branches). The vec carries every observed
    /// kind, in declaration order, deduplicated. The embedder can
    /// render this as a disjunctive contract (JSON Schema `anyOf`,
    /// for instance) or surface it as a modelling diagnostic.
    Ambiguous(Vec<PredicateArgKind>),
    /// The parameter is a collection iterated by `for` / `forall` whose
    /// element kind WAS observed: the projection of how the loop binding
    /// is used in the body - `Collection(Concrete(Subject))` for a list
    /// of subjects. The element is itself a [`ParamKind`], so nesting is
    /// expressible. This variant appears only once an element kind is
    /// actually observed: a collection whose binding is never used at a
    /// kind-bearing position carries no element evidence and stays the
    /// opaque `Concrete(Collection)` instead. This is the shape an
    /// external engine submits a whole batch through.
    Collection(Box<ParamKind>),
}

/// Errors that prevent per-transformation argument-kind analysis.
/// Programme-level validation errors do not appear here: the API
/// takes a [`ValidatedProgram`], so the type system rules out the
/// invalid-programme case before this function runs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnalysisError {
    /// No transformation declared with that name.
    #[error("unknown transformation `{name}`")]
    UnknownTransformation { name: TransformationName },
}

/// Compute the embedder-facing input contract for one transformation:
/// the resolved [`ParamKind`] for every parameter, in declaration
/// order.
///
/// The walker is a *sibling* of the static checker's
/// `check_program`, not the same walker. The checker walks
/// `Require` (and `Or` branches, `Sum` / `For` bodies) in a cloned
/// scope, which is the correct semantics for the runtime
/// binding-flow doctrine - match bindings inside a gate do not
/// export to later statements. But that semantics is wrong for an
/// **external input contract**: a parameter used only inside
/// `require` is still externally supplied, and its slot still
/// observes a concrete kind. So this walker accumulates kind
/// observations across the union of every visited position
/// (`Require` included), in a flat environment that is never cloned
/// at scope boundaries.
///
/// All variable observations are tracked, not only parameter
/// observations - this lets a parameter pick up its kind from
/// intermediate variables that are themselves observed in
/// kind-bearing positions later in the body. (`Eq` / `Neq`
/// cross-refinement, where a literal on one side would pin a bare
/// variable on the other, is deliberately omitted - the simpler
/// walker is honest about what it does; a worked example that
/// genuinely needs the inference becomes the witness for adding
/// it.)
///
/// Returns observations in `transformation.parameters` declaration
/// order, never in hash-iteration order: form generation, request
/// models, and CLI payload examples downstream depend on stable
/// human-facing order.
///
/// Takes a [`ValidatedProgram`] rather than a `&Program` so the
/// precondition (programme is validated) is enforced at the type
/// level. The accessor no longer needs to defensively re-validate
/// internally - callers that have already validated (every CLI
/// path, every worked-example test) only pay the validation cost
/// once.
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

    // Observations were already propagated eagerly through the
    // current equivalence class at each observation site, so each
    // parameter's accumulated set is a direct lookup. No post-hoc
    // class-building needed.
    Ok(transformation
        .parameters
        .iter()
        .map(|param| {
            let observed = collector
                .observations
                .get(param)
                .cloned()
                .unwrap_or_default();
            // A parameter ITERATED as a collection with an observed element
            // (a `collection_elements` entry, always non-empty) carries that
            // element kind: the projection of how its loop binding was used.
            // A parameter observed as a collection only through a
            // Collection-declared predicate arg, or iterated with a binding
            // never used at a kind-bearing position, has no element evidence
            // and stays the opaque `Concrete(Collection)`. A parameter used
            // both as a collection and a scalar stays a genuine conflict (the
            // ordinary `Ambiguous` projection).
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

/// Project a parameter's accumulated observation set into the public
/// [`ParamKind`]. `Any` is the declaration-time escape hatch; a
/// parameter observed only through `Any` slots is `Polymorphic`, not
/// `Concrete(Any)`. Conflicting concrete observations become
/// `Ambiguous` rather than silently collapsing to either side.
/// `BTreeSet` iteration yields the deterministic `PredicateArgKind`
/// declaration order, which the public `Ambiguous` payload guarantees.
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

/// Walker state for [`transformation_param_kinds`]. Tracks observations
/// for every variable encountered, not only parameters - the projection
/// to parameters happens at the end. Observations are accumulated in a
/// single flat environment; no scope cloning at `Require` / `Or` / `Sum`
/// / `For` boundaries, which is the entire point of running this
/// alongside the checker rather than reusing it.
///
/// Each variable carries a *set* of observed kinds, not a single
/// refined kind. A conflict between two concrete observations across
/// different cloned scopes the checker hides (an `Or` branch picking
/// Decimal vs another picking Subject, say) is preserved as a
/// set with both kinds, then projected to [`ParamKind::Ambiguous`].
/// Silently dropping the second observation would produce a JSON
/// Schema that rejects valid inputs of the other branch's kind.
///
/// The walker is deliberately minimal: it visits every position where
/// a variable can appear in a kind-bearing slot, observes it there,
/// and recurses. It does NOT cross-refine `Eq` / `Neq` operands
/// (pinning a bare variable to a literal's kind on the other side) -
/// real models flow parameters through claim / intent arg positions;
/// cross-refinement is reserved for the first example that genuinely
/// needs it. `Eq(param, literal)` as a parameter's *sole* kind
/// observation surfaces as `Unconstrained`, which is the right
/// signal: the embedder either receives a clean rewrite via a claim
/// arg, or learns the model is leaning on a hidden assumption.
struct ParamCollector<'a> {
    predicates: HashMap<&'a str, &'a [ArgDecl]>,
    intents: HashMap<&'a str, &'a [ArgDecl]>,
    /// Inferred kind-observation sets per definition parameter,
    /// computed callees-first at construction (a definition's params
    /// have no declared kinds; the body is the only kind source). A
    /// call argument observes every kind in its parameter's set, so a
    /// disjunctive body surfaces as `Ambiguous` rather than silently
    /// committing to one kind.
    definition_params: HashMap<String, Vec<BTreeSet<PredicateArgKind>>>,
    observations: HashMap<Var, BTreeSet<PredicateArgKind>>,
    /// Element-kind observations per collection variable: when `for x in
    /// coll` (or `forall`) iterates a variable `coll`, the loop binding's
    /// observed kinds ARE `coll`'s element kinds, captured at the `For`
    /// node before the binding's loop-local observations are discarded.
    /// Projected into [`ParamKind::Collection`] for collection parameters.
    collection_elements: HashMap<Var, BTreeSet<PredicateArgKind>>,
    /// Flow-sensitive equivalence-class membership per currently-live
    /// variable. Maintained as the walker advances: a `Let` or
    /// `LetNewSubject` rebinding `name` removes `name` from its
    /// existing class first (the old logical variable is gone), then
    /// optionally adds the new alias. Observations propagate eagerly
    /// through the current class at the moment of observation; an
    /// observation made *after* a rebind never reaches names the
    /// rebound variable used to alias. Deliberately narrow: only
    /// `Let { value: Term(Var(alias)) }` registers an alias - we do
    /// NOT try to infer aliases through `Eq` / `Neq` (that is a
    /// different semantic commitment, deferred).
    ///
    /// A variable absent from this map has the implicit singleton
    /// class `{var}`; storing all singletons would just waste
    /// memory. Stored classes always have at least two members.
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
        // Pre-walk each definition body, callees before callers, and
        // project its parameters' observation sets. On a cyclic graph
        // (unvalidated IR; `validate` rejects it) the map stays empty
        // and calls simply contribute no observations.
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

    /// Observe `name` at `kind`. Inserts the kind into the
    /// observation set of every currently-aliased member of `name`'s
    /// equivalence class - so a parameter's observation reaches its
    /// aliased local binding (and vice versa) at the moment of
    /// observation, not via a post-hoc projection. Multi-kind sets
    /// accumulate per variable and project to [`ParamKind::Ambiguous`]
    /// at the end rather than silently committing to one kind.
    /// The kind of a value expression when it is determinable without
    /// assumption: a literal's inherent kind, or a variable that every
    /// position so far has pinned to exactly one concrete kind. `None`
    /// for anything deeper - this probe is deliberately shallow, a
    /// sibling of the checker's full inference, used only to decide
    /// whether the arithmetic matrix forces the *other* operand.
    fn shallow_value_kind(&self, v: &ValueExpr) -> Option<PredicateArgKind> {
        match v {
            ValueExpr::Term(Term::Literal(lit)) => Some(match lit {
                crate::ir::Value::Subject(_) => PredicateArgKind::Subject,
                crate::ir::Value::Decimal(_) => PredicateArgKind::Decimal,
                crate::ir::Value::Date(_) => PredicateArgKind::Date,
                crate::ir::Value::Timestamp(_) => PredicateArgKind::Timestamp,
                crate::ir::Value::Duration(_) => PredicateArgKind::Duration,
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

    fn observe(&mut self, name: &Var, kind: PredicateArgKind) {
        // Collect the class members up front so we don't hold a borrow
        // of `current_class` across the mutable borrows of
        // `observations`. The implicit singleton case avoids storing
        // a class for every variable.
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

    /// Remove `name` from any current equivalence class it
    /// participates in. The other members stay aliased to each
    /// other; `name` becomes a fresh singleton. Called when a
    /// `Let` or `LetNewSubject` rebinds `name` - the old logical
    /// variable's aliases must not silently capture observations
    /// of the new binding.
    fn invalidate(&mut self, name: &Var) {
        let Some(mut class) = self.current_class.remove(name) else {
            return;
        };
        class.remove(name);
        match class.len() {
            0 | 1 => {
                // Singleton class is the implicit default - drop the
                // entry for any remaining lone member.
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

    /// Merge `name` and `alias` (and their current classes) into a
    /// single equivalence class. Called when `Let { name, value:
    /// Term(Var(alias)) }` is encountered, *after* `invalidate(name)`
    /// has cleared any prior alias relations for `name`.
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

    /// Walk a statement. Exhaustive over `Stmt` for the same honesty
    /// reason as the predicate-set walkers: a future variant that
    /// can carry a variable observation must declare itself here.
    fn walk_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Require(prop) | Stmt::BindOne(prop) => self.walk_prop(prop),
            Stmt::Let { name, value } => {
                // Flow-sensitive: clear any prior alias relations
                // for `name` first, since the rebinding creates a
                // fresh logical variable. Then optionally register
                // the new alias.
                self.invalidate(name);
                if let ValueExpr::Term(Term::Var(alias)) = value {
                    self.add_alias(name, alias);
                }
                self.walk_value(value, None);
            }
            Stmt::LetNewSubject { name } => {
                // A fresh subject identifier. Same flow-sensitive
                // rebinding rule as `Let`: drop any prior alias for
                // `name`, then observe at Subject (the checker pins
                // `name`'s kind here too).
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
                // `For` is the one Stmt the static checker walks
                // under a *cloned* scope (`check.rs` notes "the
                // loop binding and any body-introduced names do
                // not leak across iterations or beyond the loop").
                // Mirror that: the loop binding shadows any outer
                // name of the same name for the body's duration,
                // so observations inside the body attributed to
                // the binding name must NOT propagate to outer
                // aliases of that name (a parameter with the same
                // name being the live failure mode). Save the
                // binding's outer observations and class,
                // invalidate, walk the body, then restore - so
                // observations of OTHER outer variables made
                // inside the body still survive (they are
                // legitimately about the outer scope), but the
                // binding's body-time state is discarded.
                let saved_obs = self.observations.get(binding).cloned();
                let saved_class = self.current_class.get(binding).cloned();
                self.invalidate(binding);
                self.observations.remove(binding);

                for inner in body {
                    self.walk_stmt(inner);
                }

                // The loop binding's body-time observations ARE the
                // collection's element kinds. Capture them against the
                // collection variable (when it is a plain variable - a
                // parameter or a let-bound list) before the binding's
                // loop-local state is discarded below.
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
                // The source observes the collection (a `forall x in xs`
                // lowers to a source `In(x, xs)`, whose `In` arm observes
                // `xs` as a collection).
                self.walk_prop(source);

                // Same shadowing discipline as `Stmt::For`: the quantifier
                // binding is loop-local, so its body-time observations must
                // not leak to an outer name of the same name. Save, clear,
                // walk, capture the element kind, then restore.
                let saved_obs = self.observations.get(binding).cloned();
                let saved_class = self.current_class.get(binding).cloned();
                self.invalidate(binding);
                self.observations.remove(binding);

                self.walk_prop(body);

                // The binding's body-time observations ARE the source
                // collection's element kinds. Capture them against the
                // collection variable from the `In` source before the
                // binding's loop-local state is discarded.
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
                    // The decimal domain has two flavours (bare decimal,
                    // unit-tagged quantity). If either side's shallow
                    // kind already names a unit, both sides observe at
                    // that quantity kind - so `settled <= due` pins the
                    // settlement parameter to the due figure's unit.
                    // Otherwise the domain's neutral bare-decimal
                    // reading stands, as before quantities existed.
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
                // No cross-refinement (see the struct-level comment):
                // observe only the kinds that sub-positions force, not
                // the kind one operand would push onto the other.
                self.walk_value(left, None);
                self.walk_value(right, None);
            }
            Prop::In(_element, collection) => {
                // The element is introduced as a binder (the
                // checker binds it without pinning a kind, since v0
                // does not track collection item kinds); only the
                // collection contributes a kind observation.
                if let Term::Var(name) = collection {
                    self.observe(name, PredicateArgKind::Collection);
                }
            }
        }
    }

    /// Walk a value expression. `expected` carries the kind the
    /// surrounding position requires the expression to be (Decimal
    /// from `Arith`, Collection from `For`, the domain kind from
    /// `Compare`, etc.). A bare-variable operand pins to `expected`;
    /// anything else recurses, and the sub-positions force kinds
    /// from their own walkers.
    fn walk_value(&mut self, expr: &ValueExpr, expected: Option<PredicateArgKind>) {
        match expr {
            ValueExpr::Term(Term::Var(name)) => {
                if let Some(kind) = expected {
                    self.observe(name, kind);
                }
            }
            ValueExpr::Term(_) => {}
            ValueExpr::Arith { op, left, right } => {
                // Every operator runs the matrix's one-side-known
                // refinement: if one side's kind is already
                // determinable (a literal, or a variable every prior
                // position pinned to one kind) and exactly one rule
                // fits, the other side observes the forced
                // counterpart - the externally supplied turn time in
                // `tendered_at + turn_time` resolves to Duration this
                // way, and the scaling factor in `daily_amount * x`
                // resolves to Decimal. When several rules fit
                // (`Timestamp - x`, `usd_amount / x`), nothing is
                // assumed and the checker's matrix remains the only
                // judge. With NEITHER side determinable, Mul / Div /
                // Mod keep their historical bare-decimal default
                // (mirroring the checker; a unit cannot be inferred
                // from nothing); the additive operators stay
                // unrefined, as the time kinds left them.
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
                // The summed term is decimal, duration, or quantity;
                // its kind is observed from its claim position inside
                // the body, so the aggregate itself pins nothing.
                let _ = value;
                self.walk_prop(body);
            }
            ValueExpr::ValueOf {
                predicate,
                args,
                default,
            } => {
                self.observe_claim_args(predicate.as_str(), args);
                if let Some(d) = default {
                    self.walk_value(d, expected);
                }
            }
            // abs preserves its operand's kind, so the operand carries
            // the same expected kind as the abs itself.
            ValueExpr::Abs(operand) => self.walk_value(operand, expected),
        }
    }

    /// Observe variable arguments in a claim reference against the
    /// declared predicate arg kinds. An undeclared predicate
    /// contributes nothing (the checker already flagged it).
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

    // A predicate produced only as a derived claim is read-side, never
    // admitted into the candidate state invariants check, so it is not
    // an admitted supplier. (An invariant referencing it is rejected as
    // undeclared before lints run, so the lint never observes this - but
    // the supplier set must still not pretend it can be admitted.)
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
        undeclared_blockers(prop, &pset(supplied), DefinitionIndex::new(&[]))
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
