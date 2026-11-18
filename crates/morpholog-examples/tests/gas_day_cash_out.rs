//! The gas day cash-out example: a nomination judged on a receipt only the
//! network's receiver may record, before a deadline read from a published
//! calendar; SAP determinations that name their evidence; a late trade that
//! makes a price and every quiet day that leaned on it stale; and a
//! correction beside the original statement.
//!
//! The figures are the independent reference's frozen expectations
//! (computed with exact fractions from the UNC text), not values read back
//! from this programme.

#![expect(
    clippy::unwrap_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;
use std::sync::OnceLock;

use common::{Example, date, dec, dec_str, enumerate_derived, refused_by, subj, ts};
use morpholog_core::{ClaimInstance, EvalValue, RejectionReason, State, Transformation};
use morpholog_examples::gas_day_cash_out;

const CHARGING: &str = "example_charging_price_4dp_v1";
const PRECISION: &str = "example_sap_precision_v1";

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&gas_day_cash_out::program()))
}

fn act(name: &str) -> Transformation {
    gas_day_cash_out::program()
        .transformation(name)
        .unwrap_or_else(|| panic!("no act {name}"))
        .clone()
}

fn accept(state: State, name: &str, actor: &str, args: Vec<EvalValue>) -> State {
    ex().must_accept_as(&act(name), args, actor, state)
}

fn refuse(state: &State, name: &str, actor: &str, args: Vec<EvalValue>) -> RejectionReason {
    ex().must_reject_as(&act(name), args, actor, state)
}

/// The authorities, both policies, the default margin from 1 October 2026,
/// and the calendar rows the nominations read.
fn set_up() -> State {
    let mut s = accept(
        State::default(),
        "appoint_operator",
        "operator",
        vec![subj("operator")],
    );
    for calendar in ["gas_day", "dm_output_deadline"] {
        s = accept(
            s,
            "appoint_calendar_publisher",
            "operator",
            vec![subj(calendar), subj("calendar_publisher")],
        );
    }
    let grants: [(&str, Vec<EvalValue>); 6] = [
        (
            "appoint_receipt_authority",
            vec![subj("gemini"), subj("gemini_receiver")],
        ),
        (
            "grant_nomination",
            vec![subj("nominator_a"), subj("shipper_a")],
        ),
        ("appoint_market_data_source", vec![subj("market_data")]),
        ("appoint_settlement_agent", vec![subj("settlement")]),
        (
            "adopt_charging_policy",
            vec![subj(CHARGING), dec_str("0.0001")],
        ),
        (
            "adopt_sap_precision",
            vec![subj(PRECISION), dec_str("0.00000001")],
        ),
    ];
    for (name, args) in grants {
        s = accept(s, name, "operator", args);
    }
    s = accept(
        s,
        "publish_default_smp",
        "market_data",
        vec![subj("nbp"), date("2026-10-01"), dec_str("0.0771")],
    );
    // The 25-hour gas day of 24 October 2026, and the 13:00 London row of
    // the day before it, as `generate local-days` writes them.
    let rows = [
        (
            "gas_day",
            "2026-10-24",
            "2026-10-24T04:00:00Z",
            "2026-10-25T05:00:00Z",
        ),
        (
            "dm_output_deadline",
            "2026-10-23",
            "2026-10-23T12:00:00Z",
            "2026-10-24T12:00:00Z",
        ),
    ];
    for (calendar, local_date, starts_at, ends_at) in rows {
        s = accept(
            s,
            "publish_local_day",
            "calendar_publisher",
            vec![
                subj(calendar),
                date(local_date),
                ts(starts_at),
                ts(ends_at),
                subj("2026e"),
                subj("sha256:zone"),
            ],
        );
    }
    s
}

fn receive(state: State, message: &str, at: &str) -> State {
    accept(
        state,
        "receive",
        "gemini_receiver",
        vec![subj(message), subj("gemini"), ts(at)],
    )
}

fn nomination(message: &str) -> Vec<EvalValue> {
    vec![
        subj(message),
        subj("shipper_a"),
        date("2026-10-24"),
        dec(500_000),
    ]
}

#[test]
fn a_nomination_is_judged_on_its_receipt_by_a_party_allowed_to_make_it() {
    let mut s = set_up();
    s = receive(s, "nom_in_time", "2026-10-23T11:59:00Z");
    s = receive(s, "nom_late", "2026-10-23T12:01:00Z");
    s = receive(s, "nom_unauthorised", "2026-10-23T11:30:00Z");

    // 12:59 London: in time. 13:01 London: refused on its receipt, however
    // early the act deciding it runs.
    s = accept(
        s,
        "nominate_output",
        "nominator_a",
        nomination("nom_in_time"),
    );
    refused_by(
        &refuse(&s, "nominate_output", "nominator_a", nomination("nom_late")),
        "in_time",
    );
    refused_by(
        &refuse(
            &s,
            "nominate_output",
            "other_party",
            nomination("nom_unauthorised"),
        ),
        "may_nominate",
    );
    // A shipper cannot record a receipt that would make its late
    // nomination look timely.
    refused_by(
        &refuse(
            &s,
            "receive",
            "shipper_a",
            vec![
                subj("nom_forged"),
                subj("gemini"),
                ts("2026-10-23T11:00:00Z"),
            ],
        ),
        "receiver",
    );
    assert_eq!(s.claims_for("OutputNomination").count(), 1);
}

const TRADES: [(&str, &str, i64, &str); 11] = [
    ("t13a", "2026-10-13", 500_000, "3.10"),
    ("t13b", "2026-10-13", 250_000, "3.16"),
    ("t14a", "2026-10-14", 400_000, "3.05"),
    ("t14b", "2026-10-14", 400_000, "3.11"),
    ("t15a", "2026-10-15", 300_000, "3.20"),
    ("t16a", "2026-10-16", 600_000, "3.02"),
    ("t16b", "2026-10-16", 200_000, "3.12"),
    ("t17a", "2026-10-17", 350_000, "3.08"),
    ("t18a", "2026-10-18", 450_000, "3.15"),
    ("t18b", "2026-10-18", 150_000, "3.21"),
    ("t19a", "2026-10-19", 500_000, "3.12"),
];

fn trade(state: State, trade: &str, day: &str, qty: i64, price: &str) -> State {
    accept(
        state,
        "report_trade",
        "market_data",
        vec![subj(trade), date(day), dec(qty), dec_str(price)],
    )
}

fn determine(state: State, name: &str, day: &str) -> State {
    accept(state, name, "settlement", vec![date(day), subj(PRECISION)])
}

fn current_sap(state: &State, day: &str) -> EvalValue {
    let version = state
        .claims_for("CurrentSap")
        .find(|c| c.args[0] == date(day))
        .unwrap_or_else(|| panic!("no current SAP for {day}"))
        .args[1]
        .clone();
    state
        .claims_for("SapDetermination")
        .find(|c| c.args[0] == version)
        .unwrap()
        .args[2]
        .clone()
}

fn assert_saps(state: &State, expected: &[(&str, &str)]) {
    for (day, sap) in expected {
        assert_eq!(current_sap(state, day), dec_str(sap), "SAP for {day}");
    }
}

fn statement(state: State, shipper: &str, day: &str, policy: &str) -> State {
    accept(
        state,
        "issue_statement",
        "settlement",
        vec![subj(shipper), date(day), subj(policy)],
    )
}

fn statements(state: &State) -> Vec<ClaimInstance> {
    state.claims_for("Statement").cloned().collect()
}

fn needs_review(state: &State) -> Vec<ClaimInstance> {
    enumerate_derived(
        &gas_day_cash_out::program(),
        &gas_day_cash_out::needs_review(),
        state,
    )
    .unwrap()
}

/// SMP Buy, SMP Sell, charging price and cash flow to the shipper of a
/// statement.
fn figures(row: &ClaimInstance) -> [EvalValue; 4] {
    [
        row.args[5].clone(),
        row.args[6].clone(),
        row.args[8].clone(),
        row.args[9].clone(),
    ]
}

fn imbalance(state: State, shipper: &str, day: &str, kwh: i64) -> State {
    accept(
        state,
        "record_imbalance",
        "settlement",
        vec![
            subj(&format!("imbalance_{shipper}_{day}")),
            subj(shipper),
            date(day),
            dec(kwh),
        ],
    )
}

#[test]
fn a_late_trade_flags_the_statement_that_used_it_and_the_correction_stands_beside_it() {
    let mut s = set_up();
    for (id, day, qty, price) in TRADES {
        s = trade(s, id, day, qty, price);
    }
    for day in 13..=19 {
        s = determine(s, "determine_sap", &format!("2026-10-{day}"));
    }
    s = determine(s, "determine_fallback_sap", "2026-10-20");
    s = determine(s, "determine_fallback_sap", "2026-10-21");
    assert_saps(
        &s,
        &[
            ("2026-10-14", "3.08"),
            ("2026-10-20", "3.11571429"),
            ("2026-10-21", "3.11510204"),
        ],
    );
    let fallback = s
        .claims_for("SapUsedPrior")
        .filter(|c| c.args[0] == current_version(&s, "2026-10-21"))
        .count();
    assert_eq!(fallback, 7, "a quiet day names all seven days it leaned on");

    s = accept(
        s,
        "report_actions_complete",
        "market_data",
        vec![date("2026-10-21")],
    );
    s = imbalance(s, "shipper_a", "2026-10-21", -1_200_000);
    s = statement(s, "shipper_a", "2026-10-21", CHARGING);
    let original = statements(&s).pop().unwrap();
    assert_eq!(
        figures(&original),
        [
            dec_str("3.19220204"),
            dec_str("3.03800204"),
            dec_str("3.1922"),
            dec_str("-3830640.0000"),
        ]
    );
    assert_eq!(original.args[4], subj("imbalance_shipper_a_2026-10-21"));
    assert!(needs_review(&s).is_empty());

    // A Monday trade nobody had reported: the 14th is stale, and so is
    // every quiet day that leaned on it, so the statement needs review.
    s = trade(s, "t14late", "2026-10-14", 200_000, "3.40");
    assert_eq!(
        needs_review(&s),
        vec![ClaimInstance {
            predicate: "NeedsReview".into(),
            args: vec![original.args[0].clone()],
        }]
    );
    refused_by(
        &refuse(
            &s,
            "determine_sap",
            "settlement",
            vec![date("2026-10-13"), subj(PRECISION)],
        ),
        "first_or_stale",
    );

    s = determine(s, "determine_sap", "2026-10-14");
    s = determine(s, "determine_fallback_sap", "2026-10-20");
    s = determine(s, "determine_fallback_sap", "2026-10-21");
    assert_saps(
        &s,
        &[
            ("2026-10-14", "3.144"),
            ("2026-10-20", "3.12485714"),
            ("2026-10-21", "3.12555102"),
        ],
    );
    s = statement(s, "shipper_a", "2026-10-21", CHARGING);
    let rows = statements(&s);
    assert!(rows.contains(&original), "the original statement stands");
    let correction = rows.into_iter().find(|r| *r != original).unwrap();
    assert_eq!(
        figures(&correction),
        [
            dec_str("3.20265102"),
            dec_str("3.04845102"),
            dec_str("3.2027"),
            dec_str("-3843240.0000"),
        ]
    );
    assert!(
        s.claims_for("StatementCorrects")
            .any(|c| c.args == [correction.args[0].clone(), original.args[0].clone()])
    );
    assert!(needs_review(&s).is_empty());
}

fn current_version(state: &State, day: &str) -> EvalValue {
    state
        .claims_for("CurrentSap")
        .find(|c| c.args[0] == date(day))
        .unwrap()
        .args[1]
        .clone()
}

/// A trade day whose SAP is `price`, its actions complete, and a statement
/// for `imbalance` under `policy`. Returns the statement's charging price
/// and charge.
fn charged(
    state: State,
    day: &str,
    price: &str,
    kwh: i64,
    shipper: &str,
    policy: &str,
) -> (State, EvalValue, EvalValue) {
    let mut s = state;
    if !s.claims_for("CurrentSap").any(|c| c.args[0] == date(day)) {
        s = trade(s, &format!("t_{day}"), day, 1_000, price);
        s = determine(s, "determine_sap", day);
        s = accept(s, "report_actions_complete", "market_data", vec![date(day)]);
    }
    s = imbalance(s, shipper, day, kwh);
    s = statement(s, shipper, day, policy);
    let row = statements(&s)
        .into_iter()
        .find(|r| r.args[1] == subj(shipper) && r.args[2] == date(day))
        .unwrap();
    let (price, cash_flow) = (row.args[8].clone(), row.args[9].clone());
    (s, price, cash_flow)
}

#[test]
fn the_charging_price_rounds_halves_away_from_zero_on_both_sides_of_zero() {
    // SMP Buy is SAP + 0.0771 for a short shipper; SMP Sell is SAP - 0.0771
    // for a long one, negative when SAP is tiny.
    let cases = [
        ("2026-10-02", "3.00004999", -1, "3.0771"),
        ("2026-10-03", "3.00005", -1, "3.0772"),
        ("2026-10-04", "3.00005001", -1, "3.0772"),
        ("2026-10-05", "0.00004999", 1, "-0.0771"),
        ("2026-10-06", "0.00005", 1, "-0.0771"),
        ("2026-10-07", "0.00005001", 1, "-0.0770"),
    ];
    let mut s = set_up();
    for (day, sap, imbalance, expected) in cases {
        let (next, price, _) = charged(s, day, sap, imbalance, "shipper_a", CHARGING);
        assert_eq!(price, dec_str(expected), "{day}: SAP {sap}");
        s = next;
    }
}

#[test]
fn a_price_difference_below_the_quantum_moves_a_large_charge_only_as_the_policy_says() {
    let s = set_up();
    let (s, _, below) = charged(
        s,
        "2026-10-08",
        "3.00004999",
        -50_000_000,
        "shipper_a",
        CHARGING,
    );
    let (_, _, at) = charged(
        s,
        "2026-10-09",
        "3.00005",
        -50_000_000,
        "shipper_a",
        CHARGING,
    );
    assert_eq!(below, dec_str("-153855000.0000"));
    assert_eq!(at, dec_str("-153860000.0000"));
}

#[test]
fn statements_under_different_charging_policies_keep_their_own_figures() {
    let s = set_up();
    let (s, first_price, _) = charged(s, "2026-10-10", "3.00004999", -1_000, "shipper_a", CHARGING);
    let s = accept(
        s,
        "adopt_charging_policy",
        "operator",
        vec![subj("example_charging_price_2dp_v1"), dec_str("0.01")],
    );
    let first = statements(&s).pop().unwrap();
    let (s, second_price, _) = charged(
        s,
        "2026-10-10",
        "3.00004999",
        -1_000,
        "shipper_b",
        "example_charging_price_2dp_v1",
    );
    assert_eq!(first_price, dec_str("3.0771"));
    assert_eq!(second_price, dec_str("3.08"));
    let rows = statements(&s);
    assert!(rows.contains(&first), "the first statement is unchanged");
    let policies: Vec<_> = rows.iter().map(|r| r.args[7].clone()).collect();
    assert!(policies.contains(&subj(CHARGING)));
    assert!(policies.contains(&subj("example_charging_price_2dp_v1")));
}

#[test]
fn no_statement_before_the_days_actions_are_known_complete() {
    let mut s = set_up();
    s = trade(s, "t1", "2026-10-11", 1_000, "3.00");
    s = determine(s, "determine_sap", "2026-10-11");
    s = imbalance(s, "shipper_a", "2026-10-11", -1_000);
    refused_by(
        &refuse(
            &s,
            "issue_statement",
            "settlement",
            vec![subj("shipper_a"), date("2026-10-11"), subj(CHARGING)],
        ),
        "actions_complete",
    );
    // With National Gas's own buy action worse than the default margin,
    // SMP Buy is that action's price.
    s = accept(
        s,
        "report_buy_action",
        "market_data",
        vec![subj("ng_buy_1"), date("2026-10-11"), dec_str("3.5")],
    );
    s = accept(
        s,
        "report_actions_complete",
        "market_data",
        vec![date("2026-10-11")],
    );
    s = statement(s, "shipper_a", "2026-10-11", CHARGING);
    let row = statements(&s).pop().unwrap();
    assert_eq!(row.args[5], dec_str("3.5"));
}

#[test]
fn a_nomination_counts_only_the_networks_own_receipt() {
    let mut s = accept(
        set_up(),
        "appoint_receipt_authority",
        "operator",
        vec![subj("other_system"), subj("other_receiver")],
    );
    s = accept(
        s,
        "receive",
        "other_receiver",
        vec![
            subj("nom_elsewhere"),
            subj("other_system"),
            ts("2026-10-23T11:00:00Z"),
        ],
    );
    refused_by(
        &refuse(
            &s,
            "nominate_output",
            "nominator_a",
            nomination("nom_elsewhere"),
        ),
        "received_by_the_network",
    );
}

#[test]
fn no_statement_or_fallback_rests_on_a_price_already_known_to_be_stale() {
    let mut s = set_up();
    for (id, day, qty, price) in TRADES {
        s = trade(s, id, day, qty, price);
    }
    for day in 13..=19 {
        s = determine(s, "determine_sap", &format!("2026-10-{day}"));
    }
    // A late trade for the 19th arrives before anything else is decided.
    s = trade(s, "t19late", "2026-10-19", 100_000, "3.50");
    refused_by(
        &refuse(
            &s,
            "determine_fallback_sap",
            "settlement",
            vec![date("2026-10-20"), subj(PRECISION)],
        ),
        "inputs_current",
    );
    s = accept(
        s,
        "report_actions_complete",
        "market_data",
        vec![date("2026-10-19")],
    );
    s = imbalance(s, "shipper_a", "2026-10-19", -1_000);
    refused_by(
        &refuse(
            &s,
            "issue_statement",
            "settlement",
            vec![subj("shipper_a"), date("2026-10-19"), subj(CHARGING)],
        ),
        "sap_not_stale",
    );
    // Re-determined, the 19th carries both: the statement and the fallback go.
    s = determine(s, "determine_sap", "2026-10-19");
    s = statement(s, "shipper_a", "2026-10-19", CHARGING);
    determine(s, "determine_fallback_sap", "2026-10-20");
}

#[test]
fn the_cash_flow_is_signed_by_imbalance_and_price_together() {
    // SAP 3: Buy 3.0771, Sell 2.9229. SAP 0.01: Sell -0.0671. SAP -1:
    // Buy -0.9229. Positive is National Gas paying the shipper.
    let cases = [
        ("2026-10-02", "3", 1_000, "2922.9000"),
        ("2026-10-03", "3", -1_000, "-3077.1000"),
        ("2026-10-04", "0.01", 1_000, "-67.1000"),
        ("2026-10-05", "-1", -1_000, "922.9000"),
        ("2026-10-06", "3", 0, "0.0000"),
    ];
    let mut s = set_up();
    for (day, sap, kwh, expected) in cases {
        let (next, _, cash_flow) = charged(s, day, sap, kwh, "shipper_a", CHARGING);
        assert_eq!(cash_flow, dec_str(expected), "{day}: SAP {sap}, {kwh} kWh");
        s = next;
    }
}
