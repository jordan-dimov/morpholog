//! Implication and forall answer false only after every binding has
//! evaluated: an evaluation error at any binding is the result, so the
//! verdict never depends on which binding the state presents first. A
//! cap that one bucket breaks while another bucket's total leaves the
//! decimal range must report the range error under both claim orders.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    claim, forall, implies, invariant, le, predicate, program, sum, term, transformation, var,
};
use morpholog_core::{EvalError, EvalValue, Program, Prop, State, eval_invariant};
use morpholog_test_support::{claim_instance, dec_str, subj};
use rust_decimal::Decimal;

fn capped(body: Prop) -> Program {
    program("error_over_false")
        .predicates(vec![
            predicate("Cap").subject("b").decimal("cap").build(),
            predicate("Item").subject("b").decimal("v").build(),
        ])
        .invariants(vec![invariant("capped", body)])
        .transformations(vec![transformation(
            "probe",
            morpholog_core::ir_builder::params(&[]),
            vec![],
        )])
        .build()
}

fn total_within_cap() -> Prop {
    le(
        sum(term(var("v")), claim("Item", vec![var("b"), var("v")])),
        term(var("cap")),
    )
}

/// One bucket over its cap, another whose total no decimal can hold.
fn mixed_claims() -> Vec<morpholog_core::ClaimInstance> {
    vec![
        claim_instance("Cap", &[subj("broken"), dec_str("10")]),
        claim_instance("Item", &[subj("broken"), dec_str("20")]),
        claim_instance("Cap", &[subj("huge"), dec_str("0")]),
        claim_instance("Item", &[subj("huge"), EvalValue::Decimal(Decimal::MAX)]),
        claim_instance("Item", &[subj("huge"), dec_str("1")]),
    ]
}

fn assert_range_error_under_both_orders(program: &Program) {
    let inv = &program.invariants[0];
    let forward = mixed_claims();
    let mut backward = mixed_claims();
    backward.reverse();
    for (label, claims) in [("forward", forward), ("backward", backward)] {
        let state = State::from_claims(claims);
        let verdict = eval_invariant(inv, &state, None, &[]);
        assert!(
            matches!(verdict, Err(EvalError::ArithOutOfRange(_))),
            "{label} order: the range error must dominate the violation, got {verdict:?}"
        );
        assert_eq!(
            verdict.unwrap_err().to_string(),
            EvalError::sum_out_of_decimal_range().to_string()
        );
    }
}

#[test]
fn an_implication_reports_the_error_whichever_binding_comes_first() {
    let p = capped(implies(
        claim("Cap", vec![var("b"), var("cap")]),
        total_within_cap(),
    ));
    p.validate().expect("valid");
    assert_range_error_under_both_orders(&p);
}

#[test]
fn a_forall_reports_the_error_whichever_binding_comes_first() {
    let p = capped(forall(
        "b",
        claim("Cap", vec![var("b"), var("cap")]),
        total_within_cap(),
    ));
    p.validate().expect("valid");
    assert_range_error_under_both_orders(&p);
}

#[test]
fn a_violation_alone_is_still_a_violation() {
    let p = capped(implies(
        claim("Cap", vec![var("b"), var("cap")]),
        total_within_cap(),
    ));
    let state = State::from_claims(vec![
        claim_instance("Cap", &[subj("broken"), dec_str("10")]),
        claim_instance("Item", &[subj("broken"), dec_str("20")]),
    ]);
    assert_eq!(eval_invariant(&p.invariants[0], &state, None, &[]), Ok(false));
}
