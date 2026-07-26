//! Checked arithmetic: an out-of-range result anywhere in the value
//! grammar is the named `ArithOutOfRange` refusal, never a panic. The
//! plain rust_decimal operators panic on overflow - including
//! division, where a tiny divisor overflows the quotient - so every
//! arithmetic site routes through checked variants. Each refusal case
//! here is a well-typed input that panicked the kernel before this
//! suite existed; the remainder case pins the one operator with no
//! reachable overflow.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    add, claim, dec, div, eq, modulo, mul, params, qty, require, sub, sum, term, transformation,
    var,
};
use morpholog_core::{EvalError, Outcome, Prop, State};
use morpholog_test_support::{claim_instance, dec_str, propose_with_test_actor, subj};

const MAX: &str = "79228162514264337593543950335";
const TINY: &str = "0.0000000000000000000000000001";

fn refuses_out_of_range(prop: Prop) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("expected the named out-of-range refusal");
    assert!(
        matches!(err, EvalError::ArithOutOfRange(_)),
        "expected ArithOutOfRange, got: {err:?}"
    );
}

#[test]
fn multiplication_overflow_is_refused_by_name() {
    refuses_out_of_range(eq(mul(term(dec(MAX)), term(dec("2"))), term(dec("0"))));
}

#[test]
fn division_overflow_is_refused_by_name() {
    // The non-obvious one: a tiny divisor overflows the quotient even
    // though neither operand is extreme.
    refuses_out_of_range(eq(div(term(dec("8")), term(dec(TINY))), term(dec("0"))));
}

#[test]
fn addition_and_subtraction_overflow_are_refused_by_name() {
    refuses_out_of_range(eq(add(term(dec(MAX)), term(dec("1"))), term(dec("0"))));
    refuses_out_of_range(eq(
        sub(sub(term(dec("0")), term(dec(MAX))), term(dec("1"))),
        term(dec("0")),
    ));
}

#[test]
fn quantity_arithmetic_overflow_is_refused_by_name() {
    // Same-unit addition past the range, and a bare-decimal scale-up.
    refuses_out_of_range(eq(
        add(term(qty(MAX, "USD")), term(qty(MAX, "USD"))),
        term(qty("0", "USD")),
    ));
    refuses_out_of_range(eq(
        mul(term(qty(MAX, "USD")), term(dec("2"))),
        term(qty("0", "USD")),
    ));
}

#[test]
fn sum_accumulation_overflow_is_refused_by_name() {
    // The realistic route: no single absurd literal, just admitted
    // amounts whose running total leaves the range.
    let state = State::from_claims(vec![
        claim_instance("Position", &[subj("a"), dec_str(MAX)]),
        claim_instance("Position", &[subj("b"), dec_str("1")]),
    ]);
    let t = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            sum(var("amt"), claim("Position", vec![var("s"), var("amt")])),
            term(dec("0")),
        ))],
    );
    let err = propose_with_test_actor(&t, vec![], &state, &[], &[])
        .expect_err("the running total must refuse, not panic");
    assert!(
        matches!(err, EvalError::ArithOutOfRange(_)),
        "expected ArithOutOfRange, got: {err:?}"
    );
}

#[test]
fn in_range_extremes_still_evaluate_exactly() {
    // The contract's other half: checked arithmetic refuses nothing
    // that fits. MAX - MAX, MAX + 0, MAX * 1 are all lawful.
    let t = transformation(
        "probe",
        params(&[]),
        vec![require(morpholog_core::ir_builder::and(vec![
            eq(sub(term(dec(MAX)), term(dec(MAX))), term(dec("0"))),
            eq(add(term(dec(MAX)), term(dec("0"))), term(dec(MAX))),
            eq(mul(term(dec(MAX)), term(dec("1"))), term(dec(MAX))),
        ]))],
    );
    let outcome = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect("in-range extremes evaluate cleanly");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "expected acceptance: {outcome:?}"
    );
}

#[test]
fn remainder_has_no_reachable_overflow_and_stays_exact() {
    // Probed directly: rust_decimal's rem rescales internally without
    // overflow (MAX % 0.3 does not even panic unchecked), so no
    // refusal witness exists - the checked call in the kernel is
    // uniform defence, not a reachable arm. Pin the exactness instead.
    let t = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            modulo(term(dec(MAX)), term(dec("3.7"))),
            term(dec("1.6")),
        ))],
    );
    let outcome = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect("rem at the boundary evaluates cleanly");
    assert!(matches!(outcome, Outcome::Accepted { .. }));
}

#[test]
fn zero_divisors_keep_their_own_name() {
    // The refusal vocabulary stays precise: division by zero is not
    // an out-of-range condition.
    let t = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            div(term(dec("1")), term(dec("0"))),
            term(dec("0")),
        ))],
    );
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("zero divisor still errors");
    assert!(
        matches!(err, EvalError::DivisionByZero),
        "expected DivisionByZero, got: {err:?}"
    );
}
