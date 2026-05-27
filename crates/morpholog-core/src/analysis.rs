//! Static analyses over the IR. Used by the PostgreSQL adapter's read
//! path to load only the claims a derived-claim enumeration or
//! transformation body needs, and by callers that want to inspect a
//! programme's predicate vocabulary without running it.
//!
//! Every walker uses an **exhaustive** match (no `_` arm) so that a
//! future `Prop`, `ValueExpr`, or `Stmt` variant cannot silently fall
//! through and cause the read path to load an incomplete claim set.

use std::collections::BTreeSet;

use crate::ir::{DerivedClaim, PredicateName, Program, Prop, Stmt, ValueExpr};

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
pub fn predicates_referenced_by_prop(prop: &Prop, out: &mut BTreeSet<PredicateName>) {
    match prop {
        Prop::Claim { predicate, .. } => {
            out.insert(predicate.clone());
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            predicates_referenced_by_prop(left, out);
            predicates_referenced_by_prop(right, out);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                predicates_referenced_by_prop(p, out);
            }
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => {
            predicates_referenced_by_prop(p, out);
        }
        Prop::Eq(l, r)
        | Prop::Neq(l, r)
        | Prop::Compare {
            left: l, right: r, ..
        } => {
            predicates_referenced_by_value(l, out);
            predicates_referenced_by_value(r, out);
        }
        Prop::Forall { source, body, .. } => {
            predicates_referenced_by_prop(source, out);
            predicates_referenced_by_prop(body, out);
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
pub fn predicates_referenced_by_value(expr: &ValueExpr, out: &mut BTreeSet<PredicateName>) {
    match expr {
        ValueExpr::ValueOf {
            predicate, default, ..
        } => {
            out.insert(predicate.clone());
            if let Some(d) = default {
                predicates_referenced_by_value(d, out);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            predicates_referenced_by_value(left, out);
            predicates_referenced_by_value(right, out);
        }
        ValueExpr::Sum { body, .. } => {
            predicates_referenced_by_prop(body, out);
        }
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
pub fn predicates_referenced_by_derived(derived: &DerivedClaim) -> BTreeSet<PredicateName> {
    let mut out = BTreeSet::new();
    predicates_referenced_by_prop(&derived.domain, &mut out);
    for v in &derived.values {
        predicates_referenced_by_value(&v.expr, &mut out);
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
pub fn predicates_referenced_by_stmt(stmt: &Stmt, out: &mut BTreeSet<PredicateName>) {
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => predicates_referenced_by_prop(p, out),
        Stmt::Let { value, .. } => predicates_referenced_by_value(value, out),
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
            predicates_referenced_by_value(collection, out);
            for inner in body {
                predicates_referenced_by_stmt(inner, out);
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
pub fn predicates_read_by_stmt(stmt: &Stmt, out: &mut BTreeSet<PredicateName>) {
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => predicates_referenced_by_prop(p, out),
        Stmt::Let { value, .. } => predicates_referenced_by_value(value, out),
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
            predicates_referenced_by_value(collection, out);
            for inner in body {
                predicates_read_by_stmt(inner, out);
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
