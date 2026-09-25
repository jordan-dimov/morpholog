//! One read-only walk over the IR tree, shared by every pass that collects
//! or searches without judging.
//!
//! The walk knows tree topology and nothing else: which children a node
//! has, which term slots take only terms, and which quantifier binders
//! enclose a position. Consumers decide what a node means: whether a
//! definition call is followed, what a binder does, how a node evaluates.
//! Passes that rewrite or judge a tree (substitution, kind inference,
//! evaluation) keep their own match.
//!
//! The matches have no wildcard arm, so a new IR variant is one edit here
//! and the consumers cannot drift apart.

use crate::ir::{Prop, Stmt, Term, ValueExpr, Var};

/// A node the walk hands to its visitor.
#[derive(Clone, Copy)]
pub enum Node<'a> {
    Stmt(&'a Stmt),
    Prop(&'a Prop),
    /// A value expression, including a bare term in value position.
    Value(&'a ValueExpr),
    /// A term in a slot that takes only terms: a claim or call argument,
    /// an `in` operand, an extremum target, a lookup key.
    Slot(&'a Term),
    /// A quantifier introducing this name for its body.
    Binder(&'a Var),
}

/// Visit the statement, then everything under it, `for` bodies included.
pub fn walk_stmt<'a>(stmt: &'a Stmt, visit: &mut dyn FnMut(Node<'a>)) {
    stmt_scoped(stmt, &mut |n, _| visit(n), &mut Vec::new());
}

/// Visit the proposition, then everything under it, in pre-order.
pub fn walk_prop<'a>(prop: &'a Prop, visit: &mut dyn FnMut(Node<'a>)) {
    prop_scoped(prop, &mut |n, _| visit(n), &mut Vec::new());
}

/// Visit the value expression, then everything under it, in pre-order.
pub fn walk_value<'a>(expr: &'a ValueExpr, visit: &mut dyn FnMut(Node<'a>)) {
    value_scoped(expr, &mut |n, _| visit(n), &mut Vec::new());
}

/// [`walk_prop`], with the quantifier binders enclosing each node. A
/// `forall` source is outside its own binding.
pub(crate) fn walk_prop_scoped<'a>(prop: &'a Prop, visit: &mut dyn FnMut(Node<'a>, &[&'a Var])) {
    prop_scoped(prop, visit, &mut Vec::new());
}

/// Value-sort companion to [`walk_prop_scoped`].
pub(crate) fn walk_value_scoped<'a>(
    expr: &'a ValueExpr,
    visit: &mut dyn FnMut(Node<'a>, &[&'a Var]),
) {
    value_scoped(expr, visit, &mut Vec::new());
}

fn stmt_scoped<'a>(
    stmt: &'a Stmt,
    visit: &mut dyn FnMut(Node<'a>, &[&'a Var]),
    scope: &mut Vec<&'a Var>,
) {
    visit(Node::Stmt(stmt), scope);
    match stmt {
        Stmt::Require { prop, .. } | Stmt::BindOne { prop, .. } => {
            prop_scoped(prop, visit, scope);
        }
        Stmt::Let { value, .. } => value_scoped(value, visit, scope),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(claim) => slots(&claim.args, visit, scope),
        Stmt::Retract { args, .. } => slots(args, visit, scope),
        Stmt::Emit(intent) => slots(&intent.args, visit, scope),
        Stmt::For {
            collection, body, ..
        } => {
            value_scoped(collection, visit, scope);
            for s in body {
                stmt_scoped(s, visit, scope);
            }
        }
    }
}

fn prop_scoped<'a>(
    prop: &'a Prop,
    visit: &mut dyn FnMut(Node<'a>, &[&'a Var]),
    scope: &mut Vec<&'a Var>,
) {
    visit(Node::Prop(prop), scope);
    match prop {
        Prop::Claim { args, .. } | Prop::Defined { args, .. } => slots(args, visit, scope),
        Prop::In(l, r) => {
            visit(Node::Slot(l), scope);
            visit(Node::Slot(r), scope);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                prop_scoped(p, visit, scope);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            prop_scoped(left, visit, scope);
            prop_scoped(right, visit, scope);
        }
        Prop::Not(p) | Prop::Pre(p) => prop_scoped(p, visit, scope),
        Prop::Exists { binding, body } => {
            visit(Node::Binder(binding), scope);
            scope.push(binding);
            prop_scoped(body, visit, scope);
            scope.pop();
        }
        Prop::Forall {
            binding,
            source,
            body,
        } => {
            visit(Node::Binder(binding), scope);
            prop_scoped(source, visit, scope);
            scope.push(binding);
            prop_scoped(body, visit, scope);
            scope.pop();
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            value_scoped(l, visit, scope);
            value_scoped(r, visit, scope);
        }
        Prop::Compare { left, right, .. } => {
            value_scoped(left, visit, scope);
            value_scoped(right, visit, scope);
        }
    }
}

fn value_scoped<'a>(
    expr: &'a ValueExpr,
    visit: &mut dyn FnMut(Node<'a>, &[&'a Var]),
    scope: &mut Vec<&'a Var>,
) {
    visit(Node::Value(expr), scope);
    match expr {
        ValueExpr::Term(_) => {}
        ValueExpr::Arith { left, right, .. } => {
            value_scoped(left, visit, scope);
            value_scoped(right, visit, scope);
        }
        ValueExpr::Sum {
            value,
            body,
            seed: _,
        } => {
            value_scoped(value, visit, scope);
            prop_scoped(body, visit, scope);
        }
        ValueExpr::Extremum { value, body, .. } => {
            visit(Node::Slot(value), scope);
            prop_scoped(body, visit, scope);
        }
        ValueExpr::ValueOf { args, default, .. } => {
            slots(args, visit, scope);
            if let Some(d) = default {
                value_scoped(d, visit, scope);
            }
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            prop_scoped(when, visit, scope);
            value_scoped(then, visit, scope);
            value_scoped(otherwise, visit, scope);
        }
        ValueExpr::Call { args, .. } => {
            for a in args {
                value_scoped(a, visit, scope);
            }
        }
    }
}

fn slots<'a>(
    args: &'a [Term],
    visit: &mut dyn FnMut(Node<'a>, &[&'a Var]),
    scope: &mut Vec<&'a Var>,
) {
    for a in args {
        visit(Node::Slot(a), scope);
    }
}

/// True when any `Prop` node in the tree satisfies `f`, the root
/// included. `Defined` is a leaf: a call's body is scanned at its own
/// declaration.
pub(crate) fn any_prop_node(prop: &Prop, f: &impl Fn(&Prop) -> bool) -> bool {
    let mut hit = false;
    walk_prop(prop, &mut |n| {
        if let Node::Prop(p) = n {
            hit |= f(p);
        }
    });
    hit
}

/// Does the proposition contain `pre(...)` anywhere, including inside
/// a comparison operand or `sum` body?
pub(crate) fn mentions_pre(prop: &Prop) -> bool {
    any_prop_node(prop, &|p| matches!(p, Prop::Pre(_)))
}

/// True when any term in the tree satisfies `f`, in a slot or in value
/// position. `f` also sees the quantifier binders enclosing the term, so
/// a caller matching variables by name can honour shadowing.
pub(crate) fn any_term_in_prop(prop: &Prop, f: &impl Fn(&Term, &[&Var]) -> bool) -> bool {
    let mut hit = false;
    walk_prop_scoped(prop, &mut |n, scope| {
        hit |= term_of(n).is_some_and(|t| f(t, scope))
    });
    hit
}

/// Value-sort companion to [`any_term_in_prop`].
pub(crate) fn any_term_in_value(expr: &ValueExpr, f: &impl Fn(&Term, &[&Var]) -> bool) -> bool {
    let mut hit = false;
    walk_value_scoped(expr, &mut |n, scope| {
        hit |= term_of(n).is_some_and(|t| f(t, scope))
    });
    hit
}

fn term_of<'a>(node: Node<'a>) -> Option<&'a Term> {
    match node {
        Node::Slot(t) | Node::Value(ValueExpr::Term(t)) => Some(t),
        Node::Stmt(_) | Node::Prop(_) | Node::Value(_) | Node::Binder(_) => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::ir::{SumSeed, Value};

    fn claim(pred: &str) -> Prop {
        Prop::Claim {
            predicate: pred.into(),
            args: vec![],
        }
    }

    fn var_term(name: &str) -> Term {
        Term::Var(Var::from(name))
    }

    fn is_x(t: &Term, _scope: &[&Var]) -> bool {
        matches!(t, Term::Var(v) if v.as_str() == "x")
    }

    /// Each descent must find a match that sits only in its first
    /// branch, which a dropped edge would miss.
    #[test]
    fn a_match_in_the_first_branch_alone_is_found() {
        // Forall: pre only in the SOURCE, body clean.
        let forall = Prop::Forall {
            binding: Var::from("b"),
            source: Box::new(Prop::Pre(Box::new(claim("P")))),
            body: Box::new(claim("Q")),
        };
        assert!(mentions_pre(&forall));

        // Value-sort arithmetic: pre only in the LEFT operand.
        let arith_prop = Prop::Eq(
            Box::new(ValueExpr::Arith {
                op: crate::ArithOp::Add,
                left: Box::new(ValueExpr::Sum {
                    value: Box::new(Term::Literal(Value::Decimal("1".into())).into()),
                    body: Box::new(Prop::Pre(Box::new(claim("P")))),
                    seed: SumSeed::Decimal,
                }),
                right: Box::new(ValueExpr::Term(Term::Wildcard)),
            }),
            Box::new(ValueExpr::Term(Term::Wildcard)),
        );
        assert!(mentions_pre(&arith_prop));

        // In: the sought term only on the LEFT side.
        let membership = Prop::In(var_term("x"), var_term("ys"));
        assert!(any_term_in_prop(&membership, &is_x));

        // ValueOf: the term only in the ARGS, with a default present
        // that does not carry it.
        let lookup = ValueExpr::ValueOf {
            predicate: "P".into(),
            args: vec![var_term("x"), Term::Wildcard],
            extract: 1,
            default: Some(Box::new(ValueExpr::Term(var_term("other")))),
        };
        assert!(any_term_in_value(&lookup, &is_x));

        // Sum: the term is the TARGET only, body clean.
        let sum = ValueExpr::Sum {
            value: Box::new(var_term("x").into()),
            body: Box::new(claim("P")),
            seed: SumSeed::Decimal,
        };
        assert!(any_term_in_value(&sum, &is_x));

        // Term-sort arithmetic: the term only in the LEFT operand.
        let arith = ValueExpr::Arith {
            op: crate::ArithOp::Add,
            left: Box::new(ValueExpr::Term(var_term("x"))),
            right: Box::new(ValueExpr::Term(var_term("other"))),
        };
        assert!(any_term_in_value(&arith, &is_x));
    }

    /// A `forall` binds its body, not its source: a term in the source
    /// sees the outer scope.
    #[test]
    fn a_forall_source_is_outside_its_own_binding() {
        let forall = Prop::Forall {
            binding: Var::from("x"),
            source: Box::new(Prop::Claim {
                predicate: "S".into(),
                args: vec![var_term("x")],
            }),
            body: Box::new(Prop::Claim {
                predicate: "B".into(),
                args: vec![var_term("x")],
            }),
        };
        let unbound_x = |t: &Term, scope: &[&Var]| matches!(t, Term::Var(v) if v.as_str() == "x" && !scope.contains(&v));
        assert!(any_term_in_prop(&forall, &unbound_x));
        let bound_x = |t: &Term, scope: &[&Var]| matches!(t, Term::Var(v) if v.as_str() == "x" && scope.contains(&v));
        assert!(any_term_in_prop(&forall, &bound_x));
        let exists = Prop::Exists {
            binding: Var::from("x"),
            body: Box::new(Prop::Claim {
                predicate: "P".into(),
                args: vec![var_term("x")],
            }),
        };
        assert!(!any_term_in_prop(&exists, &unbound_x));
        assert!(any_term_in_prop(&exists, &bound_x));
    }
}
