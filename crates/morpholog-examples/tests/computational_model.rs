//! What each construct of the language ranges over, so a new construct
//! cannot arrive without naming the termination premise it relies on
//! (`docs/computational-model.md`). The matches below have no wildcard
//! arm: a new variant does not compile here until someone classifies it.
//! That is a review obligation the compiler enforces, not a proof that
//! the classification is honest.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use morpholog_core::fold::{Node, walk_prop, walk_stmt, walk_value};
use morpholog_core::{
    Builtin, Outcome, Prop, RejectionReason, State, Stmt, Term, Value, ValueExpr,
};
use morpholog_surface::parse_program;
use morpholog_test_support::{claim_instance, fresh, subj, test_transition};

/// What evaluating a construct does beyond a fixed amount of its own work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Draws {
    /// Evaluates its sub-expressions or sub-statements, each of lower rank.
    Children,
    /// Evaluates a definition's body; definitions call each other acyclically.
    DefinitionBody,
    /// Matches against the finite admitted state.
    AdmittedState,
    /// Ranges over a materialised finite collection.
    MaterialisedCollection,
    /// Takes one subject from the supplied subject input.
    SubjectInput,
    /// Runs a loop of its own, with its own ranking argument.
    InternallyBounded,
}
use Draws::*;

const ALL: [Draws; 6] = [
    Children,
    DefinitionBody,
    AdmittedState,
    MaterialisedCollection,
    SubjectInput,
    InternallyBounded,
];

fn stmt(s: &Stmt) -> &'static [Draws] {
    match s {
        Stmt::Require { .. } | Stmt::BindOne { .. } | Stmt::Let { .. } => &[Children],
        Stmt::LetNewSubject { .. } => &[SubjectInput],
        // Staged, never read back by the same body.
        Stmt::Assert(_) | Stmt::Emit(_) => &[],
        Stmt::Retract { .. } => &[AdmittedState],
        Stmt::For { .. } => &[MaterialisedCollection, Children],
    }
}

fn prop(p: &Prop) -> &'static [Draws] {
    match p {
        Prop::Claim { .. } => &[AdmittedState],
        Prop::Defined { .. } => &[DefinitionBody],
        // `xor` evaluates through a lowering that copies both operands:
        // larger, but of lower rank.
        Prop::Implies { .. }
        | Prop::Exists { .. }
        | Prop::And(_)
        | Prop::Or(_)
        | Prop::Pre(_)
        | Prop::Not(_)
        | Prop::Xor(..)
        | Prop::Eq(..)
        | Prop::Neq(..)
        | Prop::Compare { .. }
        | Prop::Forall { .. } => &[Children],
        Prop::In(..) => &[MaterialisedCollection],
    }
}

fn value(v: &ValueExpr) -> &'static [Draws] {
    match v {
        ValueExpr::Term(t) => term(t),
        ValueExpr::Arith { .. } | ValueExpr::Extremum { .. } | ValueExpr::Cond { .. } => {
            &[Children]
        }
        // An exact total is normalised by stripping trailing zeros.
        ValueExpr::Sum { .. } => &[Children, InternallyBounded],
        ValueExpr::ValueOf { .. } => &[AdmittedState, Children],
        ValueExpr::Call { builtin, .. } => match builtin {
            Builtin::Abs
            | Builtin::Round
            | Builtin::PeriodStartOf
            | Builtin::Min
            | Builtin::Max => &[Children],
            // A binary search over the calendar's day range.
            Builtin::PeriodIndex => &[Children, InternallyBounded],
        },
    }
}

fn term(t: &Term) -> &'static [Draws] {
    match t {
        Term::Var(_) | Term::Wildcard | Term::Actor => &[],
        Term::Literal(v) => match v {
            Value::Decimal(_)
            | Value::Subject(_)
            | Value::Date(_)
            | Value::Timestamp(_)
            | Value::Duration(_)
            | Value::Quantity { .. } => &[],
            // Parsed by the kernel's own span grammar on use.
            Value::CalendarSpan(_) => &[InternallyBounded],
        },
    }
}

fn classify(node: Node<'_>, seen: &mut BTreeSet<Draws>) {
    let draws = match node {
        Node::Stmt(s) => stmt(s),
        Node::Prop(p) => prop(p),
        Node::Value(v) => value(v),
        Node::Slot(t) => term(t),
        Node::Binder(_) => &[],
    };
    seen.extend(draws.iter().copied());
}

/// Every class is one the gallery reaches, so none is dead weight.
#[test]
fn every_construct_names_what_it_ranges_over() {
    let mut seen = BTreeSet::new();
    for p in common::all_programs() {
        let mut visit = |n| classify(n, &mut seen);
        for t in &p.transformations {
            for s in &t.body {
                walk_stmt(s, &mut visit);
            }
        }
        for i in &p.invariants {
            walk_prop(&i.body, &mut visit);
        }
        for d in &p.definitions {
            walk_prop(&d.body, &mut visit);
        }
        for d in &p.derived_claims {
            walk_prop(&d.domain, &mut visit);
            for v in &d.values {
                walk_value(&v.expr, &mut visit);
            }
        }
    }
    let unused: Vec<_> = ALL.iter().filter(|d| !seen.contains(d)).collect();
    assert!(unused.is_empty(), "no worked example draws on {unused:?}");
}

/// A body reads the state before it, never its own staged writes, so a
/// transformation is one pass with no feedback from what it admits.
#[test]
fn a_transformation_never_reads_what_it_admits() {
    let p = parse_program(
        "
program snapshot
predicate Seen(x: Subject)

transformation admit_then_read(x):
    admit Seen(x)
    require Seen(x)
",
    )
    .unwrap();
    let t = p.transformation("admit_then_read").unwrap();
    let transition = test_transition(t, vec![subj("a")]);
    let run = |state: &State| {
        morpholog_core::propose(
            t,
            &transition,
            state,
            &p.invariants,
            &p.definitions,
            &mut fresh(),
        )
        .unwrap()
    };

    let outcome = run(&State::default());
    assert!(
        matches!(
            outcome,
            Outcome::Rejected {
                reason: RejectionReason::Require { .. },
                ..
            }
        ),
        "its own admission must not satisfy its gate: {outcome:?}"
    );
    let outcome = run(&State::from_claims(vec![claim_instance(
        "Seen",
        &[subj("a")],
    )]));
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "the pre-state satisfies it: {outcome:?}"
    );
}
