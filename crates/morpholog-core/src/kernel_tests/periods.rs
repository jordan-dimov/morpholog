//! `periods(from, until, step)` arithmetic: the start of every step-wide
//! period, anchored at the Unix epoch, that meets `[from, until)`.
//! Aligned windows give one start per step, a window off the grid starts
//! at the period holding `from`, division is Euclidean before 1970, and
//! a start off the timeline is refused by name rather than clamped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::EvalError;
use crate::eval::period_starts;
use crate::state::EvalValue;

fn ts(s: &str) -> jiff::Timestamp {
    s.parse().unwrap()
}

fn starts(from: &str, until: &str, step: &str) -> Vec<String> {
    period_starts(ts(from), ts(until), step.parse().unwrap())
        .unwrap()
        .into_iter()
        .map(|v| match v {
            EvalValue::Timestamp(t) => t.to_string(),
            other => panic!("a period start is a timestamp, got {other:?}"),
        })
        .collect()
}

#[test]
fn an_aligned_window_gives_one_start_per_step() {
    assert_eq!(
        starts("2026-03-29T00:00:00Z", "2026-03-29T03:00:00Z", "PT1H"),
        [
            "2026-03-29T00:00:00Z",
            "2026-03-29T01:00:00Z",
            "2026-03-29T02:00:00Z"
        ]
    );
}

#[test]
fn a_window_off_the_grid_starts_at_the_period_holding_from() {
    // Two periods met, though the window is one hour long: the rule
    // that wants aligned windows tests the count against the length.
    assert_eq!(
        starts("2026-03-29T00:30:00Z", "2026-03-29T01:30:00Z", "PT1H"),
        ["2026-03-29T00:00:00Z", "2026-03-29T01:00:00Z"]
    );
}

/// Empty whether or not the edges sit on the grid: the period holding
/// `from` is not met by a window that holds nothing.
#[test]
fn an_empty_window_gives_no_periods() {
    assert!(starts("2026-03-29T01:00:00Z", "2026-03-29T01:00:00Z", "PT1H").is_empty());
    assert!(starts("2026-03-29T02:00:00Z", "2026-03-29T01:00:00Z", "PT1H").is_empty());
    assert!(starts("2026-03-29T01:30:00Z", "2026-03-29T01:30:00Z", "PT1H").is_empty());
    assert!(starts("2026-03-29T01:45:00Z", "2026-03-29T01:30:00Z", "PT1H").is_empty());
}

#[test]
fn a_local_day_is_as_many_quarter_hours_as_it_has() {
    // Berlin's bounds as the calendar authority would publish them.
    let count = |from, until| starts(from, until, "PT15M").len();
    assert_eq!(count("2026-03-28T23:00:00Z", "2026-03-29T22:00:00Z"), 92);
    assert_eq!(count("2026-07-14T22:00:00Z", "2026-07-15T22:00:00Z"), 96);
    assert_eq!(count("2026-10-24T22:00:00Z", "2026-10-25T23:00:00Z"), 100);
}

#[test]
fn before_the_epoch_the_grid_aligns_the_same_way() {
    assert_eq!(
        starts("1969-12-31T23:59:30Z", "1970-01-01T00:00:30Z", "PT1M"),
        ["1969-12-31T23:59:00Z", "1970-01-01T00:00:00Z"]
    );
}

#[test]
fn a_non_positive_step_is_refused_by_name() {
    for step in ["PT0S", "-PT1H"] {
        let err = period_starts(
            ts("2026-03-29T00:00:00Z"),
            ts("2026-03-29T01:00:00Z"),
            step.parse().unwrap(),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                EvalError::PeriodSpanNotPositive {
                    builtin: "periods",
                    ..
                }
            ),
            "{step}: {err:?}"
        );
    }
}

#[test]
fn a_start_off_the_timeline_is_refused_not_clamped() {
    // The earliest instant sits inside a 7-hour period whose start is
    // before the timeline begins.
    let until = jiff::Timestamp::MIN
        .checked_add(jiff::SignedDuration::from_hours(2))
        .unwrap();
    let err = period_starts(jiff::Timestamp::MIN, until, "PT7H".parse().unwrap()).unwrap_err();
    assert!(matches!(err, EvalError::ArithOutOfRange(_)), "{err:?}");
}
