//! Deliberately adversarial tests for defined propositions: IR shapes
//! the parser can never produce, constructed with `ir_builder` to pin
//! the kernel's own floors. The business-level scenarios live in
//! `morpholog-examples/tests/definitions.rs`; this layer is "the real
//! shape minus the guarantee" - capture attempts, unresolved calls,
//! missing definitions, and the expanded-depth budget.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    and, claim, defined, definition, eq, invariant, params, predicate, program, require, term,
    transformation, var,
};
use morpholog_core::{
    EvalError, State, Subject, Transition, ValidationError, propose, resolve_defined_calls,
};
use morpholog_test_support::{dec, subj, test_transition};

// Hygiene is the contract: a definition body sees ONLY its parameters.
// A body smuggling a reference to a caller-scope name (never possible
// from the surface, where validation refuses it) must surface the
// unbound name at evaluation - not silently capture the caller's value.
#[test]
fn a_body_referencing_caller_scope_errors_instead_of_capturing() {
    // define leaky(x): x = limit   -- `limit` is NOT a parameter.
    let leaky = definition(
        "leaky",
        params(&["x"]),
        eq(term(var("x")), term(var("limit"))),
    );
    // check_limit(limit): require leaky(limit) -- the caller HAS a
    // bound `limit`; the body must not see it.
    let t = transformation(
        "check_limit",
        params(&["limit"]),
        vec![require(defined("leaky", vec![var("limit")]))],
    );
    let transition = test_transition(&t, vec![dec(7)]);
    let err = propose(&t, &transition, &State::default(), &[], &[leaky])
        .expect_err("the body's free name must error, not capture");
    assert!(
        matches!(&err, EvalError::UnboundVariable(name) if name == "limit"),
        "got {err:?}"
    );
}

// A call whose definition is absent from the evaluation context is a
// programme-integrity error with its own name - unlike an unmatched
// predicate, which lawfully matches nothing.
#[test]
fn a_call_without_its_definition_is_a_distinct_kernel_error() {
    let t = transformation(
        "act",
        params(&["x"]),
        vec![require(defined("vanished", vec![var("x")]))],
    );
    let transition = test_transition(&t, vec![subj("s")]);
    let err = propose(&t, &transition, &State::default(), &[], &[])
        .expect_err("a dangling call must error");
    assert!(
        matches!(err, EvalError::UnknownDefinition(_)),
        "got {err:?}"
    );
}

// Hand-built IR that spells a call as a claim (skipping resolution)
// fails validation loudly, with guidance - never a misleading
// undeclared-predicate error, never silent acceptance.
#[test]
fn a_claim_naming_a_definition_is_an_unresolved_call_error() {
    let p = program("unresolved")
        .predicates(vec![predicate("Thing").subject("x").build()])
        .definitions(vec![definition(
            "thing_exists",
            params(&["x"]),
            claim("Thing", vec![var("x")]),
        )])
        .invariants(vec![invariant(
            "things_exist",
            claim("thing_exists", vec![var("x")]),
        )])
        .build();
    let errors = p.validate().expect_err("must fail validation");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::UnresolvedDefinitionCall { .. })),
        "got {errors:?}"
    );
}

// `resolve_defined_calls` rewrites exactly the claim-shaped references
// that name a definition, and the result validates and proposes - the
// hand-built-IR route to the same programme the parser would build.
#[test]
fn resolve_rewrites_claim_shaped_calls_and_the_programme_runs() {
    let mut p = program("resolved")
        .predicates(vec![
            predicate("Thing").subject("x").build(),
            predicate("Seen").subject("x").build(),
        ])
        .definitions(vec![definition(
            "thing_exists",
            params(&["x"]),
            claim("Thing", vec![var("x")]),
        )])
        .transformations(vec![
            transformation(
                "put",
                params(&["x"]),
                vec![morpholog_core::ir_builder::assert_("Thing", vec![var("x")])],
            ),
            transformation(
                "see",
                params(&["x"]),
                vec![
                    require(and(vec![claim("thing_exists", vec![var("x")])])),
                    morpholog_core::ir_builder::assert_("Seen", vec![var("x")]),
                ],
            ),
        ])
        .build();
    resolve_defined_calls(&mut p);
    p.validate().expect("resolved programme validates");

    let put = p.transformation("put").unwrap();
    let state = match propose(
        put,
        &test_transition(put, vec![subj("a")]),
        &State::default(),
        &p.invariants,
        &p.definitions,
    )
    .unwrap()
    {
        morpholog_core::Outcome::Accepted {
            candidate_state, ..
        } => candidate_state,
        other @ morpholog_core::Outcome::Rejected { .. } => {
            panic!("put should commit, got {other:?}")
        }
    };
    let see = p.transformation("see").unwrap();
    let outcome = propose(
        see,
        &test_transition(see, vec![subj("a")]),
        &state,
        &p.invariants,
        &p.definitions,
    )
    .unwrap();
    assert!(matches!(outcome, morpholog_core::Outcome::Accepted { .. }));
}

// The depth budget charges a call its callee's expanded depth: a chain
// of shallow definitions whose expansion nests past the limit is
// refused at validation, exactly like one deep body - the guarantee
// that `propose` never recurses past the floor on validated IR.
#[test]
fn a_definition_chain_expanding_past_the_depth_limit_is_refused() {
    let mut definitions = vec![definition(
        "wrap_0",
        params(&["x"]),
        claim("Thing", vec![var("x")]),
    )];
    for i in 1..300 {
        definitions.push(definition(
            &format!("wrap_{i}"),
            params(&["x"]),
            morpholog_core::ir_builder::not(defined(&format!("wrap_{}", i - 1), vec![var("x")])),
        ));
    }
    let p = program("deep_chain")
        .predicates(vec![predicate("Thing").subject("x").build()])
        .definitions(definitions)
        .build();
    let errors = p
        .validate()
        .expect_err("expanded depth must trip the budget");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::NestingTooDeep { .. })),
        "got {errors:?}"
    );
}

// The actor stays a call-site concern: passed as an argument it
// resolves against the proposing transition before the body runs, so
// an authority-shaped definition works from a gate without the body
// ever touching transition context.
#[test]
fn actor_as_a_call_argument_resolves_at_the_call_site() {
    let may_act = definition(
        "may_act",
        params(&["person"]),
        claim("MayAct", vec![var("person")]),
    );
    let t = transformation(
        "act",
        params(&[]),
        vec![require(defined(
            "may_act",
            vec![morpholog_core::ir_builder::actor()],
        ))],
    );
    let granted = State::from_claims(vec![morpholog_core::ClaimInstance {
        predicate: "MayAct".into(),
        args: vec![morpholog_core::EvalValue::Subject(Subject::from("anna"))],
    }]);
    let allowed = propose(
        &t,
        &Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: Subject::from("anna"),
        },
        &granted,
        &[],
        std::slice::from_ref(&may_act),
    )
    .unwrap();
    assert!(matches!(allowed, morpholog_core::Outcome::Accepted { .. }));
    let refused = propose(
        &t,
        &Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: Subject::from("boris"),
        },
        &granted,
        &[],
        &[may_act],
    )
    .unwrap();
    assert!(matches!(refused, morpholog_core::Outcome::Rejected { .. }));
}
