//! Unit tests that need crate-private items (`unify_args`, `resolve_term`,
//! `Bindings`) and so cannot live in `tests/`. One file per kernel area;
//! shared helpers live here.

mod eval;
mod explain;
mod propose;
mod state;
mod validate;

use super::*;
use crate::eval::{EvalContext, eval_value, find_matches, resolve_term, unify_args};

// Comparator builders over boxed operands, to keep test call sites short.
fn le_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Le,
        domain: OrderedDomain::Decimal,
        left: l,
        right: r,
    }
}
fn lt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Lt,
        domain: OrderedDomain::Decimal,
        left: l,
        right: r,
    }
}
fn ge_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Ge,
        domain: OrderedDomain::Decimal,
        left: l,
        right: r,
    }
}
fn gt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Gt,
        domain: OrderedDomain::Decimal,
        left: l,
        right: r,
    }
}
fn date_le_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Le,
        domain: OrderedDomain::Date,
        left: l,
        right: r,
    }
}
fn date_lt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Lt,
        domain: OrderedDomain::Date,
        left: l,
        right: r,
    }
}
fn date_ge_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Ge,
        domain: OrderedDomain::Date,
        left: l,
        right: r,
    }
}
fn date_gt_(l: Box<ValueExpr>, r: Box<ValueExpr>) -> Prop {
    Prop::Compare {
        op: CompareOp::Gt,
        domain: OrderedDomain::Date,
        left: l,
        right: r,
    }
}
use crate::state::Bindings;
use jiff::civil::Date;
use rust_decimal::Decimal;
use std::collections::BTreeSet;

/// No-actor, no-pre EvalContext for standalone expression evaluation.
fn ctx<'a>(state: &'a State, bindings: &'a Bindings) -> EvalContext<'a> {
    EvalContext::new(
        state,
        None,
        bindings,
        None,
        crate::definitions::DefinitionTable::new(&[]),
    )
}

/// No-actor EvalContext with both pre and post states, for `Prop::Pre`
/// tests.
fn ctx_with_pre<'a>(state: &'a State, pre: &'a State, bindings: &'a Bindings) -> EvalContext<'a> {
    EvalContext::new(
        state,
        Some(pre),
        bindings,
        None,
        crate::definitions::DefinitionTable::new(&[]),
    )
}

/// A parameterless transformation of one statement, so BindOne tests go
/// through `propose` rather than `find_matches`.
fn single_stmt_transformation(name: &str, body: Vec<Stmt>) -> Transformation {
    ir_builder::transformation(name, vec![], body)
}

fn run(t: &Transformation, state: &State) -> Result<Outcome, EvalError> {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![],
        actor: Subject::from("test_actor"),
    };
    propose(t, &transition, state, &[], &[])
}

fn trace_transition(t: &Transformation, args: Vec<EvalValue>) -> Transition {
    Transition {
        transformation_name: t.name.clone(),
        args,
        actor: Subject::from("trace_actor"),
    }
}
