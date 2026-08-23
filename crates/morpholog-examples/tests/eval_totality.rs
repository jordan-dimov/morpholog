//! Kernel-error reachability over the whole example gallery: a
//! declared transformation, given kind-lawful arguments, must produce
//! a lawful outcome - an acceptance or a refusal - never a kernel
//! `EvalError`. Kernel errors reachable through the declared surface
//! are programme bugs, and they live exactly where hand-scripted
//! happy paths never look: the boundary states, and the boundary
//! VALUES.
//!
//! The witness policy, honestly stated: this is not a proof of
//! totality. Each parameter is exercised at a small set of boundary
//! witnesses - zero, negative, and unit values for numerics; both
//! booleans; empty and singleton collections; distinct and shared
//! subjects - varied one parameter at a time around a baseline vector
//! (full Cartesian expansion buys little over one-at-a-time variation
//! here, and the suite stays fast). Subjects are derived from the
//! PARAMETER NAME, so a `facility` in one transformation joins the
//! `facility` another transformation admitted - without that, no
//! generated proposal ever reaches a second transformation's deeper
//! path.
//!
//! Reachability: every transformation proposes from the empty state,
//! then again from every state one BASELINE acceptance step away -
//! the ring where gates pass for the first time and aggregate
//! invariants meet near-empty books (the empty-sum landmine's home).
//!
//! Range extremes: numeric witnesses include the decimal maximum, so
//! recompute invariants multiply values past the exact range. The
//! contract there is refined, not waived: on the vectors that CARRY
//! the extreme witness - and only those - the named out-of-range
//! refusals (`ArithOutOfRange`, `RoundOutOfRange`) are the expected
//! outcome, the checked-arithmetic contract working. They remain
//! kernel evaluation errors, not business rejections; on baseline and
//! ordinary boundary vectors they fail the suite like any other
//! kernel error, and a panic fails it everywhere.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::all_programs;

use morpholog_core::{Program, State, ValidatedProgram};
use morpholog_test_support::Example;
use morpholog_test_support::differential::boundary_argument_cases;

/// One accepted step from empty, then every transformation again: the
/// depth that reaches first-commission invariant evaluation.
const REACHABILITY_DEPTH: usize = 2;

/// Propose every argument vector of every transformation against
/// `pre`, asserting lawfulness; return the successor state of each
/// BASELINE acceptance (variations probe values, the baseline drives
/// reachability - collecting every variant's successor would explode
/// the frontier without deepening it).
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
                // On a vector carrying the range-extreme witness, the
                // named out-of-range refusals are the expected outcome
                // - checked arithmetic refusing an unrepresentable
                // result. On every other vector they are failures like
                // any kernel error, so a regression producing them for
                // ordinary inputs still reddens the suite.
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
        // No frontier non-emptiness assert: an empty ring can be
        // legitimate (chess accepts no baseline move over an empty
        // board), and the vacuity it would guard against - a swallowed
        // baseline range refusal silently emptying the ring - is
        // already impossible: on non-extreme vectors every kernel
        // error, range refusals included, panics the suite.
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
