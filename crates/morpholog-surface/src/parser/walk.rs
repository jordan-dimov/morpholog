//! One read-only walk over the kernel IR, for passes that ask which names a tree binds or reads,
//! or how big it is. Each node says whether it is in value position or a term slot. Passes that
//! rewrite or judge a tree (substitution, const checks) keep their own match.

use std::collections::BTreeSet;

use morpholog_core::{Prop, Stmt, Term, ValueExpr, Var};

pub(super) enum Node<'a> {
    Prop(&'a Prop),
    /// A value expression, including a bare term in value position.
    Value(&'a ValueExpr),
    /// A term in a slot that takes only terms: a claim or call argument, an `in` operand, an
    /// extremum target, a lookup key. Substitution there cannot grow the tree.
    Slot(&'a Term),
    /// A quantifier introducing this name for its body.
    Binder(&'a Var),
}

pub(super) fn walk_stmt<'a>(stmt: &'a Stmt, visit: &mut dyn FnMut(Node<'a>)) {
    match stmt {
        Stmt::Require { prop, .. } | Stmt::BindOne { prop, .. } => walk_prop(prop, visit),
        Stmt::Let { value, .. } => walk_value(value, visit),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(claim) => slots(&claim.args, visit),
        Stmt::Retract { args, .. } => slots(args, visit),
        Stmt::Emit(intent) => slots(&intent.args, visit),
        Stmt::For {
            collection, body, ..
        } => {
            walk_value(collection, visit);
            for s in body {
                walk_stmt(s, visit);
            }
        }
    }
}

pub(super) fn walk_prop<'a>(prop: &'a Prop, visit: &mut dyn FnMut(Node<'a>)) {
    visit(Node::Prop(prop));
    match prop {
        Prop::Claim { args, .. } | Prop::Defined { args, .. } => slots(args, visit),
        Prop::In(l, r) => {
            visit(Node::Slot(l));
            visit(Node::Slot(r));
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                walk_prop(p, visit);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            walk_prop(left, visit);
            walk_prop(right, visit);
        }
        Prop::Not(p) | Prop::Pre(p) => walk_prop(p, visit),
        Prop::Exists { binding, body } => {
            visit(Node::Binder(binding));
            walk_prop(body, visit);
        }
        Prop::Forall {
            binding,
            source,
            body,
        } => {
            visit(Node::Binder(binding));
            walk_prop(source, visit);
            walk_prop(body, visit);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            walk_value(l, visit);
            walk_value(r, visit);
        }
        Prop::Compare { left, right, .. } => {
            walk_value(left, visit);
            walk_value(right, visit);
        }
    }
}

pub(super) fn walk_value<'a>(expr: &'a ValueExpr, visit: &mut dyn FnMut(Node<'a>)) {
    visit(Node::Value(expr));
    match expr {
        ValueExpr::Term(_) => {}
        ValueExpr::Arith { left, right, .. } => {
            walk_value(left, visit);
            walk_value(right, visit);
        }
        ValueExpr::Sum { value, body, .. } => {
            walk_value(value, visit);
            walk_prop(body, visit);
        }
        ValueExpr::Extremum { value, body, .. } => {
            visit(Node::Slot(value));
            walk_prop(body, visit);
        }
        ValueExpr::ValueOf { args, default, .. } => {
            slots(args, visit);
            if let Some(d) = default {
                walk_value(d, visit);
            }
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            walk_prop(when, visit);
            walk_value(then, visit);
            walk_value(otherwise, visit);
        }
        ValueExpr::Call { args, .. } => {
            for a in args {
                walk_value(a, visit);
            }
        }
    }
}

fn slots<'a>(args: &'a [Term], visit: &mut dyn FnMut(Node<'a>)) {
    for a in args {
        visit(Node::Slot(a));
    }
}

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
