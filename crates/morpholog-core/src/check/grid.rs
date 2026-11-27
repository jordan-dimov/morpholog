//! The shapes around a `periods` grid that the checker and the lint
//! tier both recognise: the gate that bounds a window, and the call
//! that builds a grid over it.

use jiff::SignedDuration;

use crate::fold::Node;
use crate::ir::{
    ArithOp, Builtin, CompareOp, OrderedDomain, Prop, Stmt, Term, Value, ValueExpr, Var,
};

/// A gate of the form `until - from no_longer_than duration(...)` (or
/// strictly shorter), or the same comparison written the other way
/// round, with a positive literal bound.
#[derive(Clone, Copy)]
pub(crate) struct WindowBound<'a> {
    pub(crate) until: &'a Var,
    pub(crate) from: &'a Var,
    pub(crate) bound: SignedDuration,
}

/// The top-level conjuncts of a proposition.
pub(crate) fn conjuncts(prop: &Prop) -> Vec<&Prop> {
    match prop {
        Prop::And(parts) => parts.iter().flat_map(conjuncts).collect(),
        other @ (Prop::Claim { .. }
        | Prop::Defined { .. }
        | Prop::Implies { .. }
        | Prop::Exists { .. }
        | Prop::Or(_)
        | Prop::Pre(_)
        | Prop::Not(_)
        | Prop::Xor(..)
        | Prop::Eq(..)
        | Prop::Neq(..)
        | Prop::Compare { .. }
        | Prop::Forall { .. }
        | Prop::In(..)) => vec![other],
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
        ValueExpr::Term(_)
        | ValueExpr::Arith { .. }
        | ValueExpr::Sum { .. }
        | ValueExpr::Extremum { .. }
        | ValueExpr::ValueOf { .. }
        | ValueExpr::Cond { .. }
        | ValueExpr::Call { .. }
        | ValueExpr::Defined { .. } => None,
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

/// What one rule or one definition body says about the windows its
/// grids range over: every window bound it states, and the names a `let`
/// makes an alias of another or aliases.
#[derive(Clone, Default)]
pub(crate) struct Frame<'a> {
    bounds: Vec<WindowBound<'a>>,
    aliased: Vec<&'a Var>,
}

/// The frame of `nodes`, the nodes of one rule or one definition body.
pub(crate) fn frame<'a>(nodes: &[Node<'a>]) -> Frame<'a> {
    let mut frame = Frame::default();
    for node in nodes {
        match node {
            Node::Prop(prop) => frame.bounds.extend(window_bound(prop)),
            Node::Stmt(Stmt::Let {
                name,
                value: ValueExpr::Term(Term::Var(of)),
            }) => frame.aliased.extend([name, of]),
            Node::Stmt(_) | Node::Value(_) | Node::Slot(_) | Node::Binder(_) => {}
        }
    }
    frame
}

/// A grid's size: the window bound and literal step that fix it, and the
/// most periods they allow.
pub(crate) struct GridSize {
    pub(crate) bound: SignedDuration,
    pub(crate) step: SignedDuration,
    pub(crate) periods: u128,
}

/// The size of `expr` when it is a `periods` call over two variables the
/// frame bounds, with a literal step. Validation accepts a grid only
/// after a bound that holds where it stands, matched through aliases and
/// rebindings, and this match is by name. So the largest bound with the
/// grid's names is taken, which is at least the one that holds, and a
/// grid over an aliased name has no size here: its bound may be written
/// with the other name.
pub(crate) fn grid_size(expr: &ValueExpr, frame: &Frame<'_>) -> Option<GridSize> {
    let (from, until, step) = grid_call(expr)?;
    let (
        ValueExpr::Term(Term::Var(from)),
        ValueExpr::Term(Term::Var(until)),
        ValueExpr::Term(Term::Literal(Value::Duration(step))),
    ) = (from, until, step)
    else {
        return None;
    };
    if frame.aliased.contains(&from) || frame.aliased.contains(&until) {
        return None;
    }
    let bound = frame
        .bounds
        .iter()
        .filter(|b| b.until == until && b.from == from)
        .map(|b| b.bound)
        .max()?;
    Some(GridSize {
        bound,
        step: *step,
        periods: max_periods(bound, *step),
    })
}
