//! Integration tests for the borrowing-base example
//! (`examples/11_borrowing_base/`).
//!
//! Exercises the new decimal arithmetic end to end: the advance-limit
//! invariant multiplies (`drawn <= advance_rate * collateral`), and the
//! `FacilityUtilisation` derived claim divides (`drawn / collateral`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, dec, dec_str, has_claim, must_accept, propose_with_test_actor, subj};
use morpholog_core::{Definition, Invariant, Outcome, State, enumerate_derived};
use morpholog_examples::borrowing_base;

fn invariants() -> Vec<Invariant> {
    borrowing_base::all_invariants()
}

fn definitions() -> Vec<Definition> {
    borrowing_base::definitions()
}

// Facility f1 at an 80% advance rate, with `collateral` of pledged value.
fn facility_with_collateral(collateral: i64) -> State {
    let state = must_accept(
        &borrowing_base::open_facility(),
        vec![subj("f1"), dec_str("0.8")],
        State::default(),
        &invariants(),
        &definitions(),
    );
    must_accept(
        &borrowing_base::pledge_collateral(),
        vec![subj("f1"), subj("asset_1"), dec(collateral)],
        state,
        &invariants(),
        &definitions(),
    )
}

#[test]
fn draw_within_advance_limit_succeeds() {
    // 80 drawn against 100 collateral at 80% = exactly the limit (the
    // comparison is inclusive).
    let pre = facility_with_collateral(100);
    let post = must_accept(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(80)],
        pre,
        &invariants(),
        &definitions(),
    );
    assert!(has_claim(
        &post,
        "Drawdown",
        &[subj("f1"), subj("draw_1"), dec(80)]
    ));
}

#[test]
fn draw_over_advance_limit_is_rejected() {
    // 81 > 0.8 * 100 = 80: the advance-limit invariant (a multiplication)
    // rejects the candidate state.
    let pre = facility_with_collateral(100);
    let outcome = propose_with_test_actor(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(81)],
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn cumulative_draws_respect_the_advance_limit() {
    // 50 then 40 = 90 > 80: the second draw breaches the limit on the
    // running sum, not on its own amount.
    let pre = facility_with_collateral(100);
    let pre = must_accept(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(50)],
        pre,
        &invariants(),
        &definitions(),
    );
    let outcome = propose_with_test_actor(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_2"), dec(40)],
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn facility_utilisation_reports_drawn_over_collateral() {
    // 60 drawn against 100 pledged: utilisation = 60 / 100 = 0.6 (the
    // derived claim divides).
    let pre = facility_with_collateral(100);
    let state = must_accept(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(60)],
        pre,
        &invariants(),
        &definitions(),
    );
    let rows = enumerate_derived(
        &borrowing_base::facility_utilisation(),
        &state,
        &borrowing_base::definitions(),
    )
    .expect("enumerate_derived should not error");
    assert!(
        rows.contains(&claim_instance(
            "FacilityUtilisation",
            &[subj("f1"), dec_str("0.6")]
        )),
        "expected utilisation 0.6; got: {rows:#?}"
    );
}

#[test]
fn zero_value_collateral_pledge_is_rejected() {
    // collateral_value_is_positive rejects a zero pledge - which is what
    // keeps the utilisation view's divisor non-zero under all admitted
    // state, not just on the intended path.
    let state = must_accept(
        &borrowing_base::open_facility(),
        vec![subj("f1"), dec_str("0.8")],
        State::default(),
        &invariants(),
        &definitions(),
    );
    let outcome = propose_with_test_actor(
        &borrowing_base::pledge_collateral(),
        vec![subj("f1"), subj("asset_1"), dec(0)],
        &state,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn negative_drawdown_is_rejected() {
    // A drawdown moves money out, never in; drawdown_amount_is_non_negative
    // rejects a negative amount (which would otherwise model a repayment).
    let pre = facility_with_collateral(100);
    let outcome = propose_with_test_actor(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(-10)],
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn advance_rate_above_one_is_rejected() {
    // An advance rate is a fraction in [0, 1]; advance_rate_within_unit_interval
    // rejects a rate above 1 (lending more than the collateral is worth).
    let empty = State::default();
    let outcome = propose_with_test_actor(
        &borrowing_base::open_facility(),
        vec![subj("f1"), dec_str("1.5")],
        &empty,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}
