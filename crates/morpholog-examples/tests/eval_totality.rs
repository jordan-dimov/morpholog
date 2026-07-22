//! The eval-totality property over the whole example gallery: a
//! declared transformation, given kind-lawful arguments, never
//! produces a kernel `EvalError` - every outcome is lawful, an
//! acceptance or a refusal. A kernel error reachable through the
//! declared surface is a programme bug, and it is exactly the class a
//! hand-scripted happy path never visits: the states nobody proposes
//! from. The empty-sum landmine lived here - an aggregate invariant
//! that detonated on the very first commission, when its book was
//! empty - so the property walks the boundary states deliberately:
//! every transformation from the empty state, then every
//! transformation again from each state one accepted step away.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::all_programs;

use morpholog_core::{
    EvalValue, ParamKind, PredicateArgKind, Program, State, transformation_param_kinds,
};
use morpholog_test_support::{Example, bool_, coll, date, dec, dur, qty, subj, ts};

/// One accepted step from empty, then every transformation again: the
/// depth that reaches first-commission invariant evaluation (a gate
/// satisfied by the first step, an aggregate over a near-empty book).
const REACHABILITY_DEPTH: usize = 2;

/// A fixed, kind-lawful argument for one parameter. Deterministic by
/// construction; the identity of subjects is arbitrary because kind
/// inference never constrains WHICH subject, only that it is one - a
/// wrong guess against a gate is a lawful refusal, never an error.
fn plausible(kind: &ParamKind, position: usize) -> EvalValue {
    match kind {
        ParamKind::Concrete(k) => plausible_concrete(k, position),
        ParamKind::Collection(inner) => coll(vec![plausible(inner, position)]),
        ParamKind::Polymorphic | ParamKind::Unconstrained => subj(&format!("p{position}")),
        ParamKind::Ambiguous(kinds) => kinds
            .first()
            .map(|k| plausible_concrete(k, position))
            .unwrap_or_else(|| subj(&format!("p{position}"))),
    }
}

fn plausible_concrete(kind: &PredicateArgKind, position: usize) -> EvalValue {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Any => subj(&format!("p{position}")),
        PredicateArgKind::Decimal => dec(1),
        PredicateArgKind::Date => date("2026-07-01"),
        PredicateArgKind::Timestamp => ts("2026-07-01T12:00:00Z"),
        PredicateArgKind::Duration => dur("PT1H"),
        PredicateArgKind::Bool => bool_(true),
        PredicateArgKind::Quantity(unit) => qty("1", unit.as_str()),
        PredicateArgKind::Collection => coll(vec![subj(&format!("p{position}"))]),
    }
}

fn args_for(program: &Program, name: &morpholog_core::TransformationName) -> Vec<EvalValue> {
    let validated = program.validated().expect("gallery programme validates");
    transformation_param_kinds(&validated, name)
        .expect("gallery parameter kinds resolve")
        .iter()
        .enumerate()
        .map(|(i, (_, kind))| plausible(kind, i))
        .collect()
}

/// Propose every transformation of `program` against `pre`, asserting
/// lawfulness; return the successor state of each acceptance.
fn propose_all(program: &Program, ex: &Example, pre: &State) -> Vec<State> {
    let mut successors = Vec::new();
    for t in &program.transformations {
        let args = args_for(program, &t.name);
        match ex.propose(t, args.clone(), pre) {
            Ok(morpholog_core::Outcome::Accepted {
                candidate_state, ..
            }) => successors.push(candidate_state),
            Ok(morpholog_core::Outcome::Rejected { .. }) => {}
            Err(e) => panic!(
                "`{}::{}` with kind-lawful args {args:?} raised a kernel error \
                 instead of a lawful outcome: {e:?}",
                program.name, t.name
            ),
        }
    }
    successors
}

#[test]
fn no_declared_transformation_can_raise_a_kernel_error_near_the_boundary() {
    for program in all_programs() {
        let ex = Example::new(&program);
        let mut frontier = vec![State::default()];
        for _ in 0..REACHABILITY_DEPTH {
            let mut next = Vec::new();
            for state in &frontier {
                next.extend(propose_all(&program, &ex, state));
            }
            frontier = next;
        }
    }
}
