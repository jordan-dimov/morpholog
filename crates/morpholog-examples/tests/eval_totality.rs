//! Every declared transformation in the gallery, given kind-lawful arguments,
//! must accept or refuse, never return a kernel `EvalError`. Such errors are
//! programme bugs, and they hide at boundary states and boundary values,
//! where hand-written happy paths never look.
//!
//! This is not a proof. Each parameter is tried at a few boundary values
//! (zero, negative and unit numbers; both booleans; empty and singleton
//! collections; distinct and shared subjects), one parameter at a time
//! around a baseline. Subjects are named after the PARAMETER, so a `facility`
//! in one transformation meets the `facility` another one admitted; without
//! that, no proposal reaches a second transformation's deeper paths.
//!
//! Every transformation runs from the empty state, then from every state one
//! baseline acceptance away, where gates first pass and sums meet near-empty
//! books.
//!
//! Numeric witnesses include the decimal maximum. Only on vectors carrying
//! it, the out-of-range errors (`ArithOutOfRange`, `RoundOutOfRange`) are
//! expected. Anywhere else they fail the suite like any kernel error, and a
//! panic fails it everywhere.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::all_programs;

use morpholog_core::{Program, State, ValidatedProgram};
use morpholog_test_support::Example;
use morpholog_test_support::differential::boundary_argument_cases;

/// One accepted step from empty, then every transformation again: the
/// depth that reaches first-commission invariant evaluation.
const REACHABILITY_DEPTH: usize = 2;

/// Propose every argument vector of every transformation against `pre`,
/// asserting each outcome is lawful. Returns the successor of each BASELINE
/// acceptance only; keeping every variant's successor would blow up the
/// frontier without reaching deeper.
fn propose_all(
    program: &Program,
    validated: &ValidatedProgram<'_>,
    ex: &Example,
    pre: &State,
) -> Vec<State> {
    let mut successors = Vec::new();
    for t in &program.transformations {
        for (v, case) in boundary_argument_cases(validated, &t.name)
            .into_iter()
            .enumerate()
        {
            let args = case.args;
            match ex.propose(t, args.clone(), pre) {
                Ok(morpholog_core::Outcome::Accepted {
                    candidate_state, ..
                }) => {
                    if v == 0 {
                        successors.push(candidate_state);
                    }
                }
                Ok(morpholog_core::Outcome::Rejected { .. }) => {}
                // Range errors are expected only on vectors carrying the
                // extreme witness; elsewhere they fail like any kernel error.
                Err(e)
                    if case.permits_range_refusal
                        && morpholog_test_support::differential::is_permitted_range_error(&e) => {}
                Err(e) => panic!(
                    "`{}::{}` with kind-lawful args {args:?} raised a kernel error \
                     instead of a lawful outcome: {e:?}",
                    program.name, t.name
                ),
            }
        }
    }
    successors
}

#[test]
fn no_declared_transformation_raises_a_kernel_error_over_boundary_witnesses() {
    for program in all_programs() {
        let validated = program.validated().expect("gallery programme validates");
        let ex = Example::new(&program);
        // The frontier may lawfully be empty (chess accepts no baseline move
        // on an empty board). It cannot be emptied silently, because any
        // kernel error on a non-extreme vector already fails the suite.
        let mut frontier = vec![State::default()];
        for _ in 0..REACHABILITY_DEPTH {
            let mut next = Vec::new();
            for state in &frontier {
                next.extend(propose_all(&program, &validated, &ex, state));
            }
            frontier = next;
        }
    }
}
