//! `round(x, quantum)` semantics: the measured convention from the
//! billing probe - nearest multiple of the quantum, exact halves away
//! from zero - pinned as a table, negatives included (the naive
//! shift-and-truncate formula the node replaces was WRONG on
//! negatives, biasing them a penny upward). Plus the refusals: a
//! non-positive quantum by name, non-decimal operands as a type error.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{dec, eq, params, require, round, term, transformation};
use morpholog_core::{Outcome, Prop, State};
use morpholog_test_support::propose_with_test_actor;

fn holds(prop: Prop) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let outcome = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect("evaluates cleanly");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "expected the proposition to hold exactly: {outcome:?}"
    );
}

fn refuses(prop: Prop, fragments: &[&str]) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("expected a kernel error");
    let rendered = format!("{err}");
    for fragment in fragments {
        assert!(
            rendered.contains(fragment),
            "expected error containing {fragment:?}, got: {rendered}"
        );
    }
}

fn negated(value: &str) -> morpholog_core::ValueExpr {
    // The surface has no signed literals; a negative arrives as 0 - x,
    // exactly as a rule would spell it.
    morpholog_core::ir_builder::sub(term(dec("0")), term(dec(value)))
}

#[test]
fn rounds_to_the_nearest_penny() {
    for (raw, expected) in [
        ("2.344", "2.34"),
        ("2.346", "2.35"),
        ("2.30", "2.3"),
        ("0.004", "0"),
        ("0.006", "0.01"),
        ("1234.5678", "1234.57"),
    ] {
        holds(eq(
            round(term(dec(raw)), term(dec("0.01"))),
            term(dec(expected)),
        ));
    }
}

#[test]
fn exact_halves_round_away_from_zero() {
    for (raw, expected) in [("2.345", "2.35"), ("0.005", "0.01"), ("0.125", "0.13")] {
        holds(eq(
            round(term(dec(raw)), term(dec("0.01"))),
            term(dec(expected)),
        ));
    }
}

#[test]
fn negatives_mirror_positives_exactly() {
    // The probe's decisive cases: -1.234 must round to -1.23 (the
    // broken formula produced -1.22) and the half -1.235 goes AWAY
    // from zero to -1.24.
    for (raw, expected) in [
        ("1.234", "1.23"),
        ("1.236", "1.24"),
        ("1.235", "1.24"),
        ("0.005", "0.01"),
    ] {
        holds(eq(
            round(negated(raw), term(dec("0.01"))),
            negated(expected),
        ));
    }
}

#[test]
fn coarser_quanta_work_the_same_way() {
    for (raw, quantum, expected) in [
        ("2.37", "0.05", "2.35"),
        ("2.375", "0.05", "2.4"),
        ("12.5", "1", "13"),
        ("125", "50", "150"),
        ("2.345", "0.001", "2.345"),
    ] {
        holds(eq(
            round(term(dec(raw)), term(dec(quantum))),
            term(dec(expected)),
        ));
    }
}

#[test]
fn zero_quantum_is_refused_by_name() {
    refuses(
        eq(round(term(dec("1")), term(dec("0"))), term(dec("1"))),
        &["round quantum must be positive"],
    );
}

#[test]
fn negative_quantum_is_refused_by_name() {
    refuses(
        eq(round(term(dec("1")), negated("0.01")), term(dec("1"))),
        &["round quantum must be positive"],
    );
}

#[test]
fn non_decimal_operand_is_a_type_error() {
    refuses(
        eq(
            round(
                term(morpholog_core::ir_builder::qty("5", "USD")),
                term(dec("0.01")),
            ),
            term(dec("5")),
        ),
        &["round is defined on decimals"],
    );
}
