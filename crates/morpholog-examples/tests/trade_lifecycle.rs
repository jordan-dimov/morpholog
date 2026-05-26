//! Integration tests for the trade-lifecycle example
//! (`examples/10_trade_lifecycle/`).
//!
//! Outcome-level tests over the parsed example: capture, commodity-scoped
//! confirmation authority, official-price correction as restatement, and
//! settlement gated on the in-force official price. The heart of the
//! suite is the last test - a correction after settlement leaves the
//! prior settlement standing, the trade-lifecycle form of the
//! `02_verified_revenue` lesson.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    dec, has_claim, must_accept, must_accept_as, propose_as, propose_with_test_actor, subj,
};
use morpholog_core::{Invariant, Outcome, State};
use morpholog_examples::trade_lifecycle;

fn invariants() -> Vec<Invariant> {
    trade_lifecycle::all_invariants()
}

// A power trade `t1`, captured for `qty` at a trader price of 50. Capture
// carries no authority gate, so the default actor is fine.
fn captured(qty: i64) -> State {
    must_accept(
        &trade_lifecycle::capture_trade(),
        vec![
            subj("t1"),
            subj("power"),
            subj("buy"),
            dec(qty),
            subj("cal26"),
            dec(50),
        ],
        State::default(),
        &invariants(),
    )
}

fn grant(state: State, principal: &str, commodity: &str) -> State {
    must_accept(
        &trade_lifecycle::grant_confirm_authority(),
        vec![subj(principal), subj(commodity)],
        state,
        &invariants(),
    )
}

// Confirm `t1` as `actor`, setting official price `opid` at `price`.
fn confirm_as(state: State, actor: &str, opid: &str, price: i64) -> State {
    must_accept_as(
        &trade_lifecycle::confirm_trade(),
        vec![
            subj("t1"),
            subj("cp1"),
            subj("conf1"),
            subj(opid),
            dec(price),
        ],
        actor,
        state,
        &invariants(),
    )
}

// ============================================================
// Capture
// ============================================================

#[test]
fn capture_records_terms_and_trader_price() {
    let post = captured(100);
    assert!(has_claim(
        &post,
        "TradeCaptured",
        &[
            subj("t1"),
            subj("power"),
            subj("buy"),
            dec(100),
            subj("cal26")
        ],
    ));
    assert!(has_claim(&post, "CapturedPrice", &[subj("t1"), dec(50)]));
}

#[test]
fn duplicate_capture_is_rejected() {
    let pre = captured(100);
    let outcome = propose_with_test_actor(
        &trade_lifecycle::capture_trade(),
        vec![
            subj("t1"),
            subj("power"),
            subj("sell"),
            dec(200),
            subj("cal26"),
            dec(60),
        ],
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Confirmation and commodity-scoped authority
// ============================================================

#[test]
fn confirm_before_capture_is_rejected() {
    // Authority exists, but the trade was never captured: the gate
    // `TradeCaptured(trade, commodity, _, _, _) and MayConfirm(...)`
    // cannot be satisfied.
    let pre = grant(State::default(), "mo", "power");
    let outcome = propose_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t1"), subj("cp1"), subj("conf1"), subj("op1"), dec(50)],
        "mo",
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn confirm_by_unauthorised_actor_is_rejected() {
    let pre = captured(100);
    let outcome = propose_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t1"), subj("cp1"), subj("conf1"), subj("op1"), dec(50)],
        "rando",
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn confirm_by_actor_authorised_for_a_different_commodity_is_rejected() {
    // `mo_gas` may confirm gas, but `t1` is a power trade. The gate ties
    // the authority to the trade's own commodity, so this is refused.
    let pre = grant(captured(100), "mo_gas", "gas");
    let outcome = propose_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t1"), subj("cp1"), subj("conf1"), subj("op1"), dec(50)],
        "mo_gas",
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn authorised_confirm_sets_the_official_price_in_force() {
    let post = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    assert!(has_claim(
        &post,
        "TradeConfirmed",
        &[subj("t1"), subj("cp1"), subj("conf1")],
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
    let pre = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let outcome = propose_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t1"), subj("cp1"), subj("conf2"), subj("op2"), dec(53)],
        "mo",
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Official-price correction (restatement)
// ============================================================

#[test]
fn correction_moves_the_pointer_and_preserves_history() {
    let pre = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let post = must_accept_as(
        &trade_lifecycle::correct_official_price(),
        vec![subj("t1"), subj("op1"), subj("op2"), dec(49)],
        "mo",
        pre,
        &invariants(),
    );
    // Both official price figures survive (append-only).
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
    // The in-force pointer moved.
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
    // Lineage recorded.
    assert!(has_claim(&post, "Supersedes", &[subj("op2"), subj("op1")]));
}

#[test]
fn correct_by_actor_authorised_for_a_different_commodity_is_rejected() {
    // The correction gate is commodity-scoped just like confirmation:
    // `mo_gas` may not correct a power trade's official price.
    let pre = grant(
        confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52),
        "mo_gas",
        "gas",
    );
    let outcome = propose_as(
        &trade_lifecycle::correct_official_price(),
        vec![subj("t1"), subj("op1"), subj("op2"), dec(49)],
        "mo_gas",
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Settlement
// ============================================================

#[test]
fn settle_before_confirmation_is_rejected() {
    // No official price in force to settle on.
    let pre = captured(100);
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![subj("t1"), dec(100), subj("s1"), subj("op1")],
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn settlement_within_captured_quantity_succeeds() {
    let pre = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let post = must_accept(
        &trade_lifecycle::settle_trade(),
        vec![subj("t1"), dec(100), subj("s1"), subj("op1")],
        pre,
        &invariants(),
    );
    assert!(has_claim(
        &post,
        "TradeSettled",
        &[subj("t1"), dec(100), subj("s1"), subj("op1")],
    ));
}

#[test]
fn settlement_over_captured_quantity_is_rejected() {
    // Captured 100, settling 150 - the settled_quantity_within_captured
    // invariant rejects the candidate state.
    let pre = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![subj("t1"), dec(150), subj("s1"), subj("op1")],
        &pre,
        &invariants(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn correction_after_settlement_leaves_the_settlement_standing() {
    // The heart of the example. Settle under official price op1, then
    // correct the official price to op2. The settlement made under op1
    // remains a true record of what was settled that day; only future
    // settlements would see op2.
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let settled = must_accept(
        &trade_lifecycle::settle_trade(),
        vec![subj("t1"), dec(100), subj("s1"), subj("op1")],
        confirmed,
        &invariants(),
    );
    let post = must_accept_as(
        &trade_lifecycle::correct_official_price(),
        vec![subj("t1"), subj("op1"), subj("op2"), dec(49)],
        "mo",
        settled,
        &invariants(),
    );

    // The settlement still stands, still pointing at the price it relied on.
    assert!(has_claim(
        &post,
        "TradeSettled",
        &[subj("t1"), dec(100), subj("s1"), subj("op1")],
    ));
    // The in-force official price has moved on.
    assert!(!has_claim(
        &post,
        "CurrentOfficialPrice",
        &[subj("t1"), subj("op1")],
    ));
    assert!(has_claim(
        &post,
        "CurrentOfficialPrice",
        &[subj("t1"), subj("op2")],
    ));
    assert!(has_claim(&post, "Supersedes", &[subj("op2"), subj("op1")]));
}
