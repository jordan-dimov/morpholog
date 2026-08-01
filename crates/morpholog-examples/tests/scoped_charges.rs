//! Scoped charges: the record picks the figure's source. One line act
//! serves both sources; the applied figure is computed from the
//! tariff's declaration, a caller-sourced line commits with no meter
//! reading at all (the untaken branch never evaluates), and a line
//! wearing the wrong source's figure is refused by name - whichever
//! act proposes it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, dec, subj};
use morpholog_core::{EvalValue, Outcome, RejectionReason, State};
use morpholog_examples::scoped_charges;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&scoped_charges::program()))
}

fn rejected_by(reason: &RejectionReason, rule: &str) -> bool {
    matches!(reason, RejectionReason::Invariant { name, .. } if name.as_str() == rule)
}

fn declared(charge: &str, source: &str) -> State {
    ex().must_accept(
        &scoped_charges::declare_charge(),
        vec![subj(charge), subj(source)],
        State::default(),
    )
}

#[test]
fn a_caller_sourced_line_takes_the_proposal_with_no_reading_at_all() {
    // The laziness demonstration: no MeterReading exists anywhere,
    // and the metered branch is never evaluated.
    let state = declared("standing", "caller");
    let after = ex().must_accept(
        &scoped_charges::record_line(),
        vec![subj("l1"), subj("standing"), subj("m1"), dec(42)],
        state,
    );
    let line = after
        .claims()
        .iter()
        .find(|c| c.predicate.as_str() == "ChargeLine")
        .expect("the line committed")
        .clone();
    assert_eq!(line.args[3], EvalValue::Decimal(42.into()), "proposed");
    assert_eq!(line.args[4], EvalValue::Decimal(42.into()), "applied");
}

#[test]
fn a_metered_line_takes_the_meters_own_reading() {
    let state = declared("consumption", "meter");
    let state = ex().must_accept(
        &scoped_charges::read_meter(),
        vec![subj("m1"), dec(431)],
        state,
    );
    // The caller proposes 999; the record applies the meter's 431.
    let after = ex().must_accept(
        &scoped_charges::record_line(),
        vec![subj("l1"), subj("consumption"), subj("m1"), dec(999)],
        state,
    );
    let line = after
        .claims()
        .iter()
        .find(|c| c.predicate.as_str() == "ChargeLine")
        .expect("the line committed")
        .clone();
    assert_eq!(line.args[3], EvalValue::Decimal(999.into()), "proposed");
    assert_eq!(line.args[4], EvalValue::Decimal(431.into()), "applied");
}

#[test]
fn a_metered_line_with_no_reading_is_a_lawful_refusal() {
    let state = declared("consumption", "meter");
    let reason = ex().must_reject(
        &scoped_charges::record_line(),
        vec![subj("l1"), subj("consumption"), subj("m1"), dec(999)],
        &state,
    );
    assert!(
        matches!(&reason, RejectionReason::Require { name: Some(n), .. }
            if n == "the_selected_source_exists"),
        "{reason:?}"
    );
}

/// A bare admission act - the shape a buggy or hostile embedder-side
/// path would take. The invariant, not the gate, is what makes the
/// wrong figure uncommittable.
fn bare_line() -> morpholog_core::Transformation {
    use morpholog_core::ir_builder::{assert_, params, transformation, var};
    transformation(
        "bare_line",
        params(&["line", "charge", "meter", "proposed", "applied"]),
        vec![assert_(
            "ChargeLine",
            vec![
                var("line"),
                var("charge"),
                var("meter"),
                var("proposed"),
                var("applied"),
            ],
        )],
    )
}

#[test]
fn the_wrong_sources_figure_is_refused_by_name_in_both_directions() {
    // A metered charge wearing the caller's figure...
    let state = declared("consumption", "meter");
    let state = ex().must_accept(
        &scoped_charges::read_meter(),
        vec![subj("m1"), dec(431)],
        state,
    );
    let reason = ex().must_reject(
        &bare_line(),
        vec![
            subj("l1"),
            subj("consumption"),
            subj("m1"),
            dec(999),
            dec(999),
        ],
        &state,
    );
    assert!(
        rejected_by(&reason, "applied_quantity_follows_the_declared_source"),
        "{reason:?}"
    );
    // ...and a caller-sourced charge wearing a foreign figure.
    let state = declared("standing", "caller");
    let reason = ex().must_reject(
        &bare_line(),
        vec![subj("l1"), subj("standing"), subj("m1"), dec(42), dec(431)],
        &state,
    );
    assert!(
        rejected_by(&reason, "applied_quantity_follows_the_declared_source"),
        "{reason:?}"
    );
}

#[test]
fn a_line_for_an_undeclared_charge_is_refused() {
    let reason = ex().must_reject(
        &bare_line(),
        vec![subj("l1"), subj("ghost"), subj("m1"), dec(1), dec(1)],
        &State::default(),
    );
    assert!(
        rejected_by(&reason, "lines_name_a_declared_charge"),
        "{reason:?}"
    );
}

#[test]
fn a_source_tag_the_rules_have_no_branch_for_is_refused() {
    let reason = ex().must_reject(
        &scoped_charges::declare_charge(),
        vec![subj("consumption"), subj("vibes")],
        &State::default(),
    );
    assert!(
        rejected_by(&reason, "sources_are_the_known_two"),
        "{reason:?}"
    );
}

#[test]
fn a_meters_reading_cannot_be_quietly_restated() {
    // The applied figure's meaning rests on the reading standing
    // still: a second, different reading for the same meter collides
    // on the uniqueness discipline.
    let state = declared("consumption", "meter");
    let state = ex().must_accept(
        &scoped_charges::read_meter(),
        vec![subj("m1"), dec(431)],
        state,
    );
    let reason = ex().must_reject(
        &scoped_charges::read_meter(),
        vec![subj("m1"), dec(500)],
        &state,
    );
    assert!(
        matches!(&reason, RejectionReason::Invariant { name, .. }
            if name.as_str().starts_with("meter_reading_unique_by")),
        "{reason:?}"
    );
}

#[test]
fn the_proposal_order_does_not_matter_for_the_selection() {
    // Reading first or declaration first - the same line commits with
    // the same applied figure.
    let mut state = State::default();
    state = ex().must_accept(
        &scoped_charges::read_meter(),
        vec![subj("m1"), dec(431)],
        state,
    );
    state = ex().must_accept(
        &scoped_charges::declare_charge(),
        vec![subj("consumption"), subj("meter")],
        state,
    );
    let outcome = ex()
        .propose(
            &scoped_charges::record_line(),
            vec![subj("l1"), subj("consumption"), subj("m1"), dec(1)],
            &state,
        )
        .expect("evaluates");
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
}
