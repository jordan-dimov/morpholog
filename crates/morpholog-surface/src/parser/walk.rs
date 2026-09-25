//! Name and size queries over the kernel IR, on the shared walk in
//! `morpholog_core::fold`. Passes that rewrite or judge a tree
//! (substitution, const checks) keep their own match.

use std::collections::BTreeSet;

use morpholog_core::{Prop, Stmt, Term, ValueExpr, Var};

pub(super) use morpholog_core::fold::{Node, walk_prop, walk_stmt, walk_value};

/// The variable a node reads, if any, and whether it is in value position.
pub(super) fn read_var<'a>(node: &Node<'a>) -> Option<(&'a Var, bool)> {
    match node {
        Node::Slot(Term::Var(v)) => Some((v, false)),
        Node::Value(ValueExpr::Term(Term::Var(v))) => Some((v, true)),
        _ => None,
    }
}

/// Every variable the tree reads, in either position.
pub(super) fn vars_in_prop(prop: &Prop, out: &mut BTreeSet<String>) {
    walk_prop(prop, &mut |n| var_into(&n, out));
}

pub(super) fn vars_in_value(expr: &ValueExpr, out: &mut BTreeSet<String>) {
    walk_value(expr, &mut |n| var_into(&n, out));
}

pub(super) fn vars_in_stmt(stmt: &Stmt, out: &mut BTreeSet<String>) {
    walk_stmt(stmt, &mut |n| var_into(&n, out));
}

pub(super) fn vars_in_term(term: &Term, out: &mut BTreeSet<String>) {
    var_into(&Node::Slot(term), out);
}

fn var_into(node: &Node<'_>, out: &mut BTreeSet<String>) {
    if let Some((v, _)) = read_var(node) {
        out.insert(v.to_string());
    }
}

/// Every name a quantifier in the tree introduces. A sum target is
/// consumed against its body's bindings and introduces nothing.
pub(super) fn binders_in_prop(prop: &Prop, out: &mut BTreeSet<String>) {
    walk_prop(prop, &mut |n| binder_into(&n, out));
}

pub(super) fn binders_in_value(expr: &ValueExpr, out: &mut BTreeSet<String>) {
    walk_value(expr, &mut |n| binder_into(&n, out));
}

fn binder_into(node: &Node<'_>, out: &mut BTreeSet<String>) {
    if let Node::Binder(v) = node {
        out.insert(v.to_string());
    }
}

/// Tree size: every proposition, value, and slot counts one.
pub(super) fn prop_nodes(prop: &Prop) -> usize {
    let mut n = 0;
    walk_prop(prop, &mut |node| {
        n += usize::from(!matches!(node, Node::Binder(_)))
    });
    n
}

pub(super) fn value_nodes(expr: &ValueExpr) -> usize {
    let mut n = 0;
    walk_value(expr, &mut |node| {
        n += usize::from(!matches!(node, Node::Binder(_)))
    });
    n
}
