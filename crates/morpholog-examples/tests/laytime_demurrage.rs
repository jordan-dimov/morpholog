//! Integration tests for the laytime and demurrage example
//! (`examples/12_laytime_demurrage/`).
//!
//! Exercises the time value kinds end to end: instants ordered by
//! gates and invariants, commencement computed by shifting an instant
//! with a duration, interval lengths computed by differencing
//! instants, counted laytime as a duration sum, and the
//! `TimeOnDemurrage` derived claim's floor-at-zero excess.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dur, must_accept, propose_with_test_actor, subj, ts};
use morpholog_core::{EvalValue, Invariant, Outcome, State, Transformation, enumerate_derived};
use morpholog_examples::laytime_demurrage as lay;

fn invariants() -> Vec<Invariant> {
    lay::all_invariants()
}

fn must_reject(t: &Transformation, args: Vec<EvalValue>, pre: &State) {
    let outcome = propose_with_test_actor(t, args, pre, &invariants())
        .expect("proposal should evaluate cleanly");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected rejection, got {outcome:?}"
    );
}

/// Fixture: voyage v1, 48 hours of allowed laytime, NOR tendered at
/// 14:00Z on 2026-10-24, clock commenced (so counting starts at
/// 20:00Z after the six-hour turn time, with the zero-length seed).
fn commenced_voyage() -> State {
    let state = must_accept(
        &lay::fix_voyage(),
        vec![subj("v1"), subj("mv_aurora"), subj("sines"), dur("PT48H")],
        State::default(),
        &invariants(),
    );
    let state = must_accept(
        &lay::tender_nor(),
        vec![subj("nor1"), subj("v1"), ts("2026-10-24T14:00:00Z")],
        state,
        &invariants(),
    );
    must_accept(
        &lay::commence_laytime(),
        vec![subj("v1"), subj("seed1")],
        state,
        &invariants(),
    )
}

fn excess_for(state: &State) -> EvalValue {
    let rows = enumerate_derived(&lay::time_on_demurrage(), state).unwrap();
    assert_eq!(rows.len(), 1, "one voyage, one row: {rows:?}");
    // Row shape: (voyage, allowed, excess) - the allowance shown
    // beside the time that ran past it.
    assert_eq!(rows[0].args[1], dur("PT48H"));
    rows[0].args[2].clone()
}

#[test]
fn commencement_is_the_notice_shifted_by_the_turn_time() {
    let state = commenced_voyage();
    assert!(
        state.claims().iter().any(|c| {
            c.predicate.as_str() == "LaytimeCommenced" && c.args[1] == ts("2026-10-24T20:00:00Z")
        }),
        "commencement should be NOR + PT6H: {:?}",
        state.claims()
    );
}

#[test]
fn intervals_accumulate_and_demurrage_is_the_excess_past_the_allowance() {
    let state = commenced_voyage();
    // A full day counted, then 34 more hours: 58 counted against 48
    // allowed, so ten hours on demurrage.
    let state = must_accept(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-24T20:00:00Z"),
            ts("2026-10-25T20:00:00Z"),
        ],
        state,
        &invariants(),
    );
    let state = must_accept(
        &lay::record_counting_interval(),
        vec![
            subj("i2"),
            subj("v1"),
            ts("2026-10-25T20:00:00Z"),
            ts("2026-10-27T06:00:00Z"),
        ],
        state,
        &invariants(),
    );
    let state = must_accept(
        &lay::complete_cargo_ops(),
        vec![subj("v1"), ts("2026-10-27T06:00:00Z")],
        state,
        &invariants(),
    );
    assert_eq!(excess_for(&state), dur("PT10H"));
}

#[test]
fn a_voyage_inside_its_allowance_shows_zero_demurrage_not_negative() {
    let state = commenced_voyage();
    let state = must_accept(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-24T20:00:00Z"),
            ts("2026-10-25T08:00:00Z"),
        ],
        state,
        &invariants(),
    );
    // Twelve hours counted against forty-eight allowed: the max floor
    // reports zero, never a negative span.
    assert_eq!(excess_for(&state), dur("PT0S"));
}

#[test]
fn counting_cannot_be_recorded_before_the_clock_starts() {
    // Fixture and notice, but no commencement: the bind on
    // LaytimeCommenced has nothing to match.
    let state = must_accept(
        &lay::fix_voyage(),
        vec![subj("v1"), subj("mv_aurora"), subj("sines"), dur("PT48H")],
        State::default(),
        &invariants(),
    );
    let state = must_accept(
        &lay::tender_nor(),
        vec![subj("nor1"), subj("v1"), ts("2026-10-24T14:00:00Z")],
        state,
        &invariants(),
    );
    must_reject(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-24T15:00:00Z"),
            ts("2026-10-24T16:00:00Z"),
        ],
        &state,
    );
}

#[test]
fn an_interval_starting_before_commencement_is_refused() {
    let state = commenced_voyage();
    // 19:00 is after the NOR but before the 20:00 commencement: the
    // `commenced_at at_or_before from` gate refuses it.
    must_reject(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-24T19:00:00Z"),
            ts("2026-10-24T21:00:00Z"),
        ],
        &state,
    );
}

#[test]
fn an_interval_ending_before_it_begins_is_refused() {
    let state = commenced_voyage();
    must_reject(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-25T08:00:00Z"),
            ts("2026-10-25T07:00:00Z"),
        ],
        &state,
    );
}

#[test]
fn the_clock_starts_once_per_voyage() {
    let state = commenced_voyage();
    must_reject(
        &lay::commence_laytime(),
        vec![subj("v1"), subj("seed2")],
        &state,
    );
}

#[test]
fn a_notice_needs_a_fixture_behind_it() {
    must_reject(
        &lay::tender_nor(),
        vec![subj("nor1"), subj("v_unknown"), ts("2026-10-24T14:00:00Z")],
        &State::default(),
    );
}
