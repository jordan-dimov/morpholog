//! Integration tests for the borrowing-base example
//! (`examples/11_borrowing_base/`).
//!
//! Exercises the new decimal arithmetic end to end: the advance-limit
//! invariant multiplies (`drawn <= advance_rate * collateral`), and the
//! `FacilityUtilisation` derived claim divides (`drawn / collateral`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec, dec_str, has_claim, subj};
use morpholog_core::{EvalValue, RejectionReason, State, enumerate_derived};
use morpholog_examples::borrowing_base;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&borrowing_base::program()))
}

// Facility f1 at an 80% advance rate, with `collateral` of pledged value.
fn facility_with_collateral(collateral: i64) -> State {
    let state = ex().must_accept(
        &borrowing_base::open_facility(),
        vec![subj("f1"), dec_str("0.8")],
        State::default(),
    );
    ex().must_accept(
        &borrowing_base::pledge_collateral(),
        vec![subj("f1"), subj("asset_1"), dec(collateral)],
        state,
    )
}

#[test]
fn draw_within_advance_limit_succeeds() {
    // 80 drawn against 100 collateral at 80% = exactly the limit (the
    // comparison is inclusive).
    let pre = facility_with_collateral(100);
    let post = ex().must_accept(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(80)],
        pre,
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
    ex().must_reject(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(81)],
        &pre,
    );
}

#[test]
fn cumulative_draws_respect_the_advance_limit() {
    // 50 then 40 = 90 > 80: the second draw breaches the limit on the
    // running sum, not on its own amount.
    let pre = facility_with_collateral(100);
    let pre = ex().must_accept(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(50)],
        pre,
    );
    ex().must_reject(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_2"), dec(40)],
        &pre,
    );
}

#[test]
fn facility_utilisation_reports_drawn_over_collateral() {
    // 60 drawn against 100 pledged: utilisation = 60 / 100 = 0.6 (the
    // derived claim divides).
    let pre = facility_with_collateral(100);
    let state = ex().must_accept(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(60)],
        pre,
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
fn the_asset_register_is_keyed_by_the_asset_alone() {
    // Two assets across two facilities. The register carries one row
    // per asset - the facility each pledge names is projected away -
    // and each row's figure is the pledge's own value, read by naming
    // the `collateral_value` field with the facility left unstated.
    let pre = facility_with_collateral(100);
    let pre = ex().must_accept(
        &borrowing_base::open_facility(),
        vec![subj("f2"), dec_str("0.5")],
        pre,
    );
    let state = ex().must_accept(
        &borrowing_base::pledge_collateral(),
        vec![subj("f2"), subj("asset_2"), dec(40)],
        pre,
    );
    let rows = enumerate_derived(
        &borrowing_base::asset_value(),
        &state,
        &borrowing_base::definitions(),
    )
    .expect("enumerate_derived should not error");
    assert_eq!(
        rows,
        vec![
            claim_instance("AssetValue", &[subj("asset_1"), dec(100)]),
            claim_instance("AssetValue", &[subj("asset_2"), dec(40)]),
        ],
        "one row per asset, whichever facility the pledge backs"
    );
}

#[test]
fn an_asset_backs_one_facility_at_one_value() {
    // Re-pledging asset_1 at a different value (even to another
    // facility) contradicts the one-pledge-per-asset declaration, so
    // the register's per-asset lookup always has one record to read.
    let pre = facility_with_collateral(100);
    let pre = ex().must_accept(
        &borrowing_base::open_facility(),
        vec![subj("f2"), dec_str("0.5")],
        pre,
    );
    ex().must_reject(
        &borrowing_base::pledge_collateral(),
        vec![subj("f2"), subj("asset_1"), dec(70)],
        &pre,
    );
}

#[test]
fn zero_value_collateral_pledge_is_rejected() {
    // collateral_value_is_positive rejects a zero pledge - which is what
    // keeps the utilisation view's divisor non-zero under all admitted
    // state, not just on the intended path.
    let state = ex().must_accept(
        &borrowing_base::open_facility(),
        vec![subj("f1"), dec_str("0.8")],
        State::default(),
    );
    ex().must_reject(
        &borrowing_base::pledge_collateral(),
        vec![subj("f1"), subj("asset_1"), dec(0)],
        &state,
    );
}

#[test]
fn negative_drawdown_is_rejected() {
    // A drawdown moves money out, never in; drawdown_amount_is_non_negative
    // rejects a negative amount (which would otherwise model a repayment).
    let pre = facility_with_collateral(100);
    ex().must_reject(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(-10)],
        &pre,
    );
}

#[test]
fn advance_rate_above_one_is_rejected() {
    // An advance rate is a fraction in [0, 1]; advance_rate_within_unit_interval
    // rejects a rate above 1 (lending more than the collateral is worth).
    let empty = State::default();
    ex().must_reject(
        &borrowing_base::open_facility(),
        vec![subj("f1"), dec_str("1.5")],
        &empty,
    );
}

/// The companion to metered billing's figures-only witness: this rule
/// binds its subject, so the refusal can say WHICH facility breached the
/// advance limit - the first question a credit officer asks.
#[test]
fn a_refusal_names_the_facility_that_breached_the_limit() {
    let pre = facility_with_collateral(100);
    let reason = ex().must_reject(
        &borrowing_base::draw(),
        vec![subj("f1"), subj("draw_1"), dec(81)],
        &pre,
    );
    let RejectionReason::Invariant { witness, .. } = &reason else {
        panic!("expected an invariant refusal, got {reason:?}");
    };
    let facility = witness
        .iter()
        .find(|w| w.var.as_str() == "facility")
        .unwrap_or_else(|| panic!("the witness must name the facility: {witness:?}"));
    assert_eq!(facility.value, EvalValue::Subject("f1".into()));
}
