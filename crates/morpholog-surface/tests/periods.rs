//! `periods(from, until, step)` at the surface: the grid's size is the
//! rule's own, so a call needs a gate bounding its window first; the
//! grid's elements are timestamps before anyone uses them; and the
//! lint tier hints when the bound still allows a very large grid.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::format_program;
use morpholog_core::{Lint, PreparedProgram, ValidationError, lints};
use morpholog_surface::parse_program;

const DECLS: &str = "program grid
predicate Trade(book: Subject, trade: Subject, mw: Decimal, starts_at: Timestamp, ends_at: Timestamp)
";

fn programme(body: &str) -> Result<morpholog_core::Program, Vec<ValidationError>> {
    let source = format!(
        "{DECLS}\ntransformation capture(book, trade, mw, starts_at, ends_at):\n{body}    admit Trade(book, trade, mw, starts_at, ends_at)\n"
    );
    let p = parse_program(&source).unwrap_or_else(|e| panic!("should parse: {e:?}\n{source}"));
    p.validate().map(|()| p)
}

const BOUND: &str = "    require ends_at - starts_at no_longer_than duration(PT744H)\n";
const GRID: &str = "    let hours = periods(starts_at, ends_at, duration(PT1H))\n";

#[test]
fn a_bounded_grid_validates_and_round_trips() {
    let p = programme(&format!(
        "{BOUND}{GRID}    require forall h in hours: (starts_at at_or_before h and h strictly_before ends_at)\n"
    ))
    .expect("bounded grid validates");
    assert_eq!(parse_program(&format_program(&p)).unwrap(), p);
}

/// Without a gate on the window, with the gate after the grid, or with
/// a bound the proposer controls, the grid's size is not the rule's.
#[test]
fn an_unbounded_grid_is_refused_at_validation() {
    let unbounded = [
        GRID.to_string(),
        format!("{GRID}{BOUND}"),
        format!("    require ends_at - starts_at no_longer_than mw * duration(PT1H)\n{GRID}"),
        format!("    require starts_at at_or_before ends_at\n{GRID}"),
        // A step the proposer supplies makes the count theirs, however
        // short the window.
        format!(
            "{BOUND}    let step = mw * duration(PT1H)\n    let hours = periods(starts_at, ends_at, step)\n"
        ),
    ];
    for body in unbounded {
        let errs = programme(&body).expect_err(&body);
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::PeriodsUnbounded { .. })),
            "{body}\n{errs:?}"
        );
    }
}

/// The gate is matched through the checker's cells, so a renamed edge
/// still counts; a strict bound does too, and so does the comparison
/// written the other way round.
#[test]
fn the_bound_follows_an_alias_and_accepts_the_gate_either_way_round() {
    programme(
        "    let until = ends_at\n    require until - starts_at shorter_than duration(PT744H)\n    let hours = periods(starts_at, until, duration(PT1H))\n    require sum(1 | h in hours) <= 744\n",
    )
    .expect("an aliased, strictly bounded window is bounded");
    programme(
        "    require duration(PT744H) no_shorter_than ends_at - starts_at\n    let hours = periods(starts_at, ends_at, duration(PT1H))\n    require sum(1 | h in hours) <= 745\n",
    )
    .expect("the reversed comparison bounds the window too");
}

/// A grid's elements are timestamps by construction, so a loop binder
/// over one is a timestamp before its first use, directly in `for`, or
/// through a `let` into `forall` and `sum`.
#[test]
fn a_grid_binder_is_a_timestamp_before_its_first_use() {
    let misuses = [
        format!(
            "{BOUND}    for h in periods(starts_at, ends_at, duration(PT1H)):\n        require h <= 3\n"
        ),
        format!("{BOUND}{GRID}    require forall h in hours: h <= 3\n"),
        format!("{BOUND}{GRID}    require sum(h | h in hours) <= 3\n"),
    ];
    for body in misuses {
        let errs = programme(&body).expect_err(&body);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::OperandKindMismatch { .. }
                    | ValidationError::VariableKindConflict { .. }
                    | ValidationError::NoArithRule { .. }
            )),
            "{body}\n{errs:?}"
        );
    }
}

#[test]
fn a_literal_zero_step_is_refused_at_validation() {
    let errs = programme(&format!(
        "{BOUND}    let hours = periods(starts_at, ends_at, duration(PT0S))\n"
    ))
    .expect_err("zero step");
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::PeriodSpanNotPositive {
                builtin: "periods",
                ..
            }
        )),
        "{errs:?}"
    );
}

/// A gate that still allows a very large grid is a hint, with the most
/// periods the rule can build; a month of hours is not.
#[test]
fn a_large_grid_is_a_hint() {
    let large = programme(
        "    require ends_at - starts_at no_longer_than duration(PT8760H)\n    let quarters = periods(starts_at, ends_at, duration(PT15M))\n    require sum(1 | q in quarters) <= 35041\n",
    )
    .unwrap();
    let found = lints(&PreparedProgram::new(large).unwrap());
    assert!(
        found.iter().any(|l| matches!(
            l,
            Lint::LargeGrid {
                transformation,
                periods: 35041,
                ..
            } if transformation == "capture"
        )),
        "{found:?}"
    );
    let month = programme(&format!(
        "{BOUND}{GRID}    require sum(1 | h in hours) <= 745\n"
    ))
    .unwrap();
    assert!(
        lints(&PreparedProgram::new(month).unwrap())
            .iter()
            .all(|l| !matches!(l, Lint::LargeGrid { .. }))
    );
}
