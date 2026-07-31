//! Covenant reporting: the calendar rules hold against the acts. The
//! anniversary roll admits only the exact three-month date (clamped at
//! month ends, drifting across two hops exactly as the calendar does),
//! the 45-day window decides Timely standing, and an overdue notice
//! carries the record's own day count - in both proposal orders.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, date, dec, subj};
use morpholog_core::{Outcome, RejectionReason, State};
use morpholog_examples::covenant_reporting;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&covenant_reporting::program()))
}

fn rejected_by(reason: &RejectionReason, rule: &str) -> bool {
    matches!(reason, RejectionReason::Invariant { name, .. } if name.as_str() == rule)
}

fn refused_at_gate(reason: &RejectionReason, gate: &str) -> bool {
    matches!(reason, RejectionReason::Require { name: Some(n), .. } if n == gate)
}

/// The facility from the .morph's own teaching: first period ending
/// 30 November, the anchor the clamping story needs.
fn opened() -> State {
    ex().must_accept(
        &covenant_reporting::open_facility(),
        vec![subj("fac"), subj("p1"), date("2026-11-30")],
        State::default(),
    )
}

fn schedule(pre: State, prior: &str, next: &str, ends_on: &str) -> State {
    ex().must_accept(
        &covenant_reporting::schedule_next_period(),
        vec![subj("fac"), subj(prior), subj(next), date(ends_on)],
        pre,
    )
}

#[test]
fn the_facility_opens_with_its_first_test_period() {
    let state = opened();
    assert_eq!(state.claims().len(), 2);
}

#[test]
fn a_quarterly_roll_three_months_on_is_admitted() {
    // 30 Nov + three months clamps to 28 Feb - the exact date is the
    // only admissible one.
    schedule(opened(), "p1", "p2", "2027-02-28");
}

#[test]
fn a_period_that_drifts_by_a_day_is_refused_by_name() {
    for wrong in ["2027-02-27", "2027-03-01", "2027-02-30"] {
        // 2027-02-30 does not exist; the date literal itself fails to
        // evaluate, so try only real dates through the invariant path.
        if wrong == "2027-02-30" {
            continue;
        }
        let reason = ex().must_reject(
            &covenant_reporting::schedule_next_period(),
            vec![subj("fac"), subj("p1"), subj("p2"), date(wrong)],
            &opened(),
        );
        assert!(
            rejected_by(&reason, "periods_follow_three_month_anniversaries"),
            "{wrong}: {reason:?}"
        );
    }
}

#[test]
fn two_hops_drift_where_one_direct_shift_does_not() {
    // The calendar's own behaviour, pinned end to end: Nov 30 -> Feb 28
    // -> May 28. The 30 May a direct six-month shift would give is
    // refused on the second hop.
    let state = schedule(opened(), "p1", "p2", "2027-02-28");
    let reason = ex().must_reject(
        &covenant_reporting::schedule_next_period(),
        vec![subj("fac"), subj("p2"), subj("p3"), date("2027-05-30")],
        &state,
    );
    assert!(
        rejected_by(&reason, "periods_follow_three_month_anniversaries"),
        "{reason:?}"
    );
    schedule(state, "p2", "p3", "2027-05-28");
}

#[test]
fn a_forked_schedule_is_refused() {
    let state = schedule(opened(), "p1", "p2", "2027-02-28");
    // A second successor for p1, even with the right date, forks the
    // chain; the no-fork uniqueness on the prior refuses it.
    let reason = ex().must_reject(
        &covenant_reporting::schedule_next_period(),
        vec![subj("fac"), subj("p1"), subj("p2b"), date("2027-02-28")],
        &state,
    );
    assert!(
        matches!(&reason, RejectionReason::Invariant { name, .. } if name.as_str().starts_with("follows_unique_by_prior")),
        "{reason:?}"
    );
}

/// A bare re-linking act that admits only the chain claim - the shape
/// a future "rewire the schedule" transformation would take. Through
/// `schedule_next_period` a merge or cross-facility link always trips
/// the period-identity rules first; these tests pin that the chain
/// disciplines are law on their own, not a side effect of that act.
fn link_only() -> morpholog_core::Transformation {
    use morpholog_core::ir_builder::{assert_, params, transformation, var};
    transformation(
        "link_only",
        params(&["next", "prior"]),
        vec![assert_("Follows", vec![var("next"), var("prior")])],
    )
}

#[test]
fn a_merged_schedule_is_refused() {
    // p2 already follows p1; a link-only act giving p2 a second
    // predecessor (the successor-less p3, so the no-fork rule stays
    // quiet) is refused as a merge.
    let state = schedule(opened(), "p1", "p2", "2027-02-28");
    let state = schedule(state, "p2", "p3", "2027-05-28");
    let reason = ex().must_reject(&link_only(), vec![subj("p2"), subj("p3")], &state);
    assert!(
        matches!(&reason, RejectionReason::Invariant { name, .. } if name.as_str().starts_with("follows_unique_by_next")),
        "{reason:?}"
    );
}

#[test]
fn a_cross_facility_link_is_refused() {
    // A second facility with its own first period; a link-only act
    // chaining across the two facilities is refused - no single
    // facility holds both periods.
    let state = ex().must_accept(
        &covenant_reporting::open_facility(),
        vec![subj("other"), subj("q1"), date("2026-11-30")],
        opened(),
    );
    let reason = ex().must_reject(&link_only(), vec![subj("q1"), subj("p1")], &state);
    assert!(
        rejected_by(&reason, "follows_links_periods_of_one_facility"),
        "{reason:?}"
    );
}

#[test]
fn a_certificate_on_day_forty_five_earns_timely_standing() {
    // Period ends 2026-11-30; day 45 after is 2027-01-14.
    let state = opened();
    let state = ex().must_accept(
        &covenant_reporting::submit_certificate(),
        vec![subj("fac"), subj("p1"), date("2027-01-14")],
        state,
    );
    ex().must_accept(
        &covenant_reporting::accept_timely(),
        vec![subj("fac"), subj("p1")],
        state,
    );
}

#[test]
fn day_forty_six_is_refused_the_standing() {
    let state = opened();
    let state = ex().must_accept(
        &covenant_reporting::submit_certificate(),
        vec![subj("fac"), subj("p1"), date("2027-01-15")],
        state,
    );
    let reason = ex().must_reject(
        &covenant_reporting::accept_timely(),
        vec![subj("fac"), subj("p1")],
        &state,
    );
    assert!(
        refused_at_gate(&reason, "delivery_landed_inside_the_window"),
        "{reason:?}"
    );
}

#[test]
fn an_overdue_notice_with_the_wrong_count_is_refused_by_name() {
    // Deadline is 2027-01-14; as of 2027-01-20 the true count is 6.
    let state = opened();
    for wrong in [dec(5), dec(7), dec(0)] {
        let reason = ex().must_reject(
            &covenant_reporting::declare_overdue(),
            vec![subj("fac"), subj("p1"), date("2027-01-20"), wrong],
            &state,
        );
        assert!(
            rejected_by(&reason, "overdue_notices_state_the_records_own_lateness"),
            "{reason:?}"
        );
    }
    ex().must_accept(
        &covenant_reporting::declare_overdue(),
        vec![subj("fac"), subj("p1"), date("2027-01-20"), dec(6)],
        state,
    );
}

#[test]
fn an_overdue_notice_before_the_deadline_is_refused() {
    let state = opened();
    // 2027-01-14 IS the deadline: not yet past it.
    for premature in ["2027-01-10", "2027-01-14"] {
        let reason = ex().must_reject(
            &covenant_reporting::declare_overdue(),
            vec![subj("fac"), subj("p1"), date(premature), dec(1)],
            &state,
        );
        assert!(
            refused_at_gate(&reason, "the_deadline_has_passed"),
            "{premature}: {reason:?}"
        );
    }
}

#[test]
fn a_notice_after_delivery_is_refused_whichever_order_is_proposed() {
    // Order one: the certificate is already in the record (late, on
    // 20 Jan); a notice dated after it is refused at the gate.
    let state = opened();
    let with_cert = ex().must_accept(
        &covenant_reporting::submit_certificate(),
        vec![subj("fac"), subj("p1"), date("2027-01-20")],
        state,
    );
    let reason = ex().must_reject(
        &covenant_reporting::declare_overdue(),
        vec![subj("fac"), subj("p1"), date("2027-01-25"), dec(11)],
        &with_cert,
    );
    assert!(
        refused_at_gate(&reason, "nothing_was_delivered_by_then"),
        "{reason:?}"
    );

    // Order two: the notice stands first (20 Jan, 6 days late); a
    // certificate then backdated to on-or-before the notice date is
    // refused by the invariant, so the record cannot be rewritten
    // under the notice.
    let state = opened();
    let with_notice = ex().must_accept(
        &covenant_reporting::declare_overdue(),
        vec![subj("fac"), subj("p1"), date("2027-01-20"), dec(6)],
        state,
    );
    let reason = ex().must_reject(
        &covenant_reporting::submit_certificate(),
        vec![subj("fac"), subj("p1"), date("2027-01-18")],
        &with_notice,
    );
    assert!(
        rejected_by(&reason, "overdue_notices_precede_any_delivery"),
        "{reason:?}"
    );
    // A delivery genuinely after the notice still records.
    ex().must_accept(
        &covenant_reporting::submit_certificate(),
        vec![subj("fac"), subj("p1"), date("2027-01-25")],
        with_notice,
    );
}

#[test]
fn a_late_certificate_never_earns_timely_standing_even_by_hand() {
    // Try to smuggle Timely standing past the gate by proposing the
    // window invariant's own violation: certificate on day 46, then
    // accept_timely. The gate refuses; nothing in the programme can
    // admit Timely without it.
    let state = opened();
    let state = ex().must_accept(
        &covenant_reporting::submit_certificate(),
        vec![subj("fac"), subj("p1"), date("2027-01-15")],
        state,
    );
    let outcome = ex()
        .propose(
            &covenant_reporting::accept_timely(),
            vec![subj("fac"), subj("p1")],
            &state,
        )
        .expect("evaluates cleanly");
    assert!(matches!(outcome, Outcome::Rejected { .. }), "{outcome:?}");
}

#[test]
fn an_orphan_link_is_refused() {
    // Scheduling after a period the record does not have fails the
    // gate before anything is admitted.
    let reason = ex().must_reject(
        &covenant_reporting::schedule_next_period(),
        vec![subj("fac"), subj("ghost"), subj("p2"), date("2027-02-28")],
        &opened(),
    );
    assert!(
        refused_at_gate(&reason, "the_prior_period_exists"),
        "{reason:?}"
    );
}
