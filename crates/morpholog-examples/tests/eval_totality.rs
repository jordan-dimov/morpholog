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

use morpholog_core::{
    EvalValue, ParamKind, PredicateArgKind, Program, State, TransformationName, ValidatedProgram,
    transformation_param_kinds,
};
use morpholog_test_support::{Example, bool_, coll, date, dec, dec_str, dur, qty, subj, ts};

/// One accepted step from empty, then every transformation again: the
/// depth that reaches first-commission invariant evaluation.
const REACHABILITY_DEPTH: usize = 2;

/// The name every shared-subject witness uses, so "two parameters
/// naming the same subject" is among the tried shapes.
const SHARED_SUBJECT: &str = "shared";

/// The exact decimal ceiling: the witness that drives recompute
/// arithmetic past the representable range.
const DECIMAL_MAX: &str = "79228162514264337593543950335";

/// The baseline argument for one parameter: kind-lawful, deterministic,
/// with subjects named after the parameter so values join across
/// transformations.
fn baseline(kind: &ParamKind, name: &str) -> EvalValue {
    match kind {
        ParamKind::Concrete(k) => baseline_concrete(k, name),
        ParamKind::Collection(inner) => coll(vec![baseline(inner, name)]),
        ParamKind::Polymorphic | ParamKind::Unconstrained => subj(name),
        ParamKind::Ambiguous(kinds) => kinds
            .first()
            .map(|k| baseline_concrete(k, name))
            .unwrap_or_else(|| subj(name)),
    }
}

fn baseline_concrete(kind: &PredicateArgKind, name: &str) -> EvalValue {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Any => subj(name),
        PredicateArgKind::Decimal => dec(1),
        PredicateArgKind::Date => date("2026-07-01"),
        PredicateArgKind::Timestamp => ts("2026-07-01T12:00:00Z"),
        PredicateArgKind::Duration => dur("PT1H"),
        PredicateArgKind::Bool => bool_(true),
        PredicateArgKind::Quantity(unit) => qty("1", unit.as_str()),
        PredicateArgKind::Collection => coll(vec![subj(name)]),
        // Expression-only: validation refuses declaring it and propose
        // refuses receiving it, so no parameter ever infers to it. The
        // subject stand-in keeps this total if that ever changes.
        PredicateArgKind::CalendarSpan => subj(name),
    }
}

/// One boundary witness: the value, and whether it is the RANGE
/// EXTREME - the only witness for which the named out-of-range
/// refusals are an expected outcome.
struct Witness {
    value: EvalValue,
    extreme: bool,
}

fn ordinary(value: EvalValue) -> Witness {
    Witness {
        value,
        extreme: false,
    }
}

/// The boundary witnesses for one parameter, beyond its baseline.
/// Zero and negative numerics reach division/remainder and band
/// checks; the shared subject reaches equality joins; the empty
/// collection reaches loops over nothing; the decimal maximum reaches
/// recompute arithmetic past the exact range.
fn boundary_witnesses(kind: &ParamKind, name: &str) -> Vec<Witness> {
    match kind {
        ParamKind::Concrete(k) => boundary_concrete(k, name),
        ParamKind::Collection(_) => vec![ordinary(coll(vec![]))],
        ParamKind::Polymorphic | ParamKind::Unconstrained => {
            vec![ordinary(subj(SHARED_SUBJECT))]
        }
        ParamKind::Ambiguous(kinds) => kinds
            .first()
            .map(|k| boundary_concrete(k, name))
            .unwrap_or_default(),
    }
}

fn boundary_concrete(kind: &PredicateArgKind, _name: &str) -> Vec<Witness> {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Any => vec![ordinary(subj(SHARED_SUBJECT))],
        PredicateArgKind::Decimal => vec![
            ordinary(dec(0)),
            ordinary(dec(-1)),
            Witness {
                value: dec_str(DECIMAL_MAX),
                extreme: true,
            },
        ],
        PredicateArgKind::Quantity(unit) => {
            vec![
                ordinary(qty("0", unit.as_str())),
                ordinary(qty("-1", unit.as_str())),
                Witness {
                    value: qty(DECIMAL_MAX, unit.as_str()),
                    extreme: true,
                },
            ]
        }
        PredicateArgKind::Bool => vec![ordinary(bool_(false))],
        PredicateArgKind::Collection => vec![ordinary(coll(vec![]))],
        // The calendar's own edges: a date at either end of the
        // representable range makes any span shift or day count near
        // the boundary reachable. Extreme, so the named out-of-range
        // refusal is lawful on vectors carrying them.
        PredicateArgKind::Date => vec![
            Witness {
                value: date("-009999-01-01"),
                extreme: true,
            },
            Witness {
                value: date("9999-12-31"),
                extreme: true,
            },
        ],
        // A single fixed instant per remaining time kind: instant and
        // duration arithmetic has no zero-like boundary an argument
        // can supply on its own.
        PredicateArgKind::Timestamp | PredicateArgKind::Duration => {
            vec![]
        }
        // Expression-only; unreachable as a parameter kind (see
        // `baseline_concrete`).
        PredicateArgKind::CalendarSpan => vec![],
    }
}

fn param_kinds(
    validated: &ValidatedProgram<'_>,
    name: &TransformationName,
) -> Vec<(String, ParamKind)> {
    transformation_param_kinds(validated, name)
        .expect("gallery parameter kinds resolve")
        .into_iter()
        .map(|(v, k)| (v.to_string(), k))
        .collect()
}

/// One generated proposal: the argument vector, and whether it
/// carries the range-extreme witness - the only case in which the
/// named out-of-range refusals are lawful. Baseline and ordinary
/// boundary vectors keep the strict contract: any kernel error fails.
struct ArgumentCase {
    args: Vec<EvalValue>,
    permits_range_refusal: bool,
}

/// Baseline vector, then every one-parameter variation across the
/// boundary witnesses.
fn argument_vectors(kinds: &[(String, ParamKind)]) -> Vec<ArgumentCase> {
    let base: Vec<EvalValue> = kinds.iter().map(|(n, k)| baseline(k, n)).collect();
    let mut vectors = vec![ArgumentCase {
        args: base.clone(),
        permits_range_refusal: false,
    }];
    for (i, (name, kind)) in kinds.iter().enumerate() {
        for witness in boundary_witnesses(kind, name) {
            let mut varied = base.clone();
            varied[i] = witness.value;
            vectors.push(ArgumentCase {
                args: varied,
                permits_range_refusal: witness.extreme,
            });
        }
    }
    vectors
}

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
        let kinds = param_kinds(validated, &t.name);
        for (v, case) in argument_vectors(&kinds).into_iter().enumerate() {
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
                Err(
                    morpholog_core::EvalError::ArithOutOfRange(_)
                    | morpholog_core::EvalError::RoundOutOfRange { .. },
                ) if case.permits_range_refusal => {}
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
