//! Integration tests for the delivery-hour limits example
//! (`examples/23_delivery_hour_limits/`).
//!
//! The cap holds in every hour a trade touches, with the hours computed
//! from the trade: the sum-of-overlaps shortcut would refuse a trade that
//! fits, a misaligned window is refused before the limit, the cap binds
//! both ways, and a day product's window is the published day's, 23 or 25
//! hours long, with no zone arithmetic anywhere in the rules.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, date, dec_str, subj, test_actor, ts};
use morpholog_core::{
    Bounding, Compared, EvalError, Impact, ImpactPlan, Outcome, RejectionReason, State,
};
use morpholog_examples::delivery_hour_limits;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&delivery_hour_limits::program()))
}

const BOOK: &str = "book_a";
const BERLIN: &str = "berlin";
const PUBLISHER: &str = "calendar_desk";

/// A book capped at ten megawatts.
fn capped() -> State {
    State::from_claims(vec![claim_instance("Limit", &[subj(BOOK), dec_str("10")])])
}

fn capture(
    trade: &str,
    mw: &str,
    from: &str,
    until: &str,
    state: &State,
) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &delivery_hour_limits::capture(),
        vec![subj(BOOK), subj(trade), dec_str(mw), ts(from), ts(until)],
        test_actor(),
        state,
    )
}

fn captured(trade: &str, mw: &str, from: &str, until: &str, state: State) -> State {
    ex().must_accept(
        &delivery_hour_limits::capture(),
        vec![subj(BOOK), subj(trade), dec_str(mw), ts(from), ts(until)],
        state,
    )
}

fn gate_refusal(outcome: &Result<Outcome, EvalError>, gate: &str) -> Option<RejectionReason> {
    match outcome {
        Ok(Outcome::Rejected {
            reason: reason @ RejectionReason::Require { name, .. },
        }) if name.as_ref().is_some_and(|n| n.as_str() == gate) => Some(reason.clone()),
        _ => None,
    }
}

/// The issue's own cases. A and B fill two different hours to eight;
/// C spans both at one, so each hour holds nine; D asks for two more
/// in the second hour, and eleven does not fit.
#[test]
fn the_cap_holds_in_every_hour_not_over_the_overlap() {
    let state = captured(
        "a",
        "8",
        "2026-03-29T00:00:00Z",
        "2026-03-29T01:00:00Z",
        capped(),
    );
    let state = captured(
        "b",
        "8",
        "2026-03-29T01:00:00Z",
        "2026-03-29T02:00:00Z",
        state,
    );
    // Overlaps both A and B: summing the overlaps gives seventeen, and the
    // shortcut would refuse this trade that fits in every hour.
    let state = captured(
        "c",
        "1",
        "2026-03-29T00:00:00Z",
        "2026-03-29T02:00:00Z",
        state,
    );
    let d = capture(
        "d",
        "2",
        "2026-03-29T01:00:00Z",
        "2026-03-29T02:00:00Z",
        &state,
    );
    let RejectionReason::Require {
        witness, compared, ..
    } = gate_refusal(&d, "hour_within_limit").unwrap_or_else(|| panic!("{d:?}"))
    else {
        unreachable!()
    };
    assert_eq!(
        compared,
        Some(Compared {
            op: "<=".to_string(),
            left: dec_str("11"),
            right: dec_str("10"),
        }),
        "the refusal reads 11 <= 10"
    );
    assert!(
        witness
            .iter()
            .any(|w| w.var.as_str() == "h" && w.value == ts("2026-03-29T01:00:00Z")),
        "the witness names the hour: {witness:?}"
    );
}

/// A window starting at half past touches two hours while being one
/// long, so it is refused as a window before any position is summed.
#[test]
fn a_window_off_the_hour_grid_is_refused_before_the_limit() {
    let state = captured(
        "a",
        "8",
        "2026-03-29T00:00:00Z",
        "2026-03-29T01:00:00Z",
        capped(),
    );
    let outcome = capture(
        "x",
        "8",
        "2026-03-29T00:30:00Z",
        "2026-03-29T01:30:00Z",
        &state,
    );
    assert!(
        gate_refusal(&outcome, "whole_hours").is_some(),
        "a misaligned window is a refused window, not a wrongly summed one: {outcome:?}"
    );
}

/// Sold megawatts are negative, and the cap binds the short side too.
#[test]
fn the_cap_binds_both_ways() {
    let outcome = capture(
        "s",
        "-11",
        "2026-03-29T00:00:00Z",
        "2026-03-29T01:00:00Z",
        &capped(),
    );
    assert!(
        gate_refusal(&outcome, "hour_within_limit").is_some(),
        "{outcome:?}"
    );
}

/// The window is bounded before the grid is built, so a window of years
/// is a business refusal, never a grid.
#[test]
fn a_window_longer_than_a_month_is_refused_before_any_grid() {
    let outcome = capture(
        "y",
        "1",
        "2026-01-01T00:00:00Z",
        "2026-03-01T00:00:00Z",
        &capped(),
    );
    assert!(
        matches!(
            outcome,
            Ok(Outcome::Rejected {
                reason: RejectionReason::Require { .. }
            })
        ),
        "{outcome:?}"
    );
}

/// Berlin's spring-forward and fall-back days of 2026, as the publisher
/// would state them: 23 and 25 hours of UTC.
fn with_published_days() -> State {
    let mut claims = capped().claims().to_vec();
    claims.push(claim_instance(
        "CalendarPublisher",
        &[subj(BERLIN), subj(PUBLISHER)],
    ));
    let state = State::from_claims(claims);
    let publish = delivery_hour_limits::publish_delivery_day();
    let state = ex().must_accept_as(
        &publish,
        vec![
            subj(BERLIN),
            date("2026-03-29"),
            ts("2026-03-28T23:00:00Z"),
            ts("2026-03-29T22:00:00Z"),
            subj("tzdb_2026a"),
        ],
        PUBLISHER,
        state,
    );
    ex().must_accept_as(
        &publish,
        vec![
            subj(BERLIN),
            date("2026-10-25"),
            ts("2026-10-24T22:00:00Z"),
            ts("2026-10-25T23:00:00Z"),
            subj("tzdb_2026a"),
        ],
        PUBLISHER,
        state,
    )
}

fn capture_day(
    trade: &str,
    mw: &str,
    local_date: &str,
    state: &State,
) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &delivery_hour_limits::capture_day(),
        vec![
            subj(BOOK),
            subj(trade),
            dec_str(mw),
            subj(BERLIN),
            date(local_date),
        ],
        test_actor(),
        state,
    )
}

/// The day product's grid is the published day's: the hour after a
/// 23-hour day is not in it, and the 25th hour of a fall-back day is.
#[test]
fn a_day_product_runs_over_the_hours_the_published_day_has() {
    // An hourly trade in the hour AFTER the short day ends: not in the
    // day's grid, so the day product still fits.
    let after = captured(
        "after",
        "3",
        "2026-03-29T22:00:00Z",
        "2026-03-29T23:00:00Z",
        with_published_days(),
    );
    let short_day = capture_day("short", "8", "2026-03-29", &after);
    assert!(
        matches!(short_day, Ok(Outcome::Accepted { .. })),
        "{short_day:?}"
    );

    // An hourly trade in the 25th hour of the long day: in the grid, so
    // eight more would make eleven there.
    let last_hour = captured(
        "last",
        "3",
        "2026-10-25T22:00:00Z",
        "2026-10-25T23:00:00Z",
        with_published_days(),
    );
    let long_day = capture_day("long", "8", "2026-10-25", &last_hour);
    let RejectionReason::Require { witness, .. } =
        gate_refusal(&long_day, "hour_within_limit").unwrap_or_else(|| panic!("{long_day:?}"))
    else {
        unreachable!()
    };
    assert!(
        witness
            .iter()
            .any(|w| w.var.as_str() == "h" && w.value == ts("2026-10-25T22:00:00Z")),
        "the 25th hour is the one named: {witness:?}"
    );
}

#[test]
fn an_unpublished_day_cannot_be_traded() {
    let outcome = capture_day("none", "1", "2026-07-01", &with_published_days());
    assert!(
        matches!(
            outcome,
            Ok(Outcome::Rejected {
                reason: RejectionReason::BindNone { .. }
            })
        ),
        "{outcome:?}"
    );
}

#[test]
fn only_the_appointed_publisher_publishes_days() {
    let outcome = ex().propose_as(
        &delivery_hour_limits::publish_delivery_day(),
        vec![
            subj(BERLIN),
            date("2026-07-01"),
            ts("2026-06-30T22:00:00Z"),
            ts("2026-07-01T22:00:00Z"),
            subj("tzdb_2026a"),
        ],
        "stranger",
        &with_published_days(),
    );
    assert!(
        matches!(
            outcome,
            Ok(Outcome::Rejected {
                reason: RejectionReason::Require { .. }
            })
        ),
        "{outcome:?}"
    );
}

/// Exactly 23, 24 or 25 hours: a 22-hour day and a 23-and-a-half-hour
/// day are both publishing mistakes.
#[test]
fn a_published_day_must_be_a_civil_day() {
    for ends_at in ["2026-07-01T20:00:00Z", "2026-07-01T21:30:00Z"] {
        let outcome = ex().propose_as(
            &delivery_hour_limits::publish_delivery_day(),
            vec![
                subj(BERLIN),
                date("2026-07-01"),
                ts("2026-06-30T22:00:00Z"),
                ts(ends_at),
                subj("tzdb_2026a"),
            ],
            PUBLISHER,
            &with_published_days(),
        );
        assert!(
            matches!(
                &outcome,
                Ok(Outcome::Rejected {
                    reason: RejectionReason::Invariant { name, .. }
                }) if name.as_str() == "a_delivery_day_is_a_civil_day"
            ),
            "a day ending {ends_at} is refused by name: {outcome:?}"
        );
    }
}

/// Each hour's start between two instants, as the grid would build it.
fn hours_from(from: &str, until: &str) -> Vec<morpholog_core::EvalValue> {
    let (from, until): (jiff::Timestamp, jiff::Timestamp) =
        (from.parse().unwrap(), until.parse().unwrap());
    let mut out = Vec::new();
    let mut h = from;
    while h < until {
        out.push(morpholog_core::EvalValue::Timestamp(h));
        h = h.checked_add(jiff::SignedDuration::from_hours(1)).unwrap();
    }
    out
}

fn price_hour(local_date: &str, hour: morpholog_core::EvalValue, state: State) -> State {
    ex().must_accept_as(
        &delivery_hour_limits::price_hour(),
        vec![subj(BERLIN), date(local_date), hour, dec_str("42.5")],
        PUBLISHER,
        state,
    )
}

fn publish_prices(local_date: &str, state: &State) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &delivery_hour_limits::publish_prices(),
        vec![subj(BERLIN), date(local_date)],
        PUBLISHER,
        state,
    )
}

fn rule_refusal(outcome: &Result<Outcome, EvalError>, rule: &str) -> Option<RejectionReason> {
    match outcome {
        Ok(Outcome::Rejected {
            reason: reason @ RejectionReason::Invariant { name, .. },
        }) if name.as_str() == rule => Some(reason.clone()),
        _ => None,
    }
}

/// The 23-hour day with its hours priced, all but the last if `short`.
fn priced_spring_day(short: bool) -> State {
    let mut hours = hours_from("2026-03-28T23:00:00Z", "2026-03-29T22:00:00Z");
    assert_eq!(hours.len(), 23);
    if short {
        hours.pop();
    }
    hours.into_iter().fold(with_published_days(), |state, h| {
        price_hour("2026-03-29", h, state)
    })
}

/// A published day has a price for every hour it has: 23 on the
/// spring day, 25 on the fall day. One hour short, the rule refuses and
/// names the hour.
#[test]
fn a_published_day_has_a_price_for_every_hour_it_has() {
    let full = priced_spring_day(false);
    assert!(matches!(
        publish_prices("2026-03-29", &full),
        Ok(Outcome::Accepted { .. })
    ));
    let short = priced_spring_day(true);
    let refused = publish_prices("2026-03-29", &short);
    let reason = rule_refusal(&refused, "a_published_day_is_priced_hour_by_hour")
        .unwrap_or_else(|| panic!("the unpriced hour refuses the day: {refused:?}"));
    let RejectionReason::Invariant { witness, .. } = reason else {
        unreachable!()
    };
    assert!(
        witness
            .iter()
            .any(|w| w.var.as_str() == "h" && w.value == ts("2026-03-29T21:00:00Z")),
        "the witness names the missing hour: {witness:?}"
    );
    // The fall day has 25 hours, and needs all of them.
    let hours = hours_from("2026-10-24T22:00:00Z", "2026-10-25T23:00:00Z");
    assert_eq!(hours.len(), 25);
    let mut state = full;
    for h in &hours[..24] {
        state = price_hour("2026-10-25", h.clone(), state);
    }
    assert!(
        rule_refusal(
            &publish_prices("2026-10-25", &state),
            "a_published_day_is_priced_hour_by_hour"
        )
        .is_some()
    );
    let state = price_hour("2026-10-25", hours[24].clone(), state);
    assert!(matches!(
        publish_prices("2026-10-25", &state),
        Ok(Outcome::Accepted { .. })
    ));
}

/// Exactly one price per hour is two rules composed, and a price is for
/// an hour inside its day. Taking the only price for an hour away from
/// a published day breaks the grid rule.
#[test]
fn prices_are_one_per_hour_inside_the_day_and_kept_once_published() {
    let state = priced_spring_day(false);
    let again = ex().propose_as(
        &delivery_hour_limits::price_hour(),
        vec![
            subj(BERLIN),
            date("2026-03-29"),
            ts("2026-03-29T10:00:00Z"),
            dec_str("1"),
        ],
        PUBLISHER,
        &state,
    );
    assert!(
        rule_refusal(&again, "price_unique_by_calendar_local_date_hour").is_some(),
        "{again:?}"
    );
    let outside = ex().propose_as(
        &delivery_hour_limits::price_hour(),
        vec![
            subj(BERLIN),
            date("2026-03-29"),
            ts("2026-03-29T22:00:00Z"),
            dec_str("1"),
        ],
        PUBLISHER,
        &state,
    );
    assert!(
        rule_refusal(&outside, "a_price_falls_in_its_day").is_some(),
        "{outside:?}"
    );
    let published = ex().must_accept_as(
        &delivery_hour_limits::publish_prices(),
        vec![subj(BERLIN), date("2026-03-29")],
        PUBLISHER,
        state,
    );
    let withdrawn = ex().propose_as(
        &delivery_hour_limits::withdraw_price(),
        vec![subj(BERLIN), date("2026-03-29"), ts("2026-03-29T10:00:00Z")],
        PUBLISHER,
        &published,
    );
    assert!(
        rule_refusal(&withdrawn, "a_published_day_is_priced_hour_by_hour").is_some(),
        "{withdrawn:?}"
    );
}

/// The grid rule's bound stops a day longer than its bound; it does not
/// stop an empty or reversed day, which would pass vacuously. The
/// civil-day rule does, and the grid rule leans on it.
#[test]
fn an_empty_or_reversed_day_is_refused_by_the_civil_day_rule_not_the_grid() {
    let mut claims = capped().claims().to_vec();
    claims.push(claim_instance(
        "CalendarPublisher",
        &[subj(BERLIN), subj(PUBLISHER)],
    ));
    let state = State::from_claims(claims);
    for (name, from, until) in [
        ("empty", "2026-03-29T00:00:00Z", "2026-03-29T00:00:00Z"),
        ("reversed", "2026-03-30T00:00:00Z", "2026-03-29T00:00:00Z"),
    ] {
        let outcome = ex().propose_as(
            &delivery_hour_limits::publish_delivery_day(),
            vec![
                subj(BERLIN),
                date("2026-03-29"),
                ts(from),
                ts(until),
                subj("tzdb_2026a"),
            ],
            PUBLISHER,
            &state,
        );
        assert!(
            rule_refusal(&outcome, "a_delivery_day_is_a_civil_day").is_some(),
            "{name}: {outcome:?}"
        );
    }
}

/// The grid rule is judged per day: a price on one day touches that
/// day's case only, and a day's bounds touch that day's case.
#[test]
fn the_grid_rule_is_bounded_to_its_day() {
    let program = delivery_hour_limits::program();
    let rule = program
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "a_published_day_is_priced_hour_by_hour")
        .unwrap();
    let plan = ImpactPlan::with_definitions(rule, &program.definitions);
    assert!(
        matches!(plan.bounding(), Bounding::Bound),
        "{:?}",
        plan.bounding()
    );
    let case = |delta: &[morpholog_core::ClaimInstance]| match plan.classify(delta, &[]) {
        Impact::Bounded(cases) => cases
            .iter()
            .map(|c| c[&"local_date".into()].clone())
            .collect::<Vec<_>>(),
        other => panic!("bounded to a day: {other:?}"),
    };
    assert_eq!(
        case(&[claim_instance(
            "Price",
            &[
                subj(BERLIN),
                date("2026-10-25"),
                ts("2026-10-25T10:00:00Z"),
                dec_str("1")
            ]
        )]),
        vec![date("2026-10-25")]
    );
    assert_eq!(
        case(&[claim_instance(
            "DeliveryDay",
            &[
                subj(BERLIN),
                date("2026-03-29"),
                ts("2026-03-28T23:00:00Z"),
                ts("2026-03-29T22:00:00Z"),
                subj("tzdb_2026b")
            ]
        )]),
        vec![date("2026-03-29")]
    );
}
