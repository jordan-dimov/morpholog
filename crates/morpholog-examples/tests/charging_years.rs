//! Charging years: a billing period may not straddle the 1 April
//! anniversary, and a run may only price from a published year. The
//! gates refuse a straddling period and an unpublished year at the
//! act, the invariants refuse both against any other act, the
//! recorded year is the record's own computation, and the year's
//! number and its sheet's start date provably name the same period.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, dec, subj};
use morpholog_core::{EvalValue, RejectionReason, State};
use morpholog_examples::charging_years;
use morpholog_test_support::date;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&charging_years::program()))
}

fn rejected_by(reason: &RejectionReason, rule: &str) -> bool {
    matches!(reason, RejectionReason::Invariant { name, .. } if name.as_str() == rule)
}

/// A state with the named years' rate sheets on the record, each
/// admitted through the guarded publication act.
fn published(year_starts: &[&str]) -> State {
    year_starts.iter().fold(State::default(), |state, d| {
        ex().must_accept(&charging_years::publish_rates(), vec![date(d)], state)
    })
}

#[test]
fn a_run_inside_one_charging_year_commits_with_its_computed_year() {
    // 2026-04-10 .. 2026-07-09: inside the charging year opening
    // 1 April 2026; the record carries the year's own name.
    let after = ex().must_accept(
        &charging_years::open_run(),
        vec![subj("r1"), date("2026-04-10"), date("2026-07-09")],
        published(&["2026-04-01"]),
    );
    let run = after
        .claims_for("BillingRun")
        .next()
        .expect("the run committed")
        .clone();
    assert_eq!(run.args[3], EvalValue::Decimal(2026.into()));
}

#[test]
fn a_period_straddling_the_first_of_april_is_refused_at_the_gate() {
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![subj("r1"), date("2026-03-15"), date("2026-04-15")],
        &published(&["2025-04-01", "2026-04-01"]),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_period_stays_inside_one_charging_year"),
        "{reason:?}"
    );
}

#[test]
fn a_sheet_whose_date_is_not_an_anniversary_is_refused_at_the_gate() {
    // 3 April only claims to open a year: recomputing year 26's first
    // day gives 1 April 2026 back, so the round-trip gate refuses.
    let reason = ex().must_reject(
        &charging_years::publish_rates(),
        vec![date("2026-04-03")],
        &State::default(),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_date_is_an_anniversary"),
        "{reason:?}"
    );
}

#[test]
fn a_run_against_an_unpublished_year_is_refused_at_the_gate() {
    // The period is lawful, but 2026's sheet is not on the record -
    // only 2025's is.
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![subj("r1"), date("2026-04-10"), date("2026-07-09")],
        &published(&["2025-04-01"]),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_years_rates_are_published"),
        "{reason:?}"
    );
}

/// The bare admission act: the shape a path that skipped the gates
/// would take. The invariants, not the gates, make the straddle, the
/// wrong year, and the unpublished year uncommittable.
fn bare_run() -> morpholog_core::Transformation {
    use morpholog_core::ir_builder::{assert_, params, transformation, var};
    transformation(
        "bare_run",
        params(&["run", "starts_on", "ends_on", "year"]),
        vec![assert_(
            "BillingRun",
            vec![var("run"), var("starts_on"), var("ends_on"), var("year")],
        )],
    )
}

#[test]
fn a_straddling_run_is_refused_by_the_invariant_against_any_act() {
    let reason = ex().must_reject(
        &bare_run(),
        vec![
            subj("r1"),
            date("2026-03-15"),
            date("2026-04-15"),
            dec(2025),
        ],
        &State::default(),
    );
    assert!(
        rejected_by(&reason, "runs_stay_inside_one_charging_year"),
        "{reason:?}"
    );
}

#[test]
fn a_wrong_recorded_year_is_refused_by_name() {
    // The period is lawful; the stated coordinate is off by one.
    let reason = ex().must_reject(
        &bare_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            dec(2025),
        ],
        &published(&["2025-04-01", "2026-04-01"]),
    );
    assert!(
        rejected_by(&reason, "runs_record_their_own_charging_year"),
        "{reason:?}"
    );
}

#[test]
fn a_run_naming_an_unpublished_year_is_refused_by_the_invariant() {
    // The coordinate is the record's own recompute and the period is
    // lawful - but no sheet starting 1 April 2026 exists, so the
    // number names a year the record cannot price from. The invariant
    // reaches the sheet by computing the year's first day back from
    // the recorded number.
    let reason = ex().must_reject(
        &bare_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            dec(2026),
        ],
        &State::default(),
    );
    assert!(
        rejected_by(&reason, "runs_price_from_a_published_year"),
        "{reason:?}"
    );
    // And the acceptance companion: with the sheet on the record the
    // same bare admission commits - the invariant asks for exactly
    // the published anniversary, nothing more.
    ex().must_accept(
        &bare_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            dec(2026),
        ],
        published(&["2026-04-01"]),
    );
}

#[test]
fn the_boundary_day_itself_opens_the_new_year() {
    // A run ending 31 March stays in the old year; a run starting
    // 1 April opens the new one - and a run from 31 March to 1 April
    // straddles.
    ex().must_accept(
        &charging_years::open_run(),
        vec![subj("r1"), date("2025-04-01"), date("2026-03-31")],
        published(&["2025-04-01"]),
    );
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![subj("r2"), date("2026-03-31"), date("2026-04-01")],
        &published(&["2025-04-01", "2026-04-01"]),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_period_stays_inside_one_charging_year"),
        "{reason:?}"
    );
}

#[test]
fn a_backwards_period_is_refused_before_the_year_rule() {
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![subj("r1"), date("2026-07-09"), date("2026-04-10")],
        &State::default(),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_period_runs_forwards"),
        "{reason:?}"
    );
}
