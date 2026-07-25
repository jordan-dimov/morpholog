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
fn an_exact_multiple_beyond_the_count_range_still_rounds() {
    // round(8, 1e-28) is exactly 8, but the COUNT of quanta (8e28)
    // exceeds the decimal range - the quotient formulation panics on
    // this input; the remainder formulation answers it.
    holds(eq(
        round(term(dec("8")), term(dec("0.0000000000000000000000000001"))),
        term(dec("8")),
    ));
}

#[test]
fn a_result_outside_the_decimal_range_is_a_named_error_not_a_panic() {
    // Decimal::MAX ends in ...335; the nearest multiple of 10 is the
    // half case, which rounds away from zero to past MAX. The exact
    // answer is unrepresentable, so the kernel refuses by name.
    refuses(
        eq(
            round(term(dec("79228162514264337593543950335")), term(dec("10"))),
            term(dec("0")),
        ),
        &["round out of decimal range"],
    );
}

#[test]
fn a_literal_zero_quantum_is_refused_at_validation() {
    use morpholog_core::ir_builder::{claim, implies, invariant, predicate, program, var};
    let p = program("q_zero")
        .predicates(vec![predicate("P").decimal("a").build()])
        .invariants(vec![invariant(
            "r",
            implies(
                claim("P", vec![var("a")]),
                eq(round(term(var("a")), term(dec("0"))), term(var("a"))),
            ),
        )])
        .build();
    let errs = p
        .validate()
        .expect_err("literal zero quantum must fail validation");
    assert!(
        errs.iter().any(|e| e
            .to_string()
            .contains("round quantum must be a positive decimal")),
        "expected the authoring-time refusal, got: {errs:?}"
    );
}

#[test]
fn a_variable_quantum_passes_validation_and_is_refused_at_runtime() {
    use morpholog_core::ir_builder::{invariant, params, program, transformation, var};
    // Statically fine: the quantum arrives through a parameter.
    let t = transformation(
        "probe",
        params(&["q"]),
        vec![require(eq(
            round(term(dec("1")), term(var("q"))),
            term(dec("1")),
        ))],
    );
    let p = program("q_var")
        .invariants(vec![invariant("noop", eq(term(dec("1")), term(dec("1"))))])
        .transformations(vec![t.clone()])
        .build();
    p.validate().expect("a variable quantum is statically fine");
    // At runtime, a zero arriving through the variable is the backstop.
    let err = propose_with_test_actor(
        &t,
        vec![morpholog_test_support::dec(0)],
        &State::default(),
        &[],
        &[],
    )
    .expect_err("zero through a variable must be the runtime refusal");
    assert!(
        format!("{err}").contains("round quantum must be positive"),
        "got: {err}"
    );
}

#[test]
fn an_any_declared_slot_refines_through_round() {
    use morpholog_core::ir_builder::{claim, implies, invariant, predicate, program, var};
    // Any is unconstrained and refines at a concrete use - round must
    // follow the checker's doctrine, not reject the flow.
    let p = program("any_flow")
        .predicates(vec![predicate("Box").any("x").build()])
        .invariants(vec![invariant(
            "rounded_box",
            implies(
                claim("Box", vec![var("x")]),
                eq(round(term(var("x")), term(dec("0.01"))), term(var("x"))),
            ),
        )])
        .build();
    p.validate()
        .expect("an Any slot flowing into round must validate");
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
