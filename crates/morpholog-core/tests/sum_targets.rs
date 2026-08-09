//! The expression-valued sum target's static floor: the target
//! consumes bindings - the body's, or the surrounding context's - and
//! never introduces them, so a variable nothing binds is refused at
//! authoring time, while an outer binding is visible inside the
//! target exactly as it is inside the body.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    and, claim, cond, dec, duration_le, invariant, le, mul, predicate, program, qty, sum, term,
    transformation, value_of, var, wildcard,
};
use morpholog_core::{Outcome, State, Term, ValidationError, lower_sum_seeds};
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

/// The empty sum's typed zero, for each target shape whose kind is
/// static knowledge the seed pass can reach: a `value` lookup, a
/// conditional with agreeing branches, a nested aggregate. Each
/// programme compares the empty sum against a quantity or duration,
/// which only a typed seed satisfies - the decimal default would be a
/// kernel type error, not a wrong answer.
#[test]
fn an_empty_sum_over_a_lookup_target_has_the_lookups_typed_zero() {
    let mut p = program("lookup_seed")
        .predicates(vec![
            predicate("Item").subject("i").build(),
            predicate("Fee").quantity("usd", "USD").build(),
            predicate("Cap").quantity("cap", "USD").build(),
        ])
        .invariants(vec![invariant(
            "capped",
            morpholog_core::ir_builder::implies(
                claim("Cap", vec![var("cap")]),
                le(
                    sum(
                        value_of("Fee", vec![wildcard()]),
                        claim("Item", vec![var("i")]),
                    ),
                    term(var("cap")),
                ),
            ),
        )])
        .transformations(vec![transformation(
            "probe",
            morpholog_core::ir_builder::params(&[]),
            vec![],
        )])
        .build();
    lower_sum_seeds(&mut p);
    p.validate()
        .expect("the lookup's declared kind types the seed");
    // No Item rows: the sum is its seed, and 0 USD fits the cap.
    let state = State::from_claims(vec![claim_instance(
        "Cap",
        &[morpholog_test_support::qty("5", "USD")],
    )]);
    let outcome =
        propose_with_test_actor(&p.transformations[0], vec![], &state, &p.invariants, &[])
            .expect("the empty sum is a typed zero, not a type error");
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
}

#[test]
fn an_empty_sum_over_a_conditional_target_has_the_branches_agreed_zero() {
    let mut p = program("cond_seed")
        .predicates(vec![
            predicate("Item").subject("i").build(),
            predicate("Rush").subject("i").build(),
            predicate("Cap").quantity("cap", "t").build(),
        ])
        .invariants(vec![invariant(
            "capped",
            morpholog_core::ir_builder::implies(
                claim("Cap", vec![var("cap")]),
                le(
                    sum(
                        cond(
                            claim("Rush", vec![var("i")]),
                            term(qty("2", "t")),
                            term(qty("1", "t")),
                        ),
                        claim("Item", vec![var("i")]),
                    ),
                    term(var("cap")),
                ),
            ),
        )])
        .transformations(vec![transformation(
            "probe",
            morpholog_core::ir_builder::params(&[]),
            vec![],
        )])
        .build();
    lower_sum_seeds(&mut p);
    p.validate().expect("agreeing branch kinds type the seed");
    let state = State::from_claims(vec![claim_instance(
        "Cap",
        &[morpholog_test_support::qty("5", "t")],
    )]);
    let outcome =
        propose_with_test_actor(&p.transformations[0], vec![], &state, &p.invariants, &[])
            .expect("the empty sum is a typed zero, not a type error");
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
}

#[test]
fn an_empty_sum_over_a_nested_sum_target_has_the_inner_sums_zero() {
    let mut p = program("nested_seed")
        .predicates(vec![
            predicate("Voyage").subject("v").build(),
            predicate("Leg").subject("v").duration("d").build(),
        ])
        .invariants(vec![invariant(
            "capped",
            duration_le(
                sum(
                    sum(var("d"), claim("Leg", vec![var("v"), var("d")])),
                    claim("Voyage", vec![var("v")]),
                ),
                term(duration_term("PT48H")),
            ),
        )])
        .transformations(vec![transformation(
            "probe",
            morpholog_core::ir_builder::params(&[]),
            vec![],
        )])
        .build();
    lower_sum_seeds(&mut p);
    p.validate().expect("the inner sum's seed types the outer");
    let outcome = propose_with_test_actor(
        &p.transformations[0],
        vec![],
        &State::default(),
        &p.invariants,
        &[],
    )
    .expect("the empty sum is a typed zero, not a type error");
    assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
}

fn duration_term(s: &str) -> Term {
    morpholog_core::ir_builder::duration(s)
}

/// The residue the seed pass cannot reach - a target whose only kind
/// evidence is a binding OUTSIDE the sum - is refused at authoring
/// time, never handed a bare-decimal zero the first empty book would
/// detonate on.
#[test]
fn an_outer_bound_quantity_target_is_refused_not_mistyped() {
    let mut p = program("outer_target")
        .predicates(vec![
            predicate("Item").subject("i").build(),
            predicate("Cap").quantity("cap", "t").build(),
        ])
        .invariants(vec![invariant(
            "capped",
            morpholog_core::ir_builder::implies(
                claim("Cap", vec![var("cap")]),
                le(
                    sum(var("cap"), claim("Item", vec![var("i")])),
                    term(var("cap")),
                ),
            ),
        )])
        .build();
    lower_sum_seeds(&mut p);
    let errors = p.validate().expect_err("the empty case cannot be typed");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::EmptySumUntyped { .. })),
        "expected EmptySumUntyped, got {errors:?}"
    );
}

/// A wildcard is a claim-pattern mark, not a value: reaching one as an
/// arithmetic operand or a conditional branch is refused at authoring
/// time, where the old behaviour was a kernel `TypeMismatch` on the
/// first witness.
#[test]
fn a_wildcard_inside_a_sum_target_is_refused_at_authoring_time() {
    let arith_target = capped_program_with_target(mul(term(wildcard()), term(dec("2"))));
    let errors = arith_target.validate().expect_err("`_` is not a value");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::WildcardAsValue { .. })),
        "expected WildcardAsValue, got {errors:?}"
    );

    let cond_target = capped_program_with_target(cond(
        claim("Loss", vec![wildcard(), var("l")]),
        term(wildcard()),
        term(dec("1")),
    ));
    let errors = cond_target.validate().expect_err("`_` is not a value");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::WildcardAsValue { .. })),
        "expected WildcardAsValue, got {errors:?}"
    );
}

fn capped_program_with_target(target: morpholog_core::ValueExpr) -> morpholog_core::Program {
    program("wildcard_targets")
        .predicates(vec![
            predicate("Loss").subject("w").decimal("l").build(),
            predicate("Cap").decimal("c").build(),
        ])
        .invariants(vec![invariant(
            "capped",
            morpholog_core::ir_builder::implies(
                claim("Cap", vec![var("c")]),
                le(
                    sum(target, claim("Loss", vec![wildcard(), var("l")])),
                    term(var("c")),
                ),
            ),
        )])
        .build()
}
