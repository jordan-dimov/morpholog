//! Generated cases over the civil calendar, through the evaluator: a
//! calendar span added to or subtracted from a date, and the two period
//! builtins. Only the laws the semantics promise are asserted. Month
//! shifts clamp, so they are neither reversible nor associative in
//! general (`date_arithmetic.rs` pins the traps); the properties here say
//! where they are, and that a result off the calendar is refused by name
//! rather than wrapped, clamped or panicked.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use jiff::civil::Date;
use proptest::prelude::*;
use rust_decimal::Decimal;

use crate::ir_builder::{
    add, assert_, date, period_index, period_start_of, span, sub, term, transformation, var,
};
use crate::ir_builder::{let_, params};
use crate::kernel_tests::support::propose_with_test_actor;
use crate::{EvalError, EvalValue, Outcome, State, ValueExpr};

/// The value `expr` evaluates to, read back from the claim it admits.
fn evaluate(expr: ValueExpr) -> Result<EvalValue, EvalError> {
    let t = transformation(
        "probe",
        params(&[]),
        vec![let_("r", expr), assert_("Result", vec![var("r")])],
    );
    match propose_with_test_actor(&t, vec![], &State::default(), &[], &[])? {
        Outcome::Accepted {
            asserted_claims, ..
        } => Ok(asserted_claims[0].args[0].clone()),
        other @ Outcome::Rejected { .. } => {
            panic!("a probe with no rules cannot be refused: {other:?}")
        }
    }
}

fn the_date(value: EvalValue) -> Date {
    match value {
        EvalValue::Date(d) => d,
        other => panic!("expected a date, got {other:?}"),
    }
}

fn spelled(months: u32, days: u32) -> String {
    match (months, days) {
        (0, 0) => "P0D".to_string(),
        (0, d) => format!("P{d}D"),
        (m, 0) => format!("P{m}M"),
        (m, d) => format!("P{m}M{d}D"),
    }
}

/// `d + span`, or the error that refused it.
fn shift(d: Date, months: u32, days: u32) -> Result<Date, EvalError> {
    evaluate(add(
        term(date(&d.to_string())),
        term(span(&spelled(months, days))),
    ))
    .map(the_date)
}

fn unshift(d: Date, months: u32, days: u32) -> Result<Date, EvalError> {
    evaluate(sub(
        term(date(&d.to_string())),
        term(span(&spelled(months, days))),
    ))
    .map(the_date)
}

fn index_of(anchor: Date, months: u32, days: u32, at: Date) -> i64 {
    match evaluate(period_index(
        term(date(&anchor.to_string())),
        term(span(&spelled(months, days))),
        term(date(&at.to_string())),
    ))
    .expect("period_index is total over representable dates")
    {
        EvalValue::Decimal(n) => i64::try_from(n).expect("a whole index"),
        other => panic!("expected an index, got {other:?}"),
    }
}

fn start_of(anchor: Date, months: u32, days: u32, n: i64) -> Result<Date, EvalError> {
    evaluate(period_start_of(
        term(date(&anchor.to_string())),
        term(span(&spelled(months, days))),
        term(crate::ir_builder::dec(&Decimal::from(n).to_string())),
    ))
    .map(the_date)
}

fn left_the_calendar(err: &EvalError) -> bool {
    matches!(err, EvalError::ArithOutOfRange(_))
}

/// Dates over the whole calendar, weighted toward the places clamping
/// lives: month ends, February, leap years, the calendar's two ends.
fn civil_date() -> impl Strategy<Value = Date> {
    let year = prop_oneof![
        3 => 1990i16..=2035,
        1 => prop::sample::select(vec![-9999i16, -9998, 1900, 2000, 2024, 2100, 9998, 9999]),
        1 => -9999i16..=9999,
    ];
    let day = prop_oneof![2 => 28i8..=31, 1 => 1i8..=31];
    (year, 1i8..=12, day).prop_map(|(y, m, d)| {
        let last = Date::new(y, m, 1).unwrap().days_in_month();
        Date::new(y, m, d.min(last)).unwrap()
    })
}

/// The month a shift of `months` lands in, by plain month counting.
fn landing_month(d: Date, months: i64) -> Option<(i16, i8)> {
    let total = i64::from(d.year()) * 12 + i64::from(d.month()) - 1 + months;
    let year = i16::try_from(total.div_euclid(12)).ok()?;
    Some((year, i8::try_from(total.rem_euclid(12) + 1).ok()?))
}

proptest! {
    #[test]
    fn a_zero_span_leaves_a_date_where_it_is(d in civil_date()) {
        prop_assert_eq!(shift(d, 0, 0).unwrap(), d);
        prop_assert_eq!(unshift(d, 0, 0).unwrap(), d);
    }

    #[test]
    fn a_day_only_shift_is_undone_by_its_negation(d in civil_date(), days in 1u32..=800) {
        let lands = d.checked_add(jiff::Span::new().days(i64::from(days))).is_ok();
        match shift(d, 0, days) {
            Ok(later) => prop_assert_eq!(unshift(later, 0, days).unwrap(), d),
            Err(e) => prop_assert!(!lands && left_the_calendar(&e), "{e}"),
        }
    }

    #[test]
    fn a_month_shift_keeps_the_day_or_lands_on_the_month_end(
        d in civil_date(),
        months in 1u32..=240,
        forward in any::<bool>(),
    ) {
        let signed = if forward { i64::from(months) } else { -i64::from(months) };
        let got = if forward { shift(d, months, 0) } else { unshift(d, months, 0) };
        let target = landing_month(d, signed).filter(|(y, _)| (-9999..=9999).contains(y));
        match (got, target) {
            (Ok(landed), Some((y, m))) => {
                prop_assert_eq!((landed.year(), landed.month()), (y, m));
                let last = Date::new(y, m, 1).unwrap().days_in_month();
                prop_assert_eq!(landed.day(), d.day().min(last));
            }
            (Err(e), None) => prop_assert!(left_the_calendar(&e), "{e}"),
            (got, target) => prop_assert!(false, "{got:?} for a landing month {target:?}"),
        }
    }

    // Clamping is the only source of drift: a date on or before the 28th
    // never clamps, so for it two month shifts are one.
    #[test]
    fn month_shifts_compose_when_nothing_clamps(
        d in civil_date().prop_filter("no clamping below the 29th", |d| d.day() <= 28),
        a in 0u32..=60,
        b in 0u32..=60,
    ) {
        if let (Ok(stepped), Ok(direct)) = (shift(d, a, 0), shift(d, a + b, 0)) {
            prop_assert_eq!(shift(stepped, b, 0).unwrap(), direct);
        }
    }

    #[test]
    fn a_period_start_is_in_the_period_it_starts(
        anchor in civil_date(),
        months in 0u32..=24,
        days in 0u32..=40,
        n in -200i64..=200,
    ) {
        prop_assume!(months > 0 || days > 0);
        match start_of(anchor, months, days, n) {
            Ok(start) => prop_assert_eq!(index_of(anchor, months, days, start), n),
            Err(e) => prop_assert!(left_the_calendar(&e), "{e}"),
        }
    }

    // Periods are half-open, [start(n), start(n + 1)): a date belongs to
    // the period whose start is on or before it, and the next start is
    // after it, wherever those starts are on the calendar.
    #[test]
    fn every_date_sits_inside_the_period_its_index_names(
        anchor in civil_date(),
        months in 0u32..=24,
        days in 0u32..=40,
        at in civil_date(),
    ) {
        prop_assume!(months > 0 || days > 0);
        let n = index_of(anchor, months, days, at);
        if let Ok(start) = start_of(anchor, months, days, n) {
            prop_assert!(start <= at, "{start} is after {at}");
        }
        if let Ok(next) = start_of(anchor, months, days, n + 1) {
            prop_assert!(at < next, "{at} is not before {next}");
        }
    }

    #[test]
    fn a_shift_off_the_calendar_is_refused_by_name(
        d in civil_date(),
        months in 0u32..=300_000,
        days in 0u32..=4_000_000,
        forward in any::<bool>(),
    ) {
        let got = if forward { shift(d, months, days) } else { unshift(d, months, days) };
        if let Err(e) = got {
            prop_assert!(left_the_calendar(&e), "{e}");
        }
    }
}
