//! Integration tests for the trade-lifecycle example
//! (`examples/10_trade_lifecycle/`).
//!
//! Outcome-level tests over the parsed example: capture, commodity-scoped
//! confirmation authority, effective-dated terms and amendment,
//! official-price correction as restatement, settlement gated on the
//! in-force official price, and the effective-quantity cap (cumulative
//! settled, by effective date, against the terms in force on that date).
//! The headline tests carry the weight: a backdated amendment lifts
//! the cap so a previously-rejected slice becomes admissible, and a
//! settlement made under the prior terms stays standing after a later
//! amendment - the trade-lifecycle form of the `02_verified_revenue`
//! lesson, now on the effective-time axis.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    date, dec, has_claim, must_accept, must_accept_as, propose_as, propose_with_test_actor, subj,
};
use morpholog_core::{Definition, Invariant, Outcome, State};
use morpholog_examples::trade_lifecycle;

fn invariants() -> Vec<Invariant> {
    trade_lifecycle::all_invariants()
}

fn definitions() -> Vec<Definition> {
    trade_lifecycle::definitions()
}

// A power trade `t1`, captured for `qty` at a trader price of 50, terms
// version `tv1` effective from the trade date. Capture carries no
// authority gate, so the default actor is fine.
fn captured(qty: i64) -> State {
    must_accept(
        &trade_lifecycle::capture_trade(),
        vec![
            subj("t1"),
            subj("power"),
            subj("buy"),
            subj("tv1"),
            dec(qty),
            subj("cal26"),
            date("2026-01-15"),
            dec(50),
        ],
        State::default(),
        &invariants(),
        &definitions(),
    )
}

fn grant(state: State, principal: &str, commodity: &str) -> State {
    must_accept(
        &trade_lifecycle::grant_confirm_authority(),
        vec![subj(principal), subj(commodity)],
        state,
        &invariants(),
        &definitions(),
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
        &definitions(),
    )
}

// Settle a slice of `t1`: `qty` under settlement id `sid` and official
// price `opid`, effective on business date `eff`.
fn settle(state: State, qty: i64, sid: &str, opid: &str, eff: &str) -> State {
    must_accept(
        &trade_lifecycle::settle_trade(),
        vec![subj("t1"), dec(qty), subj(sid), subj(opid), date(eff)],
        state,
        &invariants(),
        &definitions(),
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
    assert!(has_claim(
        &post,
        "TradeTerms",
        &[
            subj("t1"),
            subj("tv1"),
            dec(100),
            subj("cal26"),
            date("2026-01-15"),
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
            subj("tv9"),
            dec(200),
            subj("cal26"),
            date("2026-01-15"),
            dec(60),
        ],
        &pre,
        &invariants(),
        &definitions(),
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
    // `TradeCaptured(trade, commodity, _) and MayConfirm(...)` cannot be
    // satisfied.
    let pre = grant(State::default(), "mo", "power");
    let outcome = propose_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t1"), subj("cp1"), subj("conf1"), subj("op1"), dec(50)],
        "mo",
        &pre,
        &invariants(),
        &definitions(),
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
        &definitions(),
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
        &definitions(),
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
    let pre = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let outcome = propose_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t1"), subj("cp1"), subj("conf2"), subj("op2"), dec(53)],
        "mo",
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Effective-dated terms and amendment
// ============================================================

#[test]
fn amendment_admits_a_new_version_and_keeps_the_old() {
    // Capture terms version tv1 (qty 100, effective 2026-01-15), then amend
    // to tv2 (qty 120, effective 2026-02-01). Both versions stand on the
    // record; the lineage link names which amended which.
    let pre = grant(captured(100), "mo", "power");
    let post = must_accept_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv2"),
            dec(120),
            subj("cal26"),
            date("2026-02-01"),
        ],
        "mo",
        pre,
        &invariants(),
        &definitions(),
    );
    assert!(has_claim(
        &post,
        "TradeTerms",
        &[
            subj("t1"),
            subj("tv1"),
            dec(100),
            subj("cal26"),
            date("2026-01-15"),
        ],
    ));
    assert!(has_claim(
        &post,
        "TradeTerms",
        &[
            subj("t1"),
            subj("tv2"),
            dec(120),
            subj("cal26"),
            date("2026-02-01"),
        ],
    ));
    assert!(has_claim(
        &post,
        "TradeTermsSupersedes",
        &[subj("tv2"), subj("tv1")],
    ));
}

#[test]
fn amending_an_unknown_version_is_rejected() {
    // The version being amended must exist: the gate
    // `require TradeTerms(trade, prior_version_id, _, _, _)` fails for tv9.
    let pre = grant(captured(100), "mo", "power");
    let outcome = propose_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv9"),
            subj("tv2"),
            dec(120),
            subj("cal26"),
            date("2026-02-01"),
        ],
        "mo",
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn amending_an_already_amended_version_is_rejected() {
    // The amendment chain never forks: once tv1 has been amended to tv2,
    // a second amendment of tv1 is refused by `not TradeTermsSupersedes`.
    let amended = must_accept_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv2"),
            dec(120),
            subj("cal26"),
            date("2026-02-01"),
        ],
        "mo",
        grant(captured(100), "mo", "power"),
        &invariants(),
        &definitions(),
    );
    let outcome = propose_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv3"),
            dec(130),
            subj("cal26"),
            date("2026-03-01"),
        ],
        "mo",
        &amended,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn two_versions_on_the_same_effective_date_are_rejected() {
    // Amendments must take distinct effective dates: a second version
    // effective 2026-01-15 (tv1's date) violates
    // trade_terms_unique_by_trade_effective_from.
    let pre = grant(captured(100), "mo", "power");
    let outcome = propose_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv2"),
            dec(120),
            subj("cal26"),
            date("2026-01-15"),
        ],
        "mo",
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("trade_terms_unique_by_trade_effective_from"),
            "expected the per-effective-date uniqueness invariant, got: {reason}"
        ),
        Outcome::Accepted { .. } => panic!("two versions on one effective date must be rejected"),
    }
}

#[test]
fn reusing_a_version_id_for_a_different_record_is_rejected() {
    // tv1 already names (t1, 100, cal26, 2026-01-15). Amending with tv1 as
    // the *new* version id would make tv1 name a second, conflicting record
    // - trade_terms_unique_by_version_id refuses it.
    let pre = grant(captured(100), "mo", "power");
    let outcome = propose_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv1"),
            dec(120),
            subj("cal26"),
            date("2026-02-01"),
        ],
        "mo",
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("trade_terms_unique_by_version_id"),
            "expected the version-id uniqueness invariant, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("reusing a version id for a new record must be rejected")
        }
    }
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
        &definitions(),
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
    assert!(has_claim(
        &post,
        "OfficialPriceSupersedes",
        &[subj("op2"), subj("op1")],
    ));
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
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn reusing_an_official_price_id_across_figures_is_rejected() {
    // `op1` names trade `t1`'s official price. A different trade may not
    // reuse `op1` for its own figure - an official price id identifies one
    // figure (official_price_unique_by_official_price_id).
    let mut state = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    state = must_accept(
        &trade_lifecycle::capture_trade(),
        vec![
            subj("t2"),
            subj("power"),
            subj("buy"),
            subj("tv2"),
            dec(50),
            subj("cal26"),
            date("2026-01-15"),
            dec(40),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    let outcome = propose_as(
        &trade_lifecycle::confirm_trade(),
        vec![subj("t2"), subj("cp2"), subj("conf2"), subj("op1"), dec(40)],
        "mo",
        &state,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Settlement and the effective-quantity cap
// ============================================================

#[test]
fn settle_before_confirmation_is_rejected() {
    // No official price in force to settle on.
    let pre = captured(100);
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![
            subj("t1"),
            dec(100),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn settlement_within_the_effective_quantity_succeeds() {
    let pre = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let post = settle(pre, 100, "s1", "op1", "2026-01-20");
    assert!(has_claim(
        &post,
        "TradeSettled",
        &[
            subj("t1"),
            dec(100),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
    ));
}

#[test]
fn settlement_over_the_effective_quantity_is_rejected() {
    // Terms in force on the settlement date are qty 100; settling 150
    // breaks settled_within_effective_terms.
    let pre = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![
            subj("t1"),
            dec(150),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn trade_settles_in_slices_within_the_effective_quantity() {
    // Capture 100, settle 60 effective Jan 20 then 40 effective Jan 25 -
    // both slices admit and stand, the running total reaching exactly 100.
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let first = settle(confirmed, 60, "s1", "op1", "2026-01-20");
    let second = settle(first, 40, "s2", "op1", "2026-01-25");
    assert!(has_claim(
        &second,
        "TradeSettled",
        &[
            subj("t1"),
            dec(60),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
    ));
    assert!(has_claim(
        &second,
        "TradeSettled",
        &[
            subj("t1"),
            dec(40),
            subj("s2"),
            subj("op1"),
            date("2026-01-25"),
        ],
    ));
}

#[test]
fn slices_summing_over_the_effective_quantity_are_rejected() {
    // Capture 100, settle 60 effective Jan 20 (fine), then 60 more effective
    // Jan 25. 60 alone is within cap, but the cumulative total effective by
    // Jan 25 is 120, over the 100 in force then - rejected by the running
    // total in settled_within_effective_terms.
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let first = settle(confirmed, 60, "s1", "op1", "2026-01-20");
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![
            subj("t1"),
            dec(60),
            subj("s2"),
            subj("op1"),
            date("2026-01-25"),
        ],
        &first,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("settled_within_effective_terms"),
            "expected the cumulative effective cap to reject the over-total slice, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("slices summing over the effective quantity must be rejected")
        }
    }
}

#[test]
fn backdated_amendment_lifts_the_effective_cap() {
    // The headline of the effective-time story. Confirm at qty 100; a slice
    // of 110 effective 2026-02-20 is rejected (over the 100 in force then).
    // The desk then backdates an amendment to qty 120 effective 2026-02-01
    // - before the settlement date, after the original 2026-01-15. Now the
    // terms in force on 2026-02-20 allow 120, and the very same slice
    // admits. The rejected attempt never happened; the amendment changed
    // what is admissible.
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);

    let before = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![
            subj("t1"),
            dec(110),
            subj("s1"),
            subj("op1"),
            date("2026-02-20"),
        ],
        &confirmed,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(
        matches!(before, Outcome::Rejected { .. }),
        "110 over the original 100 must be rejected before the amendment; got {before:?}"
    );

    let amended = must_accept_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv2"),
            dec(120),
            subj("cal26"),
            date("2026-02-01"),
        ],
        "mo",
        confirmed,
        &invariants(),
        &definitions(),
    );

    let after = settle(amended, 110, "s1", "op1", "2026-02-20");
    assert!(has_claim(
        &after,
        "TradeSettled",
        &[
            subj("t1"),
            dec(110),
            subj("s1"),
            subj("op1"),
            date("2026-02-20"),
        ],
    ));
}

#[test]
fn settlement_under_prior_terms_remains_standing_after_amendment() {
    // The effective-time form of the verified-revenue lesson. A slice of 80
    // is settled effective 2026-01-20, well within the 100 in force then.
    // The desk later backdates an amendment *down* to qty 50, effective
    // 2026-02-01. Because the cap is judged per effective date against the
    // terms in force on that date, the Jan 20 slice - governed by the 100
    // still in force on Jan 20 - stays admitted. A backdated re-cut does not
    // retroactively invalidate a settlement that was legitimate when made.
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let settled = settle(confirmed, 80, "s1", "op1", "2026-01-20");
    let post = must_accept_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv2"),
            dec(50),
            subj("cal26"),
            date("2026-02-01"),
        ],
        "mo",
        settled,
        &invariants(),
        &definitions(),
    );
    assert!(has_claim(
        &post,
        "TradeSettled",
        &[
            subj("t1"),
            dec(80),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
    ));
    // The lower terms now stand for dates from 2026-02-01 onward.
    assert!(has_claim(
        &post,
        "TradeTerms",
        &[
            subj("t1"),
            subj("tv2"),
            dec(50),
            subj("cal26"),
            date("2026-02-01"),
        ],
    ));
}

#[test]
fn replaying_a_settlement_id_is_rejected_before_a_second_request() {
    // The settlement id is an idempotency key. Settling s1, then replaying
    // the exact same settle_trade, is refused by the freshness gate - so a
    // duplicate TradeSettlementRequested never reaches the outbox. (An
    // exact-duplicate claim would dedup in state and pass the invariant; it
    // is the re-emit the gate exists to stop.)
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let first = settle(confirmed, 40, "s1", "op1", "2026-01-20");
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![
            subj("t1"),
            dec(40),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
        &first,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "replaying a settlement id must be rejected before a second request; got {outcome:?}"
    );
}

#[test]
fn conflicting_settlements_under_one_id_are_rejected_by_the_invariant() {
    // The path the settle gate cannot see: a single transformation
    // admitting two TradeSettled with the same id but different quantities.
    // trade_settled_unique_by_settlement_id is the backstop that refuses
    // it, keeping the cumulative sum honest against hand-constructed state.
    use morpholog_core::ir_builder;

    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let bad = ir_builder::transformation(
        "double_settle_one_id",
        ir_builder::params(&["trade", "sid", "opid", "eff"]),
        vec![
            ir_builder::assert_(
                "TradeSettled",
                vec![
                    ir_builder::var("trade"),
                    ir_builder::dec("40"),
                    ir_builder::var("sid"),
                    ir_builder::var("opid"),
                    ir_builder::var("eff"),
                ],
            ),
            ir_builder::assert_(
                "TradeSettled",
                vec![
                    ir_builder::var("trade"),
                    ir_builder::dec("30"),
                    ir_builder::var("sid"),
                    ir_builder::var("opid"),
                    ir_builder::var("eff"),
                ],
            ),
        ],
    );
    let outcome = propose_with_test_actor(
        &bad,
        vec![subj("t1"), subj("s1"), subj("op1"), date("2026-01-20")],
        &confirmed,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("trade_settled_unique_by_settlement_id"),
            "expected the id-uniqueness invariant to reject conflicting tuples, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("two conflicting settlements under one id must be rejected")
        }
    }
}

#[test]
fn correction_after_settlement_leaves_the_settlement_standing() {
    // Settle under official price op1, then correct the official price to
    // op2. The settlement made under op1 remains a true record of what was
    // settled that day; only future settlements would see op2. (The price
    // axis; its terms-axis counterpart is
    // settlement_under_prior_terms_remains_standing_after_amendment.)
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let settled = settle(confirmed, 100, "s1", "op1", "2026-01-20");
    let post = must_accept_as(
        &trade_lifecycle::correct_official_price(),
        vec![subj("t1"), subj("op1"), subj("op2"), dec(49)],
        "mo",
        settled,
        &invariants(),
        &definitions(),
    );

    // The settlement still stands, still pointing at the price it relied on.
    assert!(has_claim(
        &post,
        "TradeSettled",
        &[
            subj("t1"),
            dec(100),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
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
    assert!(has_claim(
        &post,
        "OfficialPriceSupersedes",
        &[subj("op2"), subj("op1")],
    ));
}

// ============================================================
// Effective-terms preconditions and quantity sanity
// ============================================================

#[test]
fn settlement_before_any_effective_terms_is_rejected() {
    // Terms are effective from 2026-01-15. A slice effective 2026-01-01 -
    // before the trade had any terms - has no quantity to be capped
    // against. The settle gate refuses it on the ordinary path.
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![
            subj("t1"),
            dec(50),
            subj("s1"),
            subj("op1"),
            date("2026-01-01"),
        ],
        &confirmed,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn settlement_before_effective_terms_is_rejected_by_the_invariant() {
    // The path the settle gate cannot see: a transformation admitting a
    // TradeSettled effective before any terms version exists. Without the
    // backstop the effective cap would pass vacuously (no terms to compare
    // against); settled_date_has_effective_terms refuses it instead.
    use morpholog_core::ir_builder;

    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let bad = ir_builder::transformation(
        "settle_before_terms",
        ir_builder::params(&["trade", "qty", "sid", "opid", "eff"]),
        vec![ir_builder::assert_(
            "TradeSettled",
            vec![
                ir_builder::var("trade"),
                ir_builder::var("qty"),
                ir_builder::var("sid"),
                ir_builder::var("opid"),
                ir_builder::var("eff"),
            ],
        )],
    );
    let outcome = propose_with_test_actor(
        &bad,
        vec![
            subj("t1"),
            dec(50),
            subj("s_early"),
            subj("op1"),
            date("2026-01-01"),
        ],
        &confirmed,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("settled_date_has_effective_terms"),
            "expected the no-effective-terms backstop to reject, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("a settlement effective before any terms must be rejected")
        }
    }
}

#[test]
fn negative_terms_quantity_is_rejected() {
    // A terms quantity must be positive; an amendment to -10 is refused by
    // trade_terms_quantity_is_positive.
    let pre = grant(captured(100), "mo", "power");
    let outcome = propose_as(
        &trade_lifecycle::amend_trade_terms(),
        vec![
            subj("t1"),
            subj("tv1"),
            subj("tv2"),
            dec(-10),
            subj("cal26"),
            date("2026-02-01"),
        ],
        "mo",
        &pre,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("trade_terms_quantity_is_positive"),
            "expected the positive-terms-quantity invariant, got: {reason}"
        ),
        Outcome::Accepted { .. } => panic!("a negative terms quantity must be rejected"),
    }
}

#[test]
fn negative_settlement_quantity_is_rejected() {
    // A settled quantity must be positive; a -10 slice is refused by
    // settled_quantity_is_positive, so a negative slice cannot make room
    // under the running cap for an over-large positive one.
    let confirmed = confirm_as(grant(captured(100), "mo", "power"), "mo", "op1", 52);
    let outcome = propose_with_test_actor(
        &trade_lifecycle::settle_trade(),
        vec![
            subj("t1"),
            dec(-10),
            subj("s1"),
            subj("op1"),
            date("2026-01-20"),
        ],
        &confirmed,
        &invariants(),
        &definitions(),
    )
    .unwrap();
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.contains("settled_quantity_is_positive"),
            "expected the positive-settlement-quantity invariant, got: {reason}"
        ),
        Outcome::Accepted { .. } => panic!("a negative settlement quantity must be rejected"),
    }
}
