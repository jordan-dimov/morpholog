//! Integration tests for the closed-loop execution example
//! (`examples/21_closed_loop_execution/`).
//!
//! An untrusted agent proposes orders, a custodied executor places the
//! admitted ones, and the venue's own report comes back in as claims.
//! These tests walk the lawful flow, pin each gate the agent and the
//! operator meet, and enumerate every reconciliation finding over a
//! hand-built history: matched, mismatched, unobserved, unauthorised and
//! ambiguous. The login binding of each name is adapter-level and is
//! tested against PostgreSQL.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, dec, dec_str, has_claim, subj};
use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, Outcome, RejectionReason, State, Transformation,
};
use morpholog_examples::closed_loop_execution as cle;
use morpholog_test_support::enumerate_derived;

const OPERATOR: &str = "ops";
const AGENT: &str = "agent";
const REPORTER: &str = "reporter";
const VENUE: &str = "venue";
const INSTRUMENT: &str = "power_q1";

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&cle::program()))
}

fn accept_as(state: State, t: &Transformation, args: Vec<EvalValue>, actor: &str) -> State {
    ex().must_accept_as(t, args, actor, state)
}

/// Propose as `actor` and return the new state with the claims the act
/// admitted, so a test can read back a subject the act drew.
fn commit_as(
    state: &State,
    t: &Transformation,
    args: Vec<EvalValue>,
    actor: &str,
) -> (State, Vec<ClaimInstance>) {
    match ex().propose_as(t, args, actor, state) {
        Ok(Outcome::Accepted {
            candidate_state,
            asserted_claims,
            ..
        }) => (candidate_state, asserted_claims),
        other => panic!("expected `{}` to commit, got {other:?}", t.name),
    }
}

fn gate_rejected(outcome: &Result<Outcome, EvalError>) -> bool {
    matches!(
        outcome,
        Ok(Outcome::Rejected {
            reason: RejectionReason::Require { .. },
        })
    )
}

fn rejected_by(outcome: &Result<Outcome, EvalError>, rule: &str) -> bool {
    matches!(
        outcome,
        Ok(Outcome::Rejected {
            reason: RejectionReason::Invariant { name, .. },
        }) if name.as_str() == rule
    )
}

/// The desk set up: an operator bound to its login, a trader and a
/// reporter enrolled, one venue, a mandate of fifty on one instrument,
/// and the reporter speaking for the venue.
fn desk() -> State {
    let mut s = State::default();
    s = accept_as(
        s,
        &cle::appoint_operator(),
        vec![subj(OPERATOR), subj("ops_login")],
        OPERATOR,
    );
    s = accept_as(
        s,
        &cle::enrol_login(),
        vec![subj(AGENT), subj("agent_login")],
        OPERATOR,
    );
    s = accept_as(
        s,
        &cle::enrol_login(),
        vec![subj(REPORTER), subj("reporter_login")],
        OPERATOR,
    );
    s = accept_as(s, &cle::declare_venue(), vec![subj(VENUE)], OPERATOR);
    s = accept_as(
        s,
        &cle::grant_mandate(),
        vec![subj(AGENT), subj(INSTRUMENT), dec(50)],
        OPERATOR,
    );
    s = accept_as(
        s,
        &cle::grant_feed(),
        vec![subj(REPORTER), subj(VENUE)],
        OPERATOR,
    );
    s
}

fn order_args(side: &str, qty: i64, price: &str) -> Vec<EvalValue> {
    vec![
        subj(VENUE),
        subj(INSTRUMENT),
        subj(side),
        dec(qty),
        dec_str(price),
    ]
}

/// The agent places an order; returns the state and the order's subject.
fn place(state: &State, side: &str, qty: i64, price: &str) -> (State, EvalValue) {
    let (next, admitted) = commit_as(
        state,
        &cle::place_order(),
        order_args(side, qty, price),
        AGENT,
    );
    let order = admitted
        .iter()
        .find(|c| c.predicate.as_str() == "OrderAuthorised")
        .expect("the act admits the authorisation")
        .args[0]
        .clone();
    (next, order)
}

/// The reporter brings in one venue report; returns the state and the
/// report's subject.
fn report(
    state: &State,
    sequence: i64,
    order_ref: EvalValue,
    venue_order_id: &str,
    side: &str,
    qty: i64,
    price: &str,
) -> (State, EvalValue) {
    let (next, admitted) = commit_as(
        state,
        &cle::observe_venue_report(),
        vec![
            subj(VENUE),
            dec(sequence),
            order_ref,
            subj(venue_order_id),
            subj(INSTRUMENT),
            subj(side),
            dec(qty),
            dec_str(price),
        ],
        REPORTER,
    );
    let report = admitted
        .iter()
        .find(|c| c.predicate.as_str() == "VenueReport")
        .expect("the act admits the report")
        .args[0]
        .clone();
    (next, report)
}

fn rows(state: &State, derived: &morpholog_core::DerivedClaim) -> Vec<Vec<EvalValue>> {
    enumerate_derived(&cle::program(), derived, state)
        .expect("the read enumerates")
        .into_iter()
        .map(|c| c.args)
        .collect()
}

// ============================================================
// The lawful flow
// ============================================================

#[test]
fn an_admitted_order_is_authorised_and_emits_one_intent_carrying_its_subject() {
    let s = desk();
    let outcome = ex().propose_as(
        &cle::place_order(),
        order_args("buy", 10, "48.5"),
        AGENT,
        &s,
    );
    let Ok(Outcome::Accepted {
        asserted_claims,
        emitted_intents,
        ..
    }) = outcome
    else {
        panic!("expected the lawful order to commit, got {outcome:?}");
    };
    let authorised: Vec<_> = asserted_claims
        .iter()
        .filter(|c| c.predicate.as_str() == "OrderAuthorised")
        .collect();
    assert_eq!(authorised.len(), 1, "one authorisation per order");
    let order = authorised[0].args[0].clone();
    assert_eq!(
        authorised[0].args[1..],
        [
            subj(VENUE),
            subj(INSTRUMENT),
            subj("buy"),
            dec(10),
            dec_str("48.5")
        ]
    );
    assert_eq!(emitted_intents.len(), 1, "one intent per order");
    let intent = &emitted_intents[0];
    assert_eq!(intent.name.as_str(), "PlaceOrder");
    assert_eq!(
        intent.args,
        vec![
            order,
            subj(VENUE),
            subj(INSTRUMENT),
            subj("buy"),
            dec(10),
            dec_str("48.5")
        ],
        "the intent carries the order's own subject, the venue's client reference"
    );
}

#[test]
fn two_orders_draw_two_subjects() {
    let s = desk();
    let (s, first) = place(&s, "buy", 10, "48.5");
    let (_, second) = place(&s, "buy", 10, "48.5");
    assert_ne!(first, second, "the same terms twice are two orders");
}

// ============================================================
// The agent's gates
// ============================================================

#[test]
fn an_order_over_the_mandate_is_refused() {
    let s = desk();
    let outcome = ex().propose_as(
        &cle::place_order(),
        order_args("buy", 51, "48.5"),
        AGENT,
        &s,
    );
    assert!(gate_rejected(&outcome), "{outcome:?}");
    let outcome = ex().propose_as(
        &cle::place_order(),
        order_args("buy", 50, "48.5"),
        AGENT,
        &s,
    );
    assert!(
        matches!(outcome, Ok(Outcome::Accepted { .. })),
        "the mandate is inclusive: {outcome:?}"
    );
}

#[test]
fn an_order_without_a_mandate_is_refused() {
    let s = desk();
    let other_instrument = vec![
        subj(VENUE),
        subj("gas_q1"),
        subj("buy"),
        dec(1),
        dec_str("20"),
    ];
    let outcome = ex().propose_as(&cle::place_order(), other_instrument, AGENT, &s);
    assert!(
        gate_rejected(&outcome),
        "no mandate on the instrument: {outcome:?}"
    );
    let outcome = ex().propose_as(
        &cle::place_order(),
        order_args("buy", 1, "48.5"),
        "stranger",
        &s,
    );
    assert!(gate_rejected(&outcome), "no mandate at all: {outcome:?}");
    let outcome = ex().propose_as(
        &cle::place_order(),
        order_args("buy", 1, "48.5"),
        OPERATOR,
        &s,
    );
    assert!(
        gate_rejected(&outcome),
        "the operator appoints traders and is not one: {outcome:?}"
    );
}

#[test]
fn an_order_at_an_undeclared_venue_or_with_an_unknown_side_or_a_bad_figure_is_refused() {
    let s = desk();
    let elsewhere = vec![
        subj("other_venue"),
        subj(INSTRUMENT),
        subj("buy"),
        dec(1),
        dec_str("48.5"),
    ];
    for (name, args) in [
        ("undeclared venue", elsewhere),
        ("unknown side", order_args("hold", 1, "48.5")),
        ("zero quantity", order_args("buy", 0, "48.5")),
        ("negative quantity", order_args("sell", -1, "48.5")),
        ("zero price", order_args("buy", 1, "0")),
    ] {
        let outcome = ex().propose_as(&cle::place_order(), args, AGENT, &s);
        assert!(gate_rejected(&outcome), "{name}: {outcome:?}");
    }
}

// ============================================================
// The operator's gates
// ============================================================

#[test]
fn only_the_operator_appoints_and_a_mandate_goes_only_to_an_enrolled_name() {
    let s = desk();
    for (name, t, args) in [
        (
            "enrol",
            cle::enrol_login(),
            vec![subj("newcomer"), subj("newcomer_login")],
        ),
        (
            "declare a venue",
            cle::declare_venue(),
            vec![subj("venue_2")],
        ),
        (
            "grant a mandate",
            cle::grant_mandate(),
            vec![subj(AGENT), subj("gas_q1"), dec(5)],
        ),
        (
            "grant a feed",
            cle::grant_feed(),
            vec![subj(AGENT), subj(VENUE)],
        ),
    ] {
        let outcome = ex().propose_as(&t, args, AGENT, &s);
        assert!(
            gate_rejected(&outcome),
            "the agent may not {name}: {outcome:?}"
        );
    }
    let unenrolled = ex().propose_as(
        &cle::grant_mandate(),
        vec![subj("newcomer"), subj(INSTRUMENT), dec(5)],
        OPERATOR,
        &s,
    );
    assert!(
        gate_rejected(&unenrolled),
        "a name nobody has bound to a login gets no mandate: {unenrolled:?}"
    );
    let unenrolled = ex().propose_as(
        &cle::grant_feed(),
        vec![subj("newcomer"), subj(VENUE)],
        OPERATOR,
        &s,
    );
    assert!(gate_rejected(&unenrolled), "nor a feed: {unenrolled:?}");
    let non_positive = ex().propose_as(
        &cle::grant_mandate(),
        vec![subj(AGENT), subj("gas_q1"), dec(0)],
        OPERATOR,
        &s,
    );
    assert!(
        gate_rejected(&non_positive),
        "a mandate of nothing: {non_positive:?}"
    );
}

#[test]
fn the_operator_is_appointed_once() {
    let s = desk();
    let outcome = ex().propose_as(
        &cle::appoint_operator(),
        vec![subj("usurper"), subj("usurper_login")],
        "usurper",
        &s,
    );
    assert!(gate_rejected(&outcome), "{outcome:?}");
    assert!(has_claim(
        &s,
        "ActorAssertionAuthority",
        &[subj(OPERATOR), subj("ops_login")]
    ));
}

#[test]
fn a_second_mandate_on_the_same_instrument_is_refused_by_name() {
    let s = desk();
    let outcome = ex().propose_as(
        &cle::grant_mandate(),
        vec![subj(AGENT), subj(INSTRUMENT), dec(500)],
        OPERATOR,
        &s,
    );
    assert!(
        rejected_by(&outcome, "trading_mandate_unique_by_trader_instrument"),
        "{outcome:?}"
    );
}

// ============================================================
// The reporter's gates
// ============================================================

#[test]
fn only_the_feed_operator_reports_and_a_replayed_message_is_refused_by_name() {
    let s = desk();
    let (s, order) = place(&s, "buy", 10, "48.5");
    let args = |sequence: i64| {
        vec![
            subj(VENUE),
            dec(sequence),
            order.clone(),
            subj("V-1001"),
            subj(INSTRUMENT),
            subj("buy"),
            dec(10),
            dec_str("48.5"),
        ]
    };
    for actor in [AGENT, OPERATOR, "stranger"] {
        let outcome = ex().propose_as(&cle::observe_venue_report(), args(1), actor, &s);
        assert!(
            gate_rejected(&outcome),
            "{actor} may not report: {outcome:?}"
        );
    }
    let (s, _) = report(&s, 1, order.clone(), "V-1001", "buy", 10, "48.5");
    let replay = ex().propose_as(&cle::observe_venue_report(), args(1), REPORTER, &s);
    assert!(
        rejected_by(&replay, "venue_report_unique_by_venue_sequence"),
        "the same message again: {replay:?}"
    );
    let next = ex().propose_as(&cle::observe_venue_report(), args(2), REPORTER, &s);
    assert!(
        matches!(next, Ok(Outcome::Accepted { .. })),
        "a new message is a new report: {next:?}"
    );
}

// ============================================================
// The reconciliation
// ============================================================

#[test]
fn an_order_the_venue_reports_on_the_same_terms_is_matched_and_nothing_else() {
    let s = desk();
    let (s, order) = place(&s, "buy", 10, "48.5");
    assert_eq!(
        rows(&s, &cle::unobserved()),
        vec![vec![order.clone(), dec(10)]]
    );
    let (s, rep) = report(&s, 1, order.clone(), "V-1001", "buy", 10, "48.5");
    assert_eq!(
        rows(&s, &cle::matched()),
        vec![vec![order.clone(), rep.clone(), dec(10)]]
    );
    assert!(rows(&s, &cle::unobserved()).is_empty());
    assert!(rows(&s, &cle::unauthorised()).is_empty());
    assert!(rows(&s, &cle::mismatched()).is_empty());
    assert!(rows(&s, &cle::ambiguous()).is_empty());
}

#[test]
fn a_report_naming_no_authorised_order_is_the_unauthorised_finding() {
    let s = desk();
    let (s, order) = place(&s, "buy", 10, "48.5");
    let (s, _) = report(&s, 1, order.clone(), "V-1001", "buy", 10, "48.5");
    let (s, ghost) = report(
        &s,
        2,
        subj("ref-nobody-issued"),
        "V-1002",
        "sell",
        100,
        "47",
    );
    assert_eq!(
        rows(&s, &cle::unauthorised()),
        vec![vec![ghost, subj("V-1002"), dec(100), dec_str("47")]]
    );
    assert_eq!(
        rows(&s, &cle::matched()).len(),
        1,
        "the lawful order still matches"
    );
    assert!(rows(&s, &cle::mismatched()).is_empty());
}

#[test]
fn a_report_under_an_authorised_reference_on_other_terms_is_mismatched_not_matched() {
    let s = desk();
    let (s, order) = place(&s, "buy", 10, "48.5");
    let (s, rep) = report(&s, 1, order.clone(), "V-1001", "buy", 100, "48.5");
    assert_eq!(
        rows(&s, &cle::mismatched()),
        vec![vec![rep, order.clone(), dec(100), dec(10)]]
    );
    assert!(rows(&s, &cle::matched()).is_empty());
    assert!(
        rows(&s, &cle::unauthorised()).is_empty(),
        "the reference is known, so it is not the headline finding"
    );
    assert!(
        rows(&s, &cle::unobserved()).is_empty(),
        "the venue did report on the order, just not the order"
    );
    let (s, _) = report(&s, 2, order.clone(), "V-1003", "sell", 10, "48.5");
    assert_eq!(
        rows(&s, &cle::mismatched()).len(),
        2,
        "a wrong side is a mismatch too"
    );
}

#[test]
fn two_venue_orders_for_one_authorisation_is_ambiguous_and_both_match() {
    let s = desk();
    let (s, order) = place(&s, "buy", 10, "48.5");
    let (s, _) = report(&s, 1, order.clone(), "V-1001", "buy", 10, "48.5");
    assert!(rows(&s, &cle::ambiguous()).is_empty());
    let (s, _) = report(&s, 2, order.clone(), "V-1001", "buy", 10, "48.5");
    assert!(
        rows(&s, &cle::ambiguous()).is_empty(),
        "the venue repeating its own order id is one order reported twice"
    );
    let (s, _) = report(&s, 3, order.clone(), "V-1002", "buy", 10, "48.5");
    assert_eq!(
        rows(&s, &cle::ambiguous()),
        vec![vec![order.clone(), dec(3)]],
        "one row per order, whichever way the pair is read"
    );
    assert_eq!(rows(&s, &cle::matched()).len(), 3);
}

#[test]
fn the_findings_are_computed_not_stored() {
    let s = desk();
    let (s, _) = place(&s, "buy", 10, "48.5");
    assert!(
        !s.claims()
            .iter()
            .any(|c| c.predicate.as_str() == "Unobserved"),
        "a finding is never an admitted claim"
    );
}
