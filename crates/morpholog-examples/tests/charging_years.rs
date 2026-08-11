//! Charging years: a billing period may not straddle the 1 April
//! anniversary, and a run must price from its own year's rate sheet.
//! The gates refuse a straddling period and a wrong or unknown sheet
//! at the act, the invariants refuse both against any other act, the
//! recorded year is the record's own computation, and the sheet a run
//! names must start on the day its recorded year begins - the wrong
//! file choice is uncommittable.

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

/// A state with the named sheets on the record, each admitted through
/// the guarded publication act.
fn published(sheets: &[(&str, &str)]) -> State {
    sheets.iter().fold(State::default(), |state, (sheet, d)| {
        ex().must_accept(
            &charging_years::publish_rates(),
            vec![subj(sheet), date(d)],
            state,
        )
    })
}

#[test]
fn a_run_naming_its_years_sheet_commits_with_its_computed_year() {
    // 2026-04-10 .. 2026-07-09: inside the charging year opening
    // 1 April 2026, priced from that year's sheet; the record carries
    // the year's own name and the sheet it used.
    let after = ex().must_accept(
        &charging_years::open_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            subj("sheet_2026"),
        ],
        published(&[("sheet_2025", "2025-04-01"), ("sheet_2026", "2026-04-01")]),
    );
    let run = after
        .claims_for("BillingRun")
        .next()
        .expect("the run committed")
        .clone();
    assert_eq!(run.args[3], EvalValue::Decimal(2026.into()));
    assert_eq!(run.args[4], subj("sheet_2026"));
}

#[test]
fn a_run_that_read_last_years_file_is_refused_at_the_gate() {
    // The counterexample the example exists to kill: 2026's sheet IS
    // published, the period is perfectly ordinary - but the engine
    // loaded the 2025 file and says so. The wrong file choice is
    // refused by name, not left in the loader's head.
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            subj("sheet_2025"),
        ],
        &published(&[("sheet_2025", "2025-04-01"), ("sheet_2026", "2026-04-01")]),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_sheet_is_the_years_sheet"),
        "{reason:?}"
    );
}

#[test]
fn a_run_naming_an_unknown_sheet_is_refused_at_the_gate() {
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            subj("no_such_sheet"),
        ],
        &published(&[("sheet_2026", "2026-04-01")]),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_sheet_is_the_years_sheet"),
        "{reason:?}"
    );
}

#[test]
fn a_period_straddling_the_first_of_april_is_refused_at_the_gate() {
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![
            subj("r1"),
            date("2026-03-15"),
            date("2026-04-15"),
            subj("sheet_2026"),
        ],
        &published(&[("sheet_2025", "2025-04-01"), ("sheet_2026", "2026-04-01")]),
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
        vec![subj("sheet_x"), date("2026-04-03")],
        &State::default(),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_date_is_an_anniversary"),
        "{reason:?}"
    );
}

/// The bare admission act: the shape a path that skipped the gates
/// would take. The invariants, not the gates, make the straddle, the
/// wrong year, and the wrong sheet uncommittable.
fn bare_run() -> morpholog_core::Transformation {
    use morpholog_core::ir_builder::{assert_, params, transformation, var};
    transformation(
        "bare_run",
        params(&["run", "starts_on", "ends_on", "year", "rate_sheet"]),
        vec![assert_(
            "BillingRun",
            vec![
                var("run"),
                var("starts_on"),
                var("ends_on"),
                var("year"),
                var("rate_sheet"),
            ],
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
            subj("sheet_2025"),
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
            subj("sheet_2025"),
        ],
        &published(&[("sheet_2025", "2025-04-01"), ("sheet_2026", "2026-04-01")]),
    );
    assert!(
        rejected_by(&reason, "runs_record_their_own_charging_year"),
        "{reason:?}"
    );
}

#[test]
fn a_run_recording_the_wrong_sheet_is_refused_by_the_invariant() {
    // Year and period agree, both sheets are published - but the run
    // names LAST year's sheet, and the invariant joins the recorded
    // sheet to the year's recomputed first day.
    let reason = ex().must_reject(
        &bare_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            dec(2026),
            subj("sheet_2025"),
        ],
        &published(&[("sheet_2025", "2025-04-01"), ("sheet_2026", "2026-04-01")]),
    );
    assert!(
        rejected_by(&reason, "runs_price_from_their_years_sheet"),
        "{reason:?}"
    );
    // And the acceptance companion: the same bare admission naming
    // its own year's sheet commits - the invariant asks for exactly
    // the year-to-sheet join, nothing more.
    ex().must_accept(
        &bare_run(),
        vec![
            subj("r1"),
            date("2026-04-10"),
            date("2026-07-09"),
            dec(2026),
            subj("sheet_2026"),
        ],
        published(&[("sheet_2025", "2025-04-01"), ("sheet_2026", "2026-04-01")]),
    );
}

#[test]
fn the_boundary_day_itself_opens_the_new_year() {
    // A run ending 31 March stays in the old year; a run starting
    // 1 April opens the new one - and a run from 31 March to 1 April
    // straddles.
    ex().must_accept(
        &charging_years::open_run(),
        vec![
            subj("r1"),
            date("2025-04-01"),
            date("2026-03-31"),
            subj("sheet_2025"),
        ],
        published(&[("sheet_2025", "2025-04-01")]),
    );
    let reason = ex().must_reject(
        &charging_years::open_run(),
        vec![
            subj("r2"),
            date("2026-03-31"),
            date("2026-04-01"),
            subj("sheet_2026"),
        ],
        &published(&[("sheet_2025", "2025-04-01"), ("sheet_2026", "2026-04-01")]),
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
        vec![
            subj("r1"),
            date("2026-07-09"),
            date("2026-04-10"),
            subj("sheet_2026"),
        ],
        &State::default(),
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_period_runs_forwards"),
        "{reason:?}"
    );
}
