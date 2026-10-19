//! The shapes around a `periods` grid that the checker and the lint
//! tier both recognise: the gate that bounds a window, and the call
//! that builds a grid over it.

use jiff::SignedDuration;

use crate::ir::{ArithOp, Builtin, CompareOp, OrderedDomain, Prop, Term, Value, ValueExpr, Var};

/// A gate of the form `until - from no_longer_than duration(...)` (or
/// strictly shorter), or the same comparison written the other way
/// round, with a positive literal bound.
pub(crate) struct WindowBound<'a> {
    pub(crate) until: &'a Var,
    pub(crate) from: &'a Var,
    pub(crate) bound: SignedDuration,
}

/// The top-level conjuncts of a proposition.
pub(crate) fn conjuncts(prop: &Prop) -> Vec<&Prop> {
    match prop {
        Prop::And(parts) => parts.iter().flat_map(conjuncts).collect(),
        other => vec![other],
    }
}

pub(crate) fn window_bound(prop: &Prop) -> Option<WindowBound<'_>> {
    let Prop::Compare {
        op,
        domain: OrderedDomain::Duration,
        left,
        right,
    } = prop
    else {
        return None;
    };
    // The window on the small side of the comparison, the bound on the large.
    let (window, limit) = match op {
        CompareOp::Le | CompareOp::Lt => (left, right),
        CompareOp::Ge | CompareOp::Gt => (right, left),
    };
    let ValueExpr::Arith {
        op: ArithOp::Sub,
        left: until,
        right: from,
    } = window.as_ref()
    else {
        return None;
    };
    let (ValueExpr::Term(Term::Var(until)), ValueExpr::Term(Term::Var(from))) =
        (until.as_ref(), from.as_ref())
    else {
        return None;
    };
    let ValueExpr::Term(Term::Literal(Value::Duration(bound))) = limit.as_ref() else {
        return None;
    };
    if bound.is_zero() || bound.is_negative() {
        return None;
    }
    Some(WindowBound {
        until,
        from,
        bound: *bound,
    })
}

/// A `periods(from, until, step)` call's three arguments.
pub(crate) fn grid_call(expr: &ValueExpr) -> Option<(&ValueExpr, &ValueExpr, &ValueExpr)> {
    match expr {
        ValueExpr::Call {
            builtin: Builtin::Periods,
            args,
        } if args.len() == 3 => Some((&args[0], &args[1], &args[2])),
        _ => None,
    }
}

/// The most periods a grid over a window no longer than `bound` can
/// hold: one per whole step, plus one for a window off the grid.
pub(crate) fn max_periods(bound: SignedDuration, step: SignedDuration) -> u128 {
    let (bound, step) = (
        bound.as_nanos().max(0) as u128,
        step.as_nanos().max(1) as u128,
    );
    bound.div_ceil(step) + 1
}
