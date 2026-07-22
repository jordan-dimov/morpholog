//! Shared structural descent over `Prop` / `ValueExpr` trees.
//!
//! The boolean "does any subterm satisfy this?" walkers used to
//! hand-copy the same recursion, and the copies drifted. These folds
//! own the descent once. The matches are exhaustive with no wildcard
//! arm, so a new IR variant forces exactly one edit here instead of
//! one per walker.

use crate::ir::{Prop, Term, ValueExpr, Var};

/// True when any `Prop` node in the tree satisfies `f`, descending
/// through comparison operands, `sum` bodies, `ValueOf` defaults, and
/// arithmetic. `Defined` is a leaf: a call's body is scanned at its
/// own declaration.
pub(crate) fn any_prop_node(prop: &Prop, f: &impl Fn(&Prop) -> bool) -> bool {
    if f(prop) {
        return true;
    }
    match prop {
        Prop::Claim { .. } | Prop::Defined { .. } | Prop::In(_, _) => false,
        Prop::And(items) | Prop::Or(items) => items.iter().any(|p| any_prop_node(p, f)),
        Prop::Not(p) | Prop::Pre(p) | Prop::Exists { body: p, .. } => any_prop_node(p, f),
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            any_prop_node(left, f) || any_prop_node(right, f)
        }
        Prop::Eq(left, right) | Prop::Neq(left, right) | Prop::Compare { left, right, .. } => {
            any_prop_node_in_value(left, f) || any_prop_node_in_value(right, f)
        }
        Prop::Forall { source, body, .. } => any_prop_node(source, f) || any_prop_node(body, f),
    }
}

/// Value-sort companion to [`any_prop_node`]: the `Prop` nodes
/// reachable from a value expression (a `sum` body, transitively).
pub(crate) fn any_prop_node_in_value(expr: &ValueExpr, f: &impl Fn(&Prop) -> bool) -> bool {
    match expr {
        ValueExpr::Term(_) => false,
        ValueExpr::ValueOf { default, .. } => default
            .as_ref()
            .is_some_and(|d| any_prop_node_in_value(d, f)),
        ValueExpr::Sum { body, .. } => any_prop_node(body, f),
        ValueExpr::Arith { left, right, .. } => {
            any_prop_node_in_value(left, f) || any_prop_node_in_value(right, f)
        }
        ValueExpr::Abs(inner) => any_prop_node_in_value(inner, f),
    }
}

/// Does the proposition contain `pre(...)` anywhere, including inside
/// a comparison operand or `sum` body?
pub(crate) fn mentions_pre(prop: &Prop) -> bool {
    any_prop_node(prop, &|p| matches!(p, Prop::Pre(_)))
}

/// True when any term position in the tree satisfies `f`. The
/// predicate also sees the quantifier binders in scope at that
/// position, so a caller matching variables by name can honour
/// shadowing; callers that don't care ignore the slice.
pub(crate) fn any_term_in_prop(prop: &Prop, f: &impl Fn(&Term, &[&Var]) -> bool) -> bool {
    any_term_prop_scoped(prop, f, &mut Vec::new())
}

/// Value-sort companion to [`any_term_in_prop`].
pub(crate) fn any_term_in_value(expr: &ValueExpr, f: &impl Fn(&Term, &[&Var]) -> bool) -> bool {
    any_term_value_scoped(expr, f, &mut Vec::new())
}

fn any_term_prop_scoped<'p>(
    prop: &'p Prop,
    f: &impl Fn(&Term, &[&Var]) -> bool,
    scope: &mut Vec<&'p Var>,
) -> bool {
    match prop {
        Prop::Claim { args, .. } | Prop::Defined { args, .. } => args.iter().any(|t| f(t, scope)),
        Prop::In(a, b) => f(a, scope) || f(b, scope),
        Prop::And(items) | Prop::Or(items) => {
            items.iter().any(|p| any_term_prop_scoped(p, f, scope))
        }
        Prop::Not(p) | Prop::Pre(p) => any_term_prop_scoped(p, f, scope),
        Prop::Exists { binding, body } => {
            scope.push(binding);
            let hit = any_term_prop_scoped(body, f, scope);
            scope.pop();
            hit
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            any_term_prop_scoped(left, f, scope) || any_term_prop_scoped(right, f, scope)
        }
        Prop::Eq(left, right) | Prop::Neq(left, right) | Prop::Compare { left, right, .. } => {
            any_term_value_scoped(left, f, scope) || any_term_value_scoped(right, f, scope)
        }
        Prop::Forall {
            binding,
            source,
            body,
        } => {
            if any_term_prop_scoped(source, f, scope) {
                return true;
            }
            scope.push(binding);
            let hit = any_term_prop_scoped(body, f, scope);
            scope.pop();
            hit
        }
    }
}

fn any_term_value_scoped<'p>(
    expr: &'p ValueExpr,
    f: &impl Fn(&Term, &[&Var]) -> bool,
    scope: &mut Vec<&'p Var>,
) -> bool {
    match expr {
        ValueExpr::Term(t) => f(t, scope),
        ValueExpr::ValueOf { args, default, .. } => {
            args.iter().any(|t| f(t, scope))
                || default
                    .as_ref()
                    .is_some_and(|d| any_term_value_scoped(d, f, scope))
        }
        ValueExpr::Sum {
            value,
            body,
            seed: _,
        } => f(value, scope) || any_term_prop_scoped(body, f, scope),
        ValueExpr::Arith { left, right, .. } => {
            any_term_value_scoped(left, f, scope) || any_term_value_scoped(right, f, scope)
        }
        ValueExpr::Abs(inner) => any_term_value_scoped(inner, f, scope),
    }
}
