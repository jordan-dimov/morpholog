//! Shared structural descent over `Prop` / `ValueExpr` trees.
//!
//! The boolean "does any subterm satisfy this?" walkers used to
//! hand-copy the same recursion, and the copies drifted. These folds
//! own the descent once. The matches are exhaustive with no wildcard
//! arm, so a new IR variant forces exactly one edit here instead of
//! one per walker.

use crate::ir::{ExtremumOp, Prop, Stmt, Term, ValueExpr, Var};

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
        ValueExpr::Sum { body, .. } | ValueExpr::Extremum { body, .. } => any_prop_node(body, f),
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            any_prop_node(when, f)
                || any_prop_node_in_value(then, f)
                || any_prop_node_in_value(otherwise, f)
        }
        ValueExpr::Arith { left, right, .. } => {
            any_prop_node_in_value(left, f) || any_prop_node_in_value(right, f)
        }
        ValueExpr::PeriodIndex { anchor, span, at } => {
            any_prop_node_in_value(anchor, f)
                || any_prop_node_in_value(span, f)
                || any_prop_node_in_value(at, f)
        }
        ValueExpr::Abs(inner) => any_prop_node_in_value(inner, f),
        ValueExpr::Round { value, quantum } => {
            any_prop_node_in_value(value, f) || any_prop_node_in_value(quantum, f)
        }
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
        }
        | ValueExpr::Extremum { value, body, .. } => {
            f(value, scope) || any_term_prop_scoped(body, f, scope)
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            any_term_prop_scoped(when, f, scope)
                || any_term_value_scoped(then, f, scope)
                || any_term_value_scoped(otherwise, f, scope)
        }
        ValueExpr::Arith { left, right, .. } => {
            any_term_value_scoped(left, f, scope) || any_term_value_scoped(right, f, scope)
        }
        ValueExpr::PeriodIndex { anchor, span, at } => {
            any_term_value_scoped(anchor, f, scope)
                || any_term_value_scoped(span, f, scope)
                || any_term_value_scoped(at, f, scope)
        }
        ValueExpr::Abs(inner) => any_term_value_scoped(inner, f, scope),
        ValueExpr::Round { value, quantum } => {
            any_term_value_scoped(value, f, scope) || any_term_value_scoped(quantum, f, scope)
        }
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

    /// Each disjunctive descent must find a match sitting ONLY in its
    /// first branch - the position a short-circuit `||` mutated to
    /// `&&` silently stops seeing.
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
                    value: Term::Literal(Value::Decimal("1".into())),
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
            default: Some(Box::new(ValueExpr::Term(var_term("other")))),
        };
        assert!(any_term_in_value(&lookup, &is_x));

        // Sum: the term is the TARGET only, body clean.
        let sum = ValueExpr::Sum {
            value: var_term("x"),
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
}

// ============================================================
// The rewriting half.
// ============================================================

/// Where a term sits in the grammar.
///
/// A pass that has to explain itself to an author needs to know which
/// slot it was handed: "as argument 2 of `Posted`" reads very
/// differently from "as a sum target", and only the walk knows which
/// one it just reached. Carrying the position here keeps the descent
/// in one place without flattening what a client can say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermSlot<'a> {
    /// Argument of a claim pattern.
    ClaimArg { predicate: &'a str, index: usize },
    /// Argument of a call to a definition.
    DefinedArg { callee: &'a str, index: usize },
    /// Either operand of a membership test.
    InOperand,
    /// The summed target of `sum(target | body)`.
    SumTarget,
    /// The target of `min` / `max`.
    ExtremumTarget { op: ExtremumOp },
    /// Key argument of a `value P(..)` lookup.
    LookupArg { predicate: &'a str, index: usize },
    /// A term standing alone as a whole value expression.
    Value,
    /// A term in a statement's own argument list - `admit`,
    /// `retract`, `emit`.
    StatementArg,
}

/// Whether a rewrite wants the walk to continue into the node it was
/// just handed - `Skip` for a hook that has replaced the node and
/// does not want its own output re-examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descend {
    Into,
    Skip,
}

/// An in-place rewrite of a `Prop` / `ValueExpr` / `Stmt` tree.
///
/// Implementors supply only what happens AT a node; the descent
/// belongs to this module. Several passes used to carry a copy of it -
/// call resolution, sum-seed lowering, and the parse-time
/// substitutions - which is one place per pass to edit for a single
/// new IR variant, and one chance per pass to forget.
///
/// Hooks fire pre-order: a node is offered to its hook, then its
/// children are walked unless the hook answered [`Descend::Skip`].
pub trait Rewrite {
    fn prop(&mut self, _prop: &mut Prop) -> Descend {
        Descend::Into
    }
    fn value(&mut self, _value: &mut ValueExpr) -> Descend {
        Descend::Into
    }
    fn term(&mut self, _term: &mut Term, _slot: TermSlot<'_>) {}
}

/// Rewrite every node of a proposition.
pub fn rewrite_prop(prop: &mut Prop, r: &mut impl Rewrite) {
    if r.prop(prop) == Descend::Skip {
        return;
    }
    match prop {
        Prop::Claim { predicate, args } => {
            for (index, a) in args.iter_mut().enumerate() {
                let slot = TermSlot::ClaimArg {
                    predicate: predicate.as_str(),
                    index,
                };
                r.term(a, slot);
            }
        }
        Prop::Defined { name, args } => {
            for (index, a) in args.iter_mut().enumerate() {
                let slot = TermSlot::DefinedArg {
                    callee: name.as_str(),
                    index,
                };
                r.term(a, slot);
            }
        }
        Prop::In(l, rt) => {
            r.term(l, TermSlot::InOperand);
            r.term(rt, TermSlot::InOperand);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                rewrite_prop(p, r);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            rewrite_prop(left, r);
            rewrite_prop(right, r);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => rewrite_prop(p, r),
        Prop::Forall { source, body, .. } => {
            rewrite_prop(source, r);
            rewrite_prop(body, r);
        }
        Prop::Eq(l, rt) | Prop::Neq(l, rt) => {
            rewrite_value(l, r);
            rewrite_value(rt, r);
        }
        Prop::Compare { left, right, .. } => {
            rewrite_value(left, r);
            rewrite_value(right, r);
        }
    }
}

/// Rewrite every node of a value expression.
pub fn rewrite_value(value: &mut ValueExpr, r: &mut impl Rewrite) {
    if r.value(value) == Descend::Skip {
        return;
    }
    match value {
        ValueExpr::Term(t) => r.term(t, TermSlot::Value),
        ValueExpr::Arith { left, right, .. } => {
            rewrite_value(left, r);
            rewrite_value(right, r);
        }
        ValueExpr::Sum {
            value: target,
            body,
            ..
        } => {
            r.term(target, TermSlot::SumTarget);
            rewrite_prop(body, r);
        }
        ValueExpr::Extremum {
            op,
            value: target,
            body,
        } => {
            r.term(target, TermSlot::ExtremumTarget { op: *op });
            rewrite_prop(body, r);
        }
        ValueExpr::ValueOf {
            predicate,
            args,
            default,
        } => {
            for (index, a) in args.iter_mut().enumerate() {
                let slot = TermSlot::LookupArg {
                    predicate: predicate.as_str(),
                    index,
                };
                r.term(a, slot);
            }
            if let Some(d) = default {
                rewrite_value(d, r);
            }
        }
        ValueExpr::Abs(operand) => rewrite_value(operand, r),
        ValueExpr::Round { value, quantum } => {
            rewrite_value(value, r);
            rewrite_value(quantum, r);
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            rewrite_prop(when, r);
            rewrite_value(then, r);
            rewrite_value(otherwise, r);
        }
        ValueExpr::PeriodIndex { anchor, span, at } => {
            rewrite_value(anchor, r);
            rewrite_value(span, r);
            rewrite_value(at, r);
        }
    }
}

/// Rewrite every node reachable from a transformation statement.
pub fn rewrite_stmt(stmt: &mut Stmt, r: &mut impl Rewrite) {
    match stmt {
        Stmt::Require { prop, .. } | Stmt::BindOne { prop, .. } => rewrite_prop(prop, r),
        Stmt::Let { value, .. } => rewrite_value(value, r),
        Stmt::Assert(claim) => {
            for a in &mut claim.args {
                r.term(a, TermSlot::StatementArg);
            }
        }
        Stmt::Emit(intent) => {
            for a in &mut intent.args {
                r.term(a, TermSlot::StatementArg);
            }
        }
        Stmt::Retract { args, .. } => {
            for a in args {
                r.term(a, TermSlot::StatementArg);
            }
        }
        Stmt::LetNewSubject { .. } => {}
        Stmt::For {
            collection, body, ..
        } => {
            rewrite_value(collection, r);
            for inner in body {
                rewrite_stmt(inner, r);
            }
        }
    }
}

/// A read-only walk, the counting twin of [`Rewrite`]. The boolean
/// folds above short-circuit; an accumulator needs everything.
pub trait Visit {
    fn prop(&mut self, _prop: &Prop) {}
    fn value(&mut self, _value: &ValueExpr) {}
    fn term(&mut self, _term: &Term, _slot: TermSlot<'_>) {}
}

/// Visit every node of a proposition.
pub fn visit_prop(prop: &Prop, v: &mut impl Visit) {
    v.prop(prop);
    match prop {
        Prop::Claim { predicate, args } => {
            for (index, a) in args.iter().enumerate() {
                let slot = TermSlot::ClaimArg {
                    predicate: predicate.as_str(),
                    index,
                };
                v.term(a, slot);
            }
        }
        Prop::Defined { name, args } => {
            for (index, a) in args.iter().enumerate() {
                let slot = TermSlot::DefinedArg {
                    callee: name.as_str(),
                    index,
                };
                v.term(a, slot);
            }
        }
        Prop::In(l, r) => {
            v.term(l, TermSlot::InOperand);
            v.term(r, TermSlot::InOperand);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                visit_prop(p, v);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            visit_prop(left, v);
            visit_prop(right, v);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => visit_prop(p, v),
        Prop::Forall { source, body, .. } => {
            visit_prop(source, v);
            visit_prop(body, v);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            visit_value(l, v);
            visit_value(r, v);
        }
        Prop::Compare { left, right, .. } => {
            visit_value(left, v);
            visit_value(right, v);
        }
    }
}

/// Visit every node of a value expression.
pub fn visit_value(value: &ValueExpr, v: &mut impl Visit) {
    v.value(value);
    match value {
        ValueExpr::Term(t) => v.term(t, TermSlot::Value),
        ValueExpr::Arith { left, right, .. } => {
            visit_value(left, v);
            visit_value(right, v);
        }
        ValueExpr::Sum {
            value: target,
            body,
            ..
        } => {
            v.term(target, TermSlot::SumTarget);
            visit_prop(body, v);
        }
        ValueExpr::Extremum {
            op,
            value: target,
            body,
        } => {
            v.term(target, TermSlot::ExtremumTarget { op: *op });
            visit_prop(body, v);
        }
        ValueExpr::ValueOf {
            predicate,
            args,
            default,
        } => {
            for (index, a) in args.iter().enumerate() {
                let slot = TermSlot::LookupArg {
                    predicate: predicate.as_str(),
                    index,
                };
                v.term(a, slot);
            }
            if let Some(d) = default {
                visit_value(d, v);
            }
        }
        ValueExpr::Abs(operand) => visit_value(operand, v),
        ValueExpr::Round { value, quantum } => {
            visit_value(value, v);
            visit_value(quantum, v);
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            visit_prop(when, v);
            visit_value(then, v);
            visit_value(otherwise, v);
        }
        ValueExpr::PeriodIndex { anchor, span, at } => {
            visit_value(anchor, v);
            visit_value(span, v);
            visit_value(at, v);
        }
    }
}
