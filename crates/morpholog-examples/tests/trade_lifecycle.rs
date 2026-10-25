//! Integration tests for the trade-lifecycle example
//! (`examples/10_trade_lifecycle/`).
//!
//! Covers capture, commodity-scoped confirmation, amendment, price
//! correction, settlement, and the cap on quantity settled by each date.
//! The key tests: a backdated amendment can lift the cap so a refused
//! slice now fits, and a later amendment never unsettles an earlier slice.
//! Every refusal names the rule that refused: an invariant by name, an
//! unnamed gate by its text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Example, date, dec, has_claim, subj};
use morpholog_core::{RejectionReason, State};
use morpholog_examples::trade_lifecycle;
use std::sync::OnceLock;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&trade_lifecycle::program()))
}

/// The refusal names `rule`: an invariant's name, or a gate's text.
fn refused_by(reason: &RejectionReason, rule: &str) {
    assert!(
        reason.to_string().contains(rule),
        "expected a refusal by `{rule}`, got: {reason}"
    );
}

/// A gate refusal judged with `var` bound to `value`: it got past the
/// conjuncts that bind `var` and failed after them.
fn witnessed(reason: &RejectionReason, var: &str, value: &str) {
    let RejectionReason::Require { witness, .. } = reason else {
        panic!("expected a gate refusal, got: {reason}");
    };
    assert!(
        witness
            .iter()
            .any(|w| w.var.as_str() == var && w.value == subj(value)),
        "expected {var} = {value} in the witness, got: {witness:?}"
    );
}

const AUTHORITY_GATE: &str = "TradeCaptured(trade, commodity, _) and MayConfirm(actor, commodity)";

// Capture trade `trade` on `commodity` in `direction` (#buy / #sell) for
// `qty`, terms version `version`, effective from the trade date. Capture
// carries no authority gate, so the default actor is fine.
fn capture_args(
    trade: &str,
    commodity: &str,
    direction: &str,
    version: &str,
    qty: i64,
) -> Vec<morpholog_core::EvalValue> {
    vec![
        subj(trade),
        subj(commodity),
        subj(direction),
        subj(version),
        dec(qty),
        subj("cal26"),
        date("2026-01-15"),
        dec(50),
    ]
}

fn capture_into(
    state: State,
    trade: &str,
    commodity: &str,
    direction: &str,
    version: &str,
    qty: i64,
) -> State {
    ex().must_accept(
        &trade_lifecycle::capture_trade(),
        capture_args(trade, commodity, direction, version, qty),
        state,
    )
}

// A power trade `t1`, bought for `qty` at a trader price of 50, terms
// version `tv1` effective 2026-01-15.
fn captured(qty: i64) -> State {
    capture_into(State::default(), "t1", "power", "buy", "tv1", qty)
}

fn grant(state: State, principal: &str, commodity: &str) -> State {
    ex().must_accept(
        &trade_lifecycle::grant_confirm_authority(),
        vec![subj(principal), subj(commodity)],
        state,
    )
}

fn confirm_args(opid: &str, price: i64) -> Vec<morpholog_core::EvalValue> {
    vec![
        subj("t1"),
        subj("cp1"),
        subj("conf1"),
        subj(opid),
        dec(price),
    ]
}

// Confirm `t1` as `actor`, setting official price `opid` at `price`.
fn confirm_as(state: State, actor: &str, opid: &str, price: i64) -> State {
    ex().must_accept_as(
        &trade_lifecycle::confirm_trade(),
        confirm_args(opid, price),
        actor,
        state,
    )
}

// `t1` captured for 100, `mo` authorised for power, confirmed at 52
// under official price `op1`: the usual start of a settlement.
fn confirmed() -> State {
    confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52)
}

fn amend_args(prior: &str, new: &str, qty: i64, eff: &str) -> Vec<morpholog_core::EvalValue> {
    vec![
        subj("t1"),
        subj(prior),
        subj(new),
        dec(qty),
        subj("cal26"),
        date(eff),
    ]
}

// Amend `t1`'s terms as `mo`.
fn amend(state: State, prior: &str, new: &str, qty: i64, eff: &str) -> State {
    ex().must_accept_as(
        &trade_lifecycle::amend_trade_terms(),
        amend_args(prior, new, qty, eff),
        "mo",
        state,
    )
}

fn amend_refused(state: &State, prior: &str, new: &str, qty: i64, eff: &str) -> RejectionReason {
    ex().must_reject_as(
        &trade_lifecycle::amend_trade_terms(),
        amend_args(prior, new, qty, eff),
        "mo",
        state,
    )
}

fn settle_args(qty: i64, sid: &str, opid: &str, eff: &str) -> Vec<morpholog_core::EvalValue> {
    vec![subj("t1"), dec(qty), subj(sid), subj(opid), date(eff)]
}

// Settle a slice of `t1`: `qty` under settlement id `sid` and official
// price `opid`, effective on business date `eff`.
fn settle(state: State, qty: i64, sid: &str, opid: &str, eff: &str) -> State {
    ex().must_accept(
        &trade_lifecycle::settle_trade(),
        settle_args(qty, sid, opid, eff),
        state,
    )
}

fn settle_refused(state: &State, qty: i64, sid: &str, opid: &str, eff: &str) -> RejectionReason {
    ex().must_reject(
        &trade_lifecycle::settle_trade(),
        settle_args(qty, sid, opid, eff),
        state,
    )
}

fn settled(state: &State, qty: i64, sid: &str, opid: &str, eff: &str) -> bool {
    has_claim(state, "TradeSettled", &settle_args(qty, sid, opid, eff))
}

fn terms(state: &State, version: &str, qty: i64, eff: &str) -> bool {
    has_claim(
        state,
        "TradeTerms",
        &[
            subj("t1"),
            subj(version),
            dec(qty),
            subj("cal26"),
            date(eff),
        ],
    )
}

// ============================================================
// Capture
// ============================================================

#[test]
fn capture_records_identity_terms_and_trader_price() {
    let post = captured(100);
    // Identity is immutable; quantity lives on the versioned terms.
    assert!(has_claim(
        &post,
        "TradeCaptured",
        &[subj("t1"), subj("power"), subj("buy")],
    ));
    assert!(terms(&post, "tv1", 100, "2026-01-15"));
    assert!(has_claim(&post, "CapturedPrice", &[subj("t1"), dec(50)]));
}

#[test]
fn duplicate_capture_is_rejected() {
    // A second capture of t1, even on other terms, is refused by the
    // freshness gate: the trade id is fixed once captured.
    let reason = ex().must_reject(
        &trade_lifecycle::capture_trade(),
        capture_args("t1", "power", "sell", "tv9", 200),
        &captured(100),
    );
    refused_by(&reason, "not TradeCaptured(trade, _, _)");
}

// ============================================================
// Confirmation and commodity-scoped authority
// ============================================================

#[test]
fn confirmation_needs_a_captured_trade_and_authority_for_its_commodity() {
    let confirm = |state: &State, actor: &str| {
        ex().must_reject_as(
            &trade_lifecycle::confirm_trade(),
            confirm_args("op1", 50),
            actor,
            state,
        )
    };
    // Authority exists, but the trade was never captured.
    refused_by(
        &confirm(&grant(State::default(), "mo", "power"), "mo"),
        AUTHORITY_GATE,
    );
    // Captured, but the actor holds no authority at all.
    refused_by(&confirm(&captured(100), "rando"), AUTHORITY_GATE);
    // `mo_gas` may confirm gas; `t1` is power. The gate found the trade's
    // own commodity and refused the authority for it.
    let reason = confirm(&grant(captured(100), "mo_gas", "gas"), "mo_gas");
    refused_by(&reason, AUTHORITY_GATE);
    witnessed(&reason, "commodity", "power");
}

#[test]
fn authorised_confirm_sets_the_official_price_in_force() {
    let post = confirmed();
    assert!(has_claim(
        &post,
        "TradeConfirmed",
        &[subj("t1"), subj("cp1"), subj("conf1"), subj("mo")],
    ));
    assert!(has_claim(
        &post,
        "OfficialPrice",
        &[subj("t1"), dec(52), subj("op1")],
    ));
    assert!(has_claim(
        &post,
        "CurrentOfficialPrice",
        &[subj("t1"), subj("op1")],
    ));
}

#[test]
fn second_confirmation_is_rejected() {
    let reason = ex().must_reject_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t1"), subj("cp1"), subj("conf2"), subj("op2"), dec(53)],
        "mo",
        &confirmed(),
    );
    refused_by(&reason, "not TradeConfirmed(");
}

// ============================================================
// Effective-dated terms and amendment
// ============================================================

#[test]
fn amendment_admits_a_new_version_and_keeps_the_old() {
    // Amend tv1 (qty 100, effective 2026-01-15) to tv2 (qty 120, effective
    // 2026-02-01). Both versions stand; the lineage names which amended
    // which.
    let post = amend(
        grant(captured(100), "mo", "power"),
        "tv1",
        "tv2",
        120,
        "2026-02-01",
    );
    assert!(terms(&post, "tv1", 100, "2026-01-15"));
    assert!(terms(&post, "tv2", 120, "2026-02-01"));
    assert!(has_claim(
        &post,
        "TradeTermsSupersedes",
        &[subj("tv2"), subj("tv1")],
    ));
}

#[test]
fn an_amendment_must_extend_a_clean_chain_of_distinct_versions() {
    let pre = grant(captured(100), "mo", "power");
    // The version being amended must exist.
    refused_by(
        &amend_refused(&pre, "tv9", "tv2", 120, "2026-02-01"),
        "TradeTerms(trade, prior_version_id, _, _, _)",
    );
    // A second version on tv1's own effective date.
    refused_by(
        &amend_refused(&pre, "tv1", "tv2", 120, "2026-01-15"),
        "trade_terms_unique_by_trade_effective_from",
    );
    // tv1 reused as the new id would name a second, conflicting record.
    refused_by(
        &amend_refused(&pre, "tv1", "tv1", 120, "2026-02-01"),
        "trade_terms_unique_by_version_id",
    );
    // A terms quantity is positive.
    refused_by(
        &amend_refused(&pre, "tv1", "tv2", -10, "2026-02-01"),
        "trade_terms_quantity_is_positive",
    );
    // The chain never forks: once tv1 is amended, it cannot be again.
    let amended = amend(pre, "tv1", "tv2", 120, "2026-02-01");
    refused_by(
        &amend_refused(&amended, "tv1", "tv3", 130, "2026-03-01"),
        "not TradeTermsSupersedes(_, prior_version_id)",
    );
}

// ============================================================
// Official-price correction (restatement)
// ============================================================

#[test]
fn correction_moves_the_pointer_and_leaves_history_and_settlements_standing() {
    // A settlement relies on op1; correcting to op2 moves the price in
    // force, keeps both figures, and leaves the settlement on op1.
    let settled_on_op1 = settle(confirmed(), 100, "s1", "op1", "2026-01-20");
    let post = ex().must_accept_as(
        &trade_lifecycle::correct_official_price(),
        vec![subj("t1"), subj("op1"), subj("op2"), dec(49)],
        "mo",
        settled_on_op1,
    );
    assert!(has_claim(
        &post,
        "OfficialPrice",
        &[subj("t1"), dec(52), subj("op1")],
    ));
    assert!(has_claim(
        &post,
        "OfficialPrice",
        &[subj("t1"), dec(49), subj("op2")],
    ));
    assert!(has_claim(
        &post,
        "CurrentOfficialPrice",
        &[subj("t1"), subj("op2")],
    ));
    assert!(!has_claim(
        &post,
        "CurrentOfficialPrice",
        &[subj("t1"), subj("op1")],
    ));
    assert!(has_claim(
        &post,
        "OfficialPriceSupersedes",
        &[subj("op2"), subj("op1")],
    ));
    assert!(settled(&post, 100, "s1", "op1", "2026-01-20"));
}

#[test]
fn correct_by_actor_authorised_for_a_different_commodity_is_rejected() {
    // The correction gate is commodity-scoped just like confirmation.
    let reason = ex().must_reject_as(
        &trade_lifecycle::correct_official_price(),
        vec![subj("t1"), subj("op1"), subj("op2"), dec(49)],
        "mo_gas",
        &grant(confirmed(), "mo_gas", "gas"),
    );
    refused_by(&reason, AUTHORITY_GATE);
    witnessed(&reason, "commodity", "power");
}

#[test]
fn reusing_an_official_price_id_across_figures_is_rejected() {
    // `op1` names t1's official price. Trade t2 may not reuse it for its
    // own figure: an official price id identifies one figure.
    let state = capture_into(confirmed(), "t2", "power", "buy", "tv2", 50);
    let reason = ex().must_reject_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t2"), subj("cp2"), subj("conf2"), subj("op1"), dec(40)],
        "mo",
        &state,
    );
    refused_by(&reason, "official_price_unique_by_official_price_id");
}

// ============================================================
// Settlement and the effective-quantity cap
// ============================================================

#[test]
fn settle_before_confirmation_is_rejected() {
    // No official price in force to settle on.
    refused_by(
        &settle_refused(&captured(100), 100, "s1", "op1", "2026-01-20"),
        "CurrentOfficialPrice(trade, official_price_id)",
    );
}

#[test]
fn trade_settles_in_slices_within_the_effective_quantity() {
    // 60 effective Jan 20 then 40 effective Jan 25: both stand, the
    // running total reaching exactly the 100 in force.
    let first = settle(confirmed(), 60, "s1", "op1", "2026-01-20");
    let second = settle(first, 40, "s2", "op1", "2026-01-25");
    assert!(settled(&second, 60, "s1", "op1", "2026-01-20"));
    assert!(settled(&second, 40, "s2", "op1", "2026-01-25"));
}

#[test]
fn settlements_over_the_effective_quantity_are_rejected() {
    // One slice of 150 against the 100 in force.
    refused_by(
        &settle_refused(&confirmed(), 150, "s1", "op1", "2026-01-20"),
        "settled_within_effective_terms",
    );
    // 60 and then 60 more: each within the cap alone, 120 together.
    let first = settle(confirmed(), 60, "s1", "op1", "2026-01-20");
    refused_by(
        &settle_refused(&first, 60, "s2", "op1", "2026-01-25"),
        "settled_within_effective_terms",
    );
    // A negative slice cannot make room under the cap.
    refused_by(
        &settle_refused(&confirmed(), -10, "s1", "op1", "2026-01-20"),
        "settled_quantity_is_positive",
    );
}

#[test]
fn backdated_amendment_lifts_the_effective_cap() {
    // A slice of 110 effective 2026-02-20 is refused against qty 100. A
    // backdated amendment to 120 effective 2026-02-01 then lets the very
    // same slice in. The refused attempt left no trace.
    let confirmed = confirmed();
    refused_by(
        &settle_refused(&confirmed, 110, "s1", "op1", "2026-02-20"),
        "settled_within_effective_terms",
    );
    let after = settle(
        amend(confirmed, "tv1", "tv2", 120, "2026-02-01"),
        110,
        "s1",
        "op1",
        "2026-02-20",
    );
    assert!(settled(&after, 110, "s1", "op1", "2026-02-20"));
}

#[test]
fn settlement_under_prior_terms_remains_standing_after_amendment() {
    // 80 settles on 2026-01-20 under qty 100. An amendment *down* to 50
    // effective 2026-02-01 leaves it standing: the cap is judged against
    // the terms in force on each slice's own date.
    let post = amend(
        settle(confirmed(), 80, "s1", "op1", "2026-01-20"),
        "tv1",
        "tv2",
        50,
        "2026-02-01",
    );
    assert!(settled(&post, 80, "s1", "op1", "2026-01-20"));
    assert!(terms(&post, "tv2", 50, "2026-02-01"));
}

#[test]
fn replaying_a_settlement_id_is_rejected_before_a_second_request() {
    // Replaying the same settle_trade is refused by the freshness gate, so
    // a duplicate TradeSettlementRequested never reaches the outbox. The
    // duplicate claim alone would dedup in state; the gate stops the re-emit.
    let first = settle(confirmed(), 40, "s1", "op1", "2026-01-20");
    refused_by(
        &settle_refused(&first, 40, "s1", "op1", "2026-01-20"),
        "not TradeSettled(",
    );
}

#[test]
fn conflicting_settlements_under_one_id_are_rejected_by_the_invariant() {
    // One transformation admitting two TradeSettled with the same id but
    // different quantities slips past the settle gate;
    // trade_settled_unique_by_settlement_id refuses it.
    use morpholog_core::ir_builder;
    let settled_at = |qty: &str| {
        ir_builder::assert_(
            "TradeSettled",
            vec![
                ir_builder::var("trade"),
                ir_builder::dec(qty),
                ir_builder::var("sid"),
                ir_builder::var("opid"),
                ir_builder::var("eff"),
            ],
        )
    };
    let bad = ir_builder::transformation(
        "double_settle_one_id",
        ir_builder::params(&["trade", "sid", "opid", "eff"]),
        vec![settled_at("40"), settled_at("30")],
    );
    let reason = ex().must_reject(
        &bad,
        vec![subj("t1"), subj("s1"), subj("op1"), date("2026-01-20")],
        &confirmed(),
    );
    refused_by(&reason, "trade_settled_unique_by_settlement_id");
}

// ============================================================
// Effective-terms preconditions
// ============================================================

#[test]
fn settlement_before_any_effective_terms_is_rejected() {
    // A slice effective 2026-01-01, before any terms (from 2026-01-15), has
    // no quantity to be capped against. The settle gate refuses it.
    refused_by(
        &settle_refused(&confirmed(), 50, "s1", "op1", "2026-01-01"),
        "ef on_or_before effective_on",
    );
}

#[test]
fn settlement_before_effective_terms_is_rejected_by_the_invariant() {
    // A TradeSettled dated before any terms would pass the cap vacuously.
    // settled_date_has_effective_terms refuses it where the gate cannot see.
    use morpholog_core::ir_builder;
    let bad = ir_builder::transformation(
        "settle_before_terms",
        ir_builder::params(&["trade", "qty", "sid", "opid", "eff"]),
        vec![ir_builder::assert_(
            "TradeSettled",
            ["trade", "qty", "sid", "opid", "eff"]
                .into_iter()
                .map(ir_builder::var)
                .collect(),
        )],
    );
    let reason = ex().must_reject(
        &bad,
        settle_args(50, "s_early", "op1", "2026-01-01"),
        &confirmed(),
    );
    refused_by(&reason, "settled_date_has_effective_terms");
}

// ============================================================
// Position limit: abs of the net buys-minus-sells
// ============================================================

// Set the net-position limit for `commodity` as `actor` (who must hold
// confirm authority for it).
fn set_limit(state: State, actor: &str, commodity: &str, limit: i64) -> State {
    ex().must_accept_as(
        &trade_lifecycle::set_position_limit(),
        vec![subj(commodity), dec(limit)],
        actor,
        state,
    )
}

fn power_limited_to(limit: i64) -> State {
    set_limit(grant(State::default(), "mo", "power"), "mo", "power", limit)
}

#[test]
fn a_single_capture_is_held_within_the_limit_both_ways() {
    // Limit 100 on power: a buy of 60 nets to +60 and fits; a buy or a
    // sell of 150 nets to +/-150, whose magnitude does not.
    capture_into(power_limited_to(100), "t1", "power", "buy", "tv1", 60);
    for direction in ["buy", "sell"] {
        let reason = ex().must_reject(
            &trade_lifecycle::capture_trade(),
            capture_args("t1", "power", direction, "tv1", 150),
            &power_limited_to(100),
        );
        refused_by(&reason, "within_position_limit");
    }
}

#[test]
fn buys_and_sells_net_against_each_other() {
    // The net is bounded, not the gross: a buy of 40 and a sell of 35 net
    // to +5, within a limit of 50, though the legs gross 75.
    let state = capture_into(power_limited_to(50), "t1", "power", "buy", "tv1", 40);
    capture_into(state, "t2", "power", "sell", "tv2", 35);
}

#[test]
fn an_amendment_that_grows_a_position_past_the_limit_is_refused() {
    // The invariant judges the resulting state, not the action: amending t1
    // up to 150 breaches the limit just as a new capture of 150 would.
    let state = capture_into(power_limited_to(100), "t1", "power", "buy", "tv1", 80);
    refused_by(
        &amend_refused(&state, "tv1", "tv2", 150, "2026-02-01"),
        "within_position_limit",
    );
}

/// A settlement records the terms version that governed it on its own date.
/// After an amendment effective 2026-02-01, a slice settled on 2026-01-20
/// still names the original version.
#[test]
fn a_settlement_names_the_version_that_governed_it() {
    let amended = amend(
        grant(captured(100), "mo", "power"),
        "tv1",
        "tv2",
        120,
        "2026-02-01",
    );
    let confirmed = confirm_as(amended, "mo", "op1", 52);

    let early = settle(confirmed.clone(), 100, "s1", "op1", "2026-01-20");
    assert!(
        has_claim(&early, "SettledUnderTerms", &[subj("s1"), subj("tv1")]),
        "a January slice is governed by the version effective 2026-01-15, \
         not the amendment effective 2026-02-01"
    );

    let late = settle(confirmed, 100, "s2", "op1", "2026-02-10");
    assert!(
        has_claim(&late, "SettledUnderTerms", &[subj("s2"), subj("tv2")]),
        "a February slice is governed by the amendment"
    );
}
