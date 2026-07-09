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
use std::sync::OnceLock;

use common::{Example, dur, qty, subj, ts};
use morpholog_core::{EvalValue, State, enumerate_derived};
use morpholog_examples::laytime_demurrage as lay;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&lay::program()))
}

/// Fixture: voyage v1, 48 hours of allowed laytime, NOR tendered at
/// 14:00Z on 2026-10-24, clock commenced (so counting starts at
/// 20:00Z after the six-hour turn time, with the zero-length seed).
fn commenced_voyage() -> State {
    let state = ex().must_accept(
        &lay::fix_voyage(),
        vec![subj("v1"), subj("mv_aurora"), subj("sines"), dur("PT48H")],
        State::default(),
    );
    let state = ex().must_accept(
        &lay::tender_nor(),
        vec![subj("nor1"), subj("v1"), ts("2026-10-24T14:00:00Z")],
        state,
    );
    ex().must_accept(
        &lay::commence_laytime(),
        vec![subj("v1"), subj("seed1")],
        state,
    )
}

fn excess_for(state: &State) -> EvalValue {
    let rows = enumerate_derived(&lay::time_on_demurrage(), state, &lay::definitions()).unwrap();
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
    let state = ex().must_accept(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-24T20:00:00Z"),
            ts("2026-10-25T20:00:00Z"),
        ],
        state,
    );
    let state = ex().must_accept(
        &lay::record_counting_interval(),
        vec![
            subj("i2"),
            subj("v1"),
            ts("2026-10-25T20:00:00Z"),
            ts("2026-10-27T06:00:00Z"),
        ],
        state,
    );
    let state = ex().must_accept(
        &lay::complete_cargo_ops(),
        vec![subj("v1"), ts("2026-10-27T06:00:00Z")],
        state,
    );
    assert_eq!(excess_for(&state), dur("PT10H"));
}

#[test]
fn a_voyage_inside_its_allowance_shows_zero_demurrage_not_negative() {
    let state = commenced_voyage();
    let state = ex().must_accept(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-24T20:00:00Z"),
            ts("2026-10-25T08:00:00Z"),
        ],
        state,
    );
    // Twelve hours counted against forty-eight allowed: the max floor
    // reports zero, never a negative span.
    assert_eq!(excess_for(&state), dur("PT0S"));
}

#[test]
fn counting_cannot_be_recorded_before_the_clock_starts() {
    // Fixture and notice, but no commencement: the bind on
    // LaytimeCommenced has nothing to match.
    let state = ex().must_accept(
        &lay::fix_voyage(),
        vec![subj("v1"), subj("mv_aurora"), subj("sines"), dur("PT48H")],
        State::default(),
    );
    let state = ex().must_accept(
        &lay::tender_nor(),
        vec![subj("nor1"), subj("v1"), ts("2026-10-24T14:00:00Z")],
        state,
    );
    ex().must_reject(
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
    ex().must_reject(
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
    ex().must_reject(
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
fn the_laytime_commenced_unique_by_voyage_per_voyage() {
    let state = commenced_voyage();
    ex().must_reject(
        &lay::commence_laytime(),
        vec![subj("v1"), subj("seed2")],
        &state,
    );
}

#[test]
fn a_notice_needs_a_fixture_behind_it() {
    ex().must_reject(
        &lay::tender_nor(),
        vec![subj("nor1"), subj("v_unknown"), ts("2026-10-24T14:00:00Z")],
        &State::default(),
    );
}

#[test]
fn time_on_demurrage_is_safe_to_inspect_before_commencement() {
    // Fixture only: the clock has not started, so the derived view has
    // no row for the voyage - and crucially does not error. (Without
    // the LaytimeCommenced conjunct in the domain, this read would hit
    // the empty-duration-sum landmine the seed pattern exists to
    // defuse. The review of this PR caught exactly that.)
    let state = ex().must_accept(
        &lay::fix_voyage(),
        vec![subj("v1"), subj("mv_aurora"), subj("sines"), dur("PT48H")],
        State::default(),
    );
    let rows = enumerate_derived(&lay::time_on_demurrage(), &state, &lay::definitions())
        .expect("pre-commencement inspection must not error");
    assert!(rows.is_empty(), "no clock, no row: {rows:?}");
}

#[test]
fn two_voyages_enumerate_deterministically() {
    // A second voyage alongside the first: two rows, in a stable
    // order. This also exercises the derived-claim ordering over the
    // new value kinds (subject keys, duration keys) - the silent
    // failure mode there would be rows in a varying order.
    let state = commenced_voyage();
    let state = ex().must_accept(
        &lay::fix_voyage(),
        vec![subj("v2"), subj("mv_borealis"), subj("sines"), dur("PT24H")],
        state,
    );
    let state = ex().must_accept(
        &lay::tender_nor(),
        vec![subj("nor2"), subj("v2"), ts("2026-11-01T08:00:00Z")],
        state,
    );
    let state = ex().must_accept(
        &lay::commence_laytime(),
        vec![subj("v2"), subj("seed2")],
        state,
    );
    let rows = enumerate_derived(&lay::time_on_demurrage(), &state, &lay::definitions()).unwrap();
    assert_eq!(rows.len(), 2, "two commenced voyages, two rows");
    let again = enumerate_derived(&lay::time_on_demurrage(), &state, &lay::definitions()).unwrap();
    assert_eq!(rows, again, "enumeration order is deterministic");
    let voyages: Vec<_> = rows.iter().map(|r| r.args[0].clone()).collect();
    assert!(
        voyages.contains(&subj("v1")) && voyages.contains(&subj("v2")),
        "both voyages present: {voyages:?}"
    );
}

// ============================================================
// Stage 3: unit-tagged quantities - the cargo book in tonnes,
// the money book in dollars.
// ============================================================

#[test]
fn cargo_book_caps_at_the_declared_capacity() {
    let state = ex().must_accept(
        &lay::fix_voyage(),
        vec![subj("v1"), subj("mv_aurora"), subj("sines"), dur("PT48H")],
        State::default(),
    );
    let state = ex().must_accept(
        &lay::declare_capacity(),
        vec![subj("v1"), subj("seed_parcel"), qty("45000", "t")],
        state,
    );
    let state = ex().must_accept(
        &lay::load_parcel(),
        vec![subj("p1"), subj("v1"), qty("30000", "t")],
        state,
    );
    // To the boundary: the comparison is exact, tonnes against tonnes.
    let state = ex().must_accept(
        &lay::load_parcel(),
        vec![subj("p2"), subj("v1"), qty("15000", "t")],
        state,
    );
    // One more tonne does not fit.
    ex().must_reject(
        &lay::load_parcel(),
        vec![subj("p3"), subj("v1"), qty("1", "t")],
        &state,
    );
}

/// Fixture: a commenced voyage that ran 132 hours past its 48-hour
/// allowance (one 180-hour counting interval), with the demurrage
/// rate agreed at 25000 USD per day and cargo ops completed - the
/// state a settlement negotiation starts from. 132 hours is exactly
/// 5.5 days, so the due figure is exactly 137500 USD.
fn voyage_on_demurrage() -> State {
    let state = commenced_voyage();
    let state = ex().must_accept(
        &lay::record_counting_interval(),
        vec![
            subj("i1"),
            subj("v1"),
            ts("2026-10-24T20:00:00Z"),
            ts("2026-11-01T08:00:00Z"),
        ],
        state,
    );
    let state = ex().must_accept(
        &lay::agree_demurrage_rate(),
        vec![subj("v1"), subj("seed_settlement"), qty("25000", "USD")],
        state,
    );
    ex().must_accept(
        &lay::complete_cargo_ops(),
        vec![subj("v1"), ts("2026-11-01T08:00:00Z")],
        state,
    )
}

#[test]
fn demurrage_settles_to_the_exact_due_figure_and_not_a_cent_more() {
    let state = voyage_on_demurrage();
    // The derived figure prices the delay before anything is paid:
    // row shape (voyage, allowed, daily, due).
    let rows = enumerate_derived(&lay::demurrage_due(), &state, &lay::definitions()).unwrap();
    assert_eq!(rows.len(), 1, "one voyage on demurrage: {rows:?}");
    assert_eq!(rows[0].args[3], qty("137500.00", "USD"));

    // Settle the whole figure, to the cent.
    let state = ex().must_accept(
        &lay::settle_demurrage(),
        vec![subj("s1"), subj("v1"), qty("137500.00", "USD")],
        state,
    );
    // A cent past what the delay is worth is refused.
    ex().must_reject(
        &lay::settle_demurrage(),
        vec![subj("s2"), subj("v1"), qty("0.01", "USD")],
        &state,
    );
}

#[test]
fn tonnes_offered_as_dollars_do_not_evaluate_let_alone_commit() {
    let state = voyage_on_demurrage();
    let err = ex()
        .propose(
            &lay::settle_demurrage(),
            vec![subj("s1"), subj("v1"), qty("5", "t")],
            &state,
        )
        .expect_err("a tonne is not a dollar: kernel error, not rejection");
    let msg = format!("{err}");
    assert!(
        msg.contains("Decimal[USD]") && msg.contains("Decimal[t]"),
        "the refusal names both units: {msg}"
    );
}

#[test]
fn demurrage_due_has_no_row_before_the_rate_is_agreed() {
    // Same vacuity discipline as TimeOnDemurrage: before the rate (or
    // the clock) exists, the question "what is owed?" has no row
    // rather than a wrong answer.
    let state = commenced_voyage();
    let rows = enumerate_derived(&lay::demurrage_due(), &state, &lay::definitions()).unwrap();
    assert!(rows.is_empty(), "no rate agreed yet: {rows:?}");
}

#[test]
fn settlement_before_the_rate_is_agreed_is_refused() {
    // Without a rate the delay has no price, and the cap invariant's
    // antecedent would be vacuously false - so both the gate and the
    // settlement_requires_rate invariant refuse the attempt. Cargo
    // ops are completed first, so the rate really is the only thing
    // missing.
    let state = commenced_voyage();
    let state = ex().must_accept(
        &lay::complete_cargo_ops(),
        vec![subj("v1"), ts("2026-10-25T08:00:00Z")],
        state,
    );
    ex().must_reject(
        &lay::settle_demurrage(),
        vec![subj("s1"), subj("v1"), qty("1000", "USD")],
        &state,
    );
}
