//! The expression-valued sum target's static floor: the target
//! consumes bindings - the body's, or the surrounding context's - and
//! never introduces them, so a variable nothing binds is refused at
//! authoring time, while an outer binding is visible inside the
//! target exactly as it is inside the body.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    and, claim, invariant, le, mul, predicate, program, sum, term, transformation, var, wildcard,
};
use morpholog_core::{Outcome, State, Term};
use morpholog_test_support::{claim_instance, dec_str, propose_with_test_actor, subj};

fn capped_program(target_factor: Term) -> morpholog_core::Program {
    program("sum_targets")
        .predicates(vec![
            predicate("Loss").subject("w").decimal("l").build(),
            predicate("Scale").decimal("k").build(),
            predicate("Cap").decimal("c").build(),
        ])
        .invariants(vec![invariant(
            "capped",
            morpholog_core::ir_builder::implies(
                and(vec![
                    claim("Cap", vec![var("c")]),
                    claim("Scale", vec![var("k")]),
                ]),
                le(
                    sum(
                        mul(term(var("l")), term(target_factor)),
                        claim("Loss", vec![wildcard(), var("l")]),
                    ),
                    term(var("c")),
                ),
            ),
        )])
        .transformations(vec![transformation(
            "probe",
            morpholog_core::ir_builder::params(&[]),
            vec![],
        )])
        .build()
}

#[test]
fn an_unbound_target_variable_is_refused_at_authoring_time() {
    let p = capped_program(var("ghost"));
    let errors = p.validate().expect_err("nothing binds `ghost`");
    assert!(
        errors.iter().any(|e| {
            let rendered = format!("{e}");
            rendered.contains("`ghost`") && rendered.contains("nothing binds it")
        }),
        "got: {errors:?}"
    );
}

#[test]
fn a_context_bound_variable_resolves_inside_the_target() {
    // `k` is bound by the invariant's antecedent, outside the sum: the
    // target sees it like any other outer binding. 2 * 3 = 6 fits a
    // cap of 10; the same rule refuses a cap of 1.
    let p = capped_program(var("k"));
    p.validate()
        .expect("the outer binding satisfies the target");
    let state = State::from_claims(vec![
        claim_instance("Loss", &[subj("w1"), dec_str("2")]),
        claim_instance("Scale", &[dec_str("3")]),
        claim_instance("Cap", &[dec_str("10")]),
    ]);
    let outcome =
        propose_with_test_actor(&p.transformations[0], vec![], &state, &p.invariants, &[])
            .expect("evaluates cleanly");
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");

    let tight = State::from_claims(vec![
        claim_instance("Loss", &[subj("w1"), dec_str("2")]),
        claim_instance("Scale", &[dec_str("3")]),
        claim_instance("Cap", &[dec_str("1")]),
    ]);
    let outcome =
        propose_with_test_actor(&p.transformations[0], vec![], &tight, &p.invariants, &[])
            .expect("evaluates cleanly");
    assert!(matches!(outcome, Outcome::Rejected { .. }), "{outcome:?}");
}
