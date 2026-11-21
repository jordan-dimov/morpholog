//! The gas day cash-out example: a nomination judged on a receipt only the
//! network's receiver may record, before a deadline read from a published
//! calendar; SAP determinations that name their evidence; a late trade that
//! makes a price and every quiet day that leaned on it stale; and a
//! correction beside the original statement; and renominations, each
//! judged on its window, effective hour and lead time, with the implied
//! flow rate they leave; and trade nominations that pair, are rejected,
//! expire on the network's own completeness evidence, or are withdrawn.
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
    for calendar in [
        "gas_day",
        "dm_output_deadline",
        "renomination_start",
        "renomination_end",
        "trade_counterpart_deadline",
    ] {
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
        // The renomination window of that gas day: 15:00 London the day
        // before, in summer time, to 03:00 on the gas day, in winter time.
        (
            "renomination_start",
            "2026-10-23",
            "2026-10-23T14:00:00Z",
            "2026-10-24T14:00:00Z",
        ),
        (
            "renomination_end",
            "2026-10-25",
            "2026-10-25T03:00:00Z",
            "2026-10-26T03:00:00Z",
        ),
        // The gas day thirty days before, when its trade nominations may
        // first be made, and 06:00 London on the gas day, in summer time,
        // when a counterpart to one made before the day is due.
        (
            "gas_day",
            "2026-09-24",
            "2026-09-24T04:00:00Z",
            "2026-09-25T04:00:00Z",
        ),
        (
            "trade_counterpart_deadline",
            "2026-10-24",
            "2026-10-24T05:00:00Z",
            "2026-10-25T06:00:00Z",
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

fn implied_rates(state: &State) -> Vec<ClaimInstance> {
    enumerate_derived(
        &gas_day_cash_out::program(),
        &gas_day_cash_out::implied_flow_rate(),
        state,
    )
    .unwrap()
}

/// A renomination's message, shipper, receipt, the nomination it names
/// as previous, its effective hour and quantity; the gate that refuses it, if any; and the rate after it.
type Renomination = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    i64,
    Option<&'static str>,
    &'static str,
);

/// Renominations on the 25-hour gas day of 24 October 2026, in the order
/// they are judged. Each names the nomination it revises, which must be
/// the one in force (C4.1.5(a), C4.1.8): `ren_p`, judged after `ren_q`
/// though received before it, names a nomination `ren_q` already
/// replaced. Then the window from 15:00 London the day before to 03:00 on
/// the gas day (TPD C4.1.3(b)), an effective hour inside the day and on
/// the hour (C1.9), an hour's lead (C4.1.10(a)), not before the one it
/// replaces takes effect (the example's own order policy), and no
/// negative rate (C4.1.3(d)). The implied rate is
/// the first quantity over 24, though the day has 25 hours, plus each
/// change over the hours left (C1.8). Outcomes and rates are the
/// reference's frozen ones.
#[test]
fn a_renomination_is_judged_on_its_window_hour_and_lead_and_moves_the_implied_rate() {
    let mut s = set_up();
    s = receive(s, "nom_d24", "2026-10-23T09:00:00Z");
    s = accept(
        s,
        "nominate_output",
        "nominator_a",
        vec![
            subj("nom_d24"),
            subj("shipper_a"),
            date("2026-10-24"),
            dec(2_400_000),
        ],
    );
    let rate = |s: &State| implied_rates(s)[0].args[2].clone();
    assert_eq!(rate(&s), dec(100_000));
    // Revising a first nomination before its deadline is not modelled: a
    // second one for the same day is refused.
    s = receive(s, "nom_d24_again", "2026-10-23T09:30:00Z");
    refused_by(
        &refuse(
            &s,
            "nominate_output",
            "nominator_a",
            vec![
                subj("nom_d24_again"),
                subj("shipper_a"),
                date("2026-10-24"),
                dec(2_000_000),
            ],
        ),
        "first_for_the_day",
    );

    let cases: [Renomination; 14] = [
        (
            "ren_early",
            "shipper_a",
            "2026-10-23T13:59:00Z",
            "nom_d24",
            "2026-10-24T06:00:00Z",
            2_000_000,
            Some("in_window"),
            "100000",
        ),
        (
            "ren_a",
            "shipper_a",
            "2026-10-24T10:20:00Z",
            "nom_d24",
            "2026-10-24T13:00:00Z",
            2_000_000,
            None,
            "75000",
        ),
        (
            "ren_before_a",
            "shipper_a",
            "2026-10-24T11:00:00Z",
            "ren_a",
            "2026-10-24T12:00:00Z",
            2_100_000,
            Some("in_order"),
            "75000",
        ),
        (
            "ren_off_hour",
            "shipper_a",
            "2026-10-24T11:00:00Z",
            "ren_a",
            "2026-10-24T14:30:00Z",
            2_100_000,
            Some("on_the_hour"),
            "75000",
        ),
        (
            "ren_short_lead",
            "shipper_a",
            "2026-10-24T13:30:00Z",
            "ren_a",
            "2026-10-24T14:00:00Z",
            2_100_000,
            Some("lead_time"),
            "75000",
        ),
        (
            "ren_outside_day",
            "shipper_a",
            "2026-10-24T20:00:00Z",
            "ren_a",
            "2026-10-25T05:00:00Z",
            2_100_000,
            Some("within_the_day"),
            "75000",
        ),
        (
            "ren_q",
            "shipper_a",
            "2026-10-24T16:30:00Z",
            "ren_a",
            "2026-10-24T19:00:00Z",
            2_060_000,
            None,
            "81000",
        ),
        (
            "ren_p",
            "shipper_a",
            "2026-10-24T16:00:00Z",
            "ren_a",
            "2026-10-24T18:00:00Z",
            2_050_000,
            Some("previous_in_force"),
            "81000",
        ),
        (
            "ren_names_later",
            "shipper_a",
            "2026-10-24T16:10:00Z",
            "ren_q",
            "2026-10-24T20:00:00Z",
            2_050_000,
            Some("after_the_previous"),
            "81000",
        ),
        (
            "ren_b",
            "shipper_a",
            "2026-10-24T22:30:00Z",
            "ren_q",
            "2026-10-25T00:00:00Z",
            2_300_000,
            None,
            "129000",
        ),
        (
            "ren_small",
            "shipper_a",
            "2026-10-25T00:30:00Z",
            "ren_b",
            "2026-10-25T02:00:00Z",
            2_310_000,
            None,
            "132333.333333",
        ),
        (
            "ren_negative",
            "shipper_a",
            "2026-10-25T00:45:00Z",
            "ren_small",
            "2026-10-25T03:00:00Z",
            0,
            Some("rate_not_negative"),
            "132333.333333",
        ),
        (
            "ren_late",
            "shipper_a",
            "2026-10-25T03:01:00Z",
            "ren_small",
            "2026-10-25T04:00:00Z",
            2_200_000,
            Some("in_window"),
            "132333.333333",
        ),
        (
            "ren_no_earlier",
            "shipper_b",
            "2026-10-24T10:00:00Z",
            "nom_none",
            "2026-10-24T13:00:00Z",
            1_000_000,
            Some("earlier_nomination"),
            "132333.333333",
        ),
    ];
    s = accept(
        s,
        "grant_nomination",
        "operator",
        vec![subj("nominator_a"), subj("shipper_b")],
    );
    for (message, shipper, received_at, previous, effective_at, qty, refusal, after) in cases {
        s = receive(s, message, received_at);
        let args = vec![
            subj(message),
            subj(shipper),
            date("2026-10-24"),
            subj(previous),
            ts(effective_at),
            dec(qty),
        ];
        match refusal {
            Some(gate) => refused_by(&refuse(&s, "renominate", "nominator_a", args), gate),
            None => s = accept(s, "renominate", "nominator_a", args),
        }
        assert_eq!(rate(&s), dec_str(after), "{message}");
    }
    // Only the shipper's nominator may renominate, on the network's own
    // receipt.
    s = receive(s, "ren_unauthorised", "2026-10-24T10:00:00Z");
    refused_by(
        &refuse(
            &s,
            "renominate",
            "other_party",
            vec![
                subj("ren_unauthorised"),
                subj("shipper_a"),
                date("2026-10-24"),
                subj("ren_small"),
                ts("2026-10-24T13:00:00Z"),
                dec(2_000_000),
            ],
        ),
        "may_nominate",
    );
    // Each renomination stays on the record beside the one it replaced.
    assert_eq!(s.claims_for("OutputNomination").count(), 5);
    assert_eq!(s.claims_for("Renominates").count(), 4);
    let current: Vec<_> = s.claims_for("CurrentNomination").collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].args[2], subj("ren_small"));
}

/// One step of the trade-nomination story, with the gate that refuses it,
/// if any.
enum Event {
    /// Message, shipper, side, counterparty, quantity, receipt.
    Submit(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        i64,
        &'static str,
        Option<&'static str>,
    ),
    /// A message held and read later: message, its receipt.
    Receive(&'static str, &'static str),
    /// A held message read as a trade nomination: message, shipper, side,
    /// counterparty, quantity.
    ReadTrade(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        i64,
        Option<&'static str>,
    ),
    /// A held message the network takes as no nomination: message, reason.
    SetAside(&'static str, &'static str, Option<&'static str>),
    /// An output nomination for the day: message, shipper, quantity, receipt.
    Output(&'static str, &'static str, i64, &'static str),
    /// A held message read as an output nomination: message, shipper,
    /// quantity.
    ReadOutput(&'static str, &'static str, i64, Option<&'static str>),
    /// A held message read as a renomination: message, shipper, previous,
    /// effective hour, quantity.
    ReadRenomination(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        i64,
        Option<&'static str>,
    ),
    /// A held message read as a withdrawal: the trade nomination, the
    /// withdrawal.
    ReadWithdrawal(&'static str, &'static str, Option<&'static str>),
    /// First, second.
    Match(&'static str, &'static str, Option<&'static str>),
    /// Message, how far the network's record is complete.
    Expire(&'static str, &'static str, Option<&'static str>),
    /// Message, the withdrawal's receipt.
    Withdraw(&'static str, &'static str, Option<&'static str>),
}

/// Trade nominations for the 25-hour gas day of 24 October 2026 (TPD C5),
/// in the reference's order and with its frozen outcomes: the window from
/// thirty days before to 03:00 on the day, a counterpart due by 06:00 on
/// the day for a side made before it and within the hour otherwise, a
/// pair whose quantities differ rejected on both sides, an expiry only
/// once the network's record is complete past the deadline, every
/// message received by it is read and no counterpart waits in time, and
/// a withdrawal before the day only.
#[test]
fn trade_nominations_pair_expire_and_withdraw_as_the_code_says() {
    let mut s = set_up();
    s = accept(
        s,
        "grant_nomination",
        "operator",
        vec![subj("nominator_a"), subj("shipper_b")],
    );
    let events = [
        Event::Submit(
            "tn_early",
            "shipper_a",
            "disposing",
            "shipper_b",
            10_000,
            "2026-09-24T03:59:00Z",
            Some("in_window"),
        ),
        Event::SetAside("tn_early", "refused", None),
        Event::Output("on_a", "shipper_a", 2_400_000, "2026-10-23T08:30:00Z"),
        Event::Submit(
            "tn_self",
            "shipper_a",
            "disposing",
            "shipper_a",
            10_000,
            "2026-10-23T09:00:00Z",
            Some("two_shippers"),
        ),
        Event::SetAside("tn_self", "refused", None),
        Event::ReadOutput("tn_self", "shipper_b", 1_000_000, Some("unread")),
        Event::Submit(
            "tn_a1",
            "shipper_a",
            "disposing",
            "shipper_b",
            100_000,
            "2026-10-23T10:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_a2",
            "shipper_a",
            "disposing",
            "shipper_b",
            50_000,
            "2026-10-23T11:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_a6",
            "shipper_a",
            "disposing",
            "shipper_b",
            10_000,
            "2026-10-23T12:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_a8",
            "shipper_a",
            "disposing",
            "shipper_b",
            7_000,
            "2026-10-23T13:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_b8",
            "shipper_b",
            "acquiring",
            "shipper_a",
            7_000,
            "2026-10-23T14:00:00Z",
            None,
        ),
        Event::Match("tn_a8", "tn_b8", None),
        Event::Withdraw("tn_a8", "2026-10-23T20:00:00Z", Some("unpaired")),
        Event::SetAside("withdraw_tn_a8", "refused", None),
        Event::ReadWithdrawal("tn_a2", "withdraw_tn_a8", Some("unread")),
        Event::Withdraw("tn_a6", "2026-10-24T03:00:00Z", None),
        Event::Submit(
            "tn_b1",
            "shipper_b",
            "acquiring",
            "shipper_a",
            100_000,
            "2026-10-24T04:30:00Z",
            None,
        ),
        Event::Match("tn_a1", "tn_b1", None),
        Event::Withdraw("tn_a1", "2026-10-24T04:45:00Z", Some("before_the_day")),
        Event::SetAside("withdraw_tn_a1", "refused", None),
        Event::ReadRenomination(
            "withdraw_tn_a1",
            "shipper_a",
            "on_a",
            "2026-10-24T06:00:00Z",
            2_400_000,
            Some("unread"),
        ),
        Event::Submit(
            "tn_b2",
            "shipper_b",
            "acquiring",
            "shipper_a",
            50_000,
            "2026-10-24T05:01:00Z",
            None,
        ),
        Event::Match("tn_a2", "tn_b2", Some("in_time")),
        Event::Expire("tn_a2", "2026-10-24T04:59:00Z", Some("record_complete")),
        Event::Expire("tn_a2", "2026-10-24T05:00:00Z", Some("record_complete")),
        Event::Expire("tn_a2", "2026-10-24T05:01:00Z", None),
        Event::Submit(
            "tn_b2x",
            "shipper_b",
            "acquiring",
            "shipper_a",
            50_000,
            "2026-10-24T04:50:00Z",
            Some("record_open"),
        ),
        Event::Submit(
            "tn_a3",
            "shipper_a",
            "disposing",
            "shipper_b",
            30_000,
            "2026-10-24T12:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_b3",
            "shipper_b",
            "acquiring",
            "shipper_a",
            30_000,
            "2026-10-24T12:59:00Z",
            None,
        ),
        Event::Match("tn_a3", "tn_b3", None),
        Event::Submit(
            "tn_a4",
            "shipper_a",
            "disposing",
            "shipper_b",
            20_000,
            "2026-10-24T13:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_b4",
            "shipper_b",
            "acquiring",
            "shipper_a",
            20_000,
            "2026-10-24T14:01:00Z",
            None,
        ),
        Event::Match("tn_a4", "tn_b4", Some("in_time")),
        Event::Submit(
            "tn_a5",
            "shipper_a",
            "disposing",
            "shipper_b",
            40_000,
            "2026-10-24T15:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_b5",
            "shipper_b",
            "acquiring",
            "shipper_a",
            45_000,
            "2026-10-24T15:10:00Z",
            None,
        ),
        Event::Match("tn_a5", "tn_b5", None),
        Event::Expire("tn_b2", "2026-10-24T15:30:00Z", None),
        Event::Expire("tn_a4", "2026-10-24T15:30:00Z", None),
        Event::Expire("tn_b4", "2026-10-24T15:30:00Z", None),
        Event::Submit(
            "tn_a7",
            "shipper_a",
            "disposing",
            "shipper_b",
            5_000,
            "2026-10-24T16:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_b7",
            "shipper_b",
            "acquiring",
            "shipper_a",
            5_000,
            "2026-10-24T16:30:00Z",
            None,
        ),
        Event::Expire(
            "tn_a7",
            "2026-10-24T17:30:00Z",
            Some("no_counterpart_in_time"),
        ),
        Event::Match("tn_a7", "tn_b7", None),
        Event::Submit(
            "tn_a9",
            "shipper_a",
            "disposing",
            "shipper_b",
            40_000,
            "2026-10-24T18:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_a10",
            "shipper_a",
            "disposing",
            "shipper_b",
            45_000,
            "2026-10-24T18:05:00Z",
            None,
        ),
        Event::Submit(
            "tn_b9",
            "shipper_b",
            "acquiring",
            "shipper_a",
            45_000,
            "2026-10-24T18:10:00Z",
            None,
        ),
        Event::Match("tn_a9", "tn_b9", Some("no_equal_pair")),
        Event::Match("tn_a10", "tn_b9", None),
        Event::Submit(
            "tn_a11",
            "shipper_a",
            "disposing",
            "shipper_b",
            30_000,
            "2026-10-24T19:00:00Z",
            None,
        ),
        Event::Submit(
            "tn_b11",
            "shipper_b",
            "acquiring",
            "shipper_a",
            35_000,
            "2026-10-24T19:05:00Z",
            None,
        ),
        Event::Submit(
            "tn_b12",
            "shipper_b",
            "acquiring",
            "shipper_a",
            30_000,
            "2026-10-24T19:10:00Z",
            None,
        ),
        Event::Match("tn_a11", "tn_b11", Some("no_equal_pair")),
        Event::Match("tn_a11", "tn_b12", None),
        Event::Withdraw("tn_a9", "2026-10-24T19:30:00Z", Some("before_the_day")),
        Event::SetAside("withdraw_tn_a9", "refused", None),
        Event::Submit(
            "tn_a13",
            "shipper_a",
            "disposing",
            "shipper_b",
            25_000,
            "2026-10-24T20:10:00Z",
            None,
        ),
        Event::Receive("tn_b13", "2026-10-24T20:40:00Z"),
        Event::Expire("tn_a13", "2026-10-24T21:11:00Z", Some("every_receipt_read")),
        Event::ReadTrade(
            "tn_b13",
            "shipper_b",
            "acquiring",
            "shipper_a",
            25_000,
            None,
        ),
        Event::Match("tn_a13", "tn_b13", None),
        Event::Submit(
            "tn_a14",
            "shipper_a",
            "disposing",
            "shipper_b",
            15_000,
            "2026-10-24T21:20:00Z",
            None,
        ),
        Event::Receive("msg_garbled", "2026-10-24T22:20:00Z"),
        Event::Expire("tn_a14", "2026-10-24T22:21:00Z", Some("every_receipt_read")),
        Event::SetAside("msg_garbled", "malformed", None),
        Event::Expire("tn_a14", "2026-10-24T22:21:00Z", None),
        Event::ReadTrade(
            "msg_garbled",
            "shipper_b",
            "acquiring",
            "shipper_a",
            15_000,
            Some("unread"),
        ),
        Event::SetAside("tn_a14", "refused", Some("unread")),
        Event::Submit(
            "tn_late",
            "shipper_a",
            "disposing",
            "shipper_b",
            10_000,
            "2026-10-25T03:01:00Z",
            Some("in_window"),
        ),
        Event::SetAside("tn_late", "refused", None),
    ];
    for event in events {
        let (name, actor, args, refusal) = match event {
            Event::Submit(message, shipper, side, counterparty, qty, received_at, refusal) => {
                // A receipt before an instant the record is complete through
                // is refused at the receipt, before any nomination.
                if refusal == Some("record_open") {
                    let args = vec![subj(message), subj("gemini"), ts(received_at)];
                    refused_by(
                        &refuse(&s, "receive", "gemini_receiver", args),
                        "record_open",
                    );
                    continue;
                }
                s = receive(s, message, received_at);
                let args = vec![
                    subj(message),
                    subj(shipper),
                    date("2026-10-24"),
                    subj(side),
                    subj(counterparty),
                    dec(qty),
                ];
                ("trade_nominate", "nominator_a", args, refusal)
            }
            Event::Receive(message, received_at) => {
                s = receive(s, message, received_at);
                continue;
            }
            Event::ReadTrade(message, shipper, side, counterparty, qty, refusal) => {
                let args = vec![
                    subj(message),
                    subj(shipper),
                    date("2026-10-24"),
                    subj(side),
                    subj(counterparty),
                    dec(qty),
                ];
                ("trade_nominate", "nominator_a", args, refusal)
            }
            Event::SetAside(message, reason, refusal) => (
                "set_aside",
                "gemini_receiver",
                vec![subj(message), subj(reason)],
                refusal,
            ),
            Event::Output(message, shipper, qty, received_at) => {
                s = receive(s, message, received_at);
                let args = vec![subj(message), subj(shipper), date("2026-10-24"), dec(qty)];
                ("nominate_output", "nominator_a", args, None)
            }
            Event::ReadOutput(message, shipper, qty, refusal) => {
                let args = vec![subj(message), subj(shipper), date("2026-10-24"), dec(qty)];
                ("nominate_output", "nominator_a", args, refusal)
            }
            Event::ReadRenomination(message, shipper, previous, effective_at, qty, refusal) => {
                let args = vec![
                    subj(message),
                    subj(shipper),
                    date("2026-10-24"),
                    subj(previous),
                    ts(effective_at),
                    dec(qty),
                ];
                ("renominate", "nominator_a", args, refusal)
            }
            Event::ReadWithdrawal(message, withdrawal, refusal) => (
                "withdraw_trade",
                "nominator_a",
                vec![subj(message), subj(withdrawal)],
                refusal,
            ),
            Event::Match(first, second, refusal) => (
                "match_trades",
                "gemini_receiver",
                vec![subj(first), subj(second)],
                refusal,
            ),
            Event::Expire(message, complete_through, refusal) => {
                let known = s
                    .claims_for("CompleteThrough")
                    .any(|c| c.args[1] == ts(complete_through));
                if !known {
                    s = accept(
                        s,
                        "report_complete_through",
                        "gemini_receiver",
                        vec![subj("gemini"), ts(complete_through)],
                    );
                }
                (
                    "expire_trade",
                    "gemini_receiver",
                    vec![subj(message)],
                    refusal,
                )
            }
            Event::Withdraw(message, received_at, refusal) => {
                let withdrawal = format!("withdraw_{message}");
                s = receive(s, &withdrawal, received_at);
                (
                    "withdraw_trade",
                    "nominator_a",
                    vec![subj(message), subj(&withdrawal)],
                    refusal,
                )
            }
        };
        match refusal {
            Some(gate) => refused_by(&refuse(&s, name, actor, args), gate),
            None => s = accept(s, name, actor, args),
        }
    }
    let pairs: Vec<_> = s
        .claims_for("TradeEffective")
        .map(|c| (c.args[0].clone(), c.args[1].clone()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("tn_a8", "tn_b8"),
            ("tn_a1", "tn_b1"),
            ("tn_a3", "tn_b3"),
            ("tn_a7", "tn_b7"),
            ("tn_a10", "tn_b9"),
            ("tn_a11", "tn_b12"),
            ("tn_a13", "tn_b13"),
        ]
        .map(|(a, b)| (subj(a), subj(b)))
    );
    let mut rejected: Vec<_> = s
        .claims_for("TradeRejected")
        .map(|c| (c.args[0].clone(), c.args[1].clone()))
        .collect();
    rejected.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(
        rejected,
        [
            ("tn_a14", "no_counterpart"),
            ("tn_a2", "no_counterpart"),
            ("tn_a4", "no_counterpart"),
            ("tn_a5", "quantity_mismatch"),
            ("tn_b2", "no_counterpart"),
            ("tn_b4", "no_counterpart"),
            ("tn_b5", "quantity_mismatch"),
        ]
        .map(|(m, r)| (subj(m), subj(r)))
    );
    let withdrawn: Vec<_> = s
        .claims_for("TradeWithdrawn")
        .map(|c| c.args[0].clone())
        .collect();
    assert_eq!(withdrawn, [subj("tn_a6")]);
    // A shipper's nominator cannot pair, expire or set aside trade
    // nominations: that is the network's system's act.
    refused_by(
        &refuse(&s, "expire_trade", "nominator_a", vec![subj("tn_a9")]),
        "network",
    );
    s = receive(s, "msg_unread", "2026-10-25T03:30:00Z");
    refused_by(
        &refuse(
            &s,
            "set_aside",
            "nominator_a",
            vec![subj("msg_unread"), subj("malformed")],
        ),
        "receiver",
    );
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
