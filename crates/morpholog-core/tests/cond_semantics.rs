//! `if(when, then, otherwise)` semantics, pinned as the floor the
//! implementation stands on: the exists-style test selects a branch,
//! the witnesses' bindings are discarded (require's non-export rule),
//! ONLY the selected branch evaluates (an error in the untaken branch
//! cannot surface - and a condition error propagates, never silently
//! selecting `otherwise`), and branch kinds unify with no ordering
//! requirement, so subject tags and booleans are lawful branch kinds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    assert_, claim, cond, dec, div, eq, invariant, let_, params, predicate, program, require, subj,
    term, transformation, value_of, var, wildcard,
};
use morpholog_core::{ClaimInstance, EvalValue, Outcome, Prop, State};
use morpholog_test_support::propose_with_test_actor;

fn holds_against(prop: Prop, state: &State) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let outcome = propose_with_test_actor(&t, vec![], state, &[], &[]).expect("evaluates cleanly");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "expected the proposition to hold: {outcome:?}"
    );
}

fn flag(name: &str) -> ClaimInstance {
    ClaimInstance {
        predicate: "Flag".into(),
        args: vec![EvalValue::Subject(name.into())],
    }
}

/// `Flag(#on)` as a condition against a state that has or lacks it.
fn when_flag() -> Prop {
    claim("Flag", vec![subj("on")])
}

#[test]
fn the_present_condition_selects_then_and_the_absent_one_otherwise() {
    let picked = cond(when_flag(), term(dec("1")), term(dec("2")));
    holds_against(
        eq(picked.clone(), term(dec("1"))),
        &State::from_claims(vec![flag("on")]),
    );
    holds_against(eq(picked, term(dec("2"))), &State::default());
}

#[test]
fn only_the_selected_branch_evaluates() {
    // A division by zero sits in the UNTAKEN branch each time; if the
    // conditional were eager, both cases would error instead of hold.
    let poison = div(term(dec("1")), term(dec("0")));
    holds_against(
        eq(
            cond(when_flag(), term(dec("7")), poison.clone()),
            term(dec("7")),
        ),
        &State::from_claims(vec![flag("on")]),
    );
    holds_against(
        eq(cond(when_flag(), poison, term(dec("9"))), term(dec("9"))),
        &State::default(),
    );
}

#[test]
fn an_untaken_defaultless_lookup_does_not_error() {
    // `value Missing(_)` with no matching claim is a kernel error when
    // evaluated; in the untaken branch it must never be.
    let dead_lookup = value_of("Missing", vec![wildcard()]);
    holds_against(
        eq(
            cond(when_flag(), term(dec("7")), dead_lookup),
            term(dec("7")),
        ),
        &State::from_claims(vec![flag("on")]),
    );
}

#[test]
fn a_condition_error_propagates_rather_than_selecting_otherwise() {
    // The condition compares a looked-up value that does not exist:
    // deciding the branch is impossible, and the kernel says so.
    let undecidable = eq(value_of("Missing", vec![wildcard()]), term(dec("1")));
    let t = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            cond(undecidable, term(dec("1")), term(dec("2"))),
            term(dec("2")),
        ))],
    );
    propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("an undecidable condition is an error, not an else");
}

#[test]
fn witnesses_bound_inside_the_condition_do_not_export() {
    // `Holds(x)` binds x inside the condition; using x in a branch
    // must be refused statically - require's non-export rule.
    let p = program("leaky")
        .predicates(vec![
            predicate("Holds").subject("x").build(),
            predicate("Out").subject("v").build(),
        ])
        .transformations(vec![transformation(
            "leak",
            params(&["seed"]),
            vec![
                let_(
                    "picked",
                    cond(
                        claim("Holds", vec![var("x")]),
                        term(var("x")),
                        term(var("seed")),
                    ),
                ),
                assert_("Out", vec![var("picked")]),
            ],
        )])
        .build();
    let errors = p.validate().expect_err("x must not escape the condition");
    assert!(
        errors.iter().any(|e| {
            let rendered = format!("{e}");
            rendered.contains("`x`") && rendered.contains("nothing binds it")
        }),
        "got: {errors:?}"
    );
}

#[test]
fn outer_bindings_are_visible_inside_the_condition() {
    // The condition's claim match must see the already-bound `who`,
    // narrowing the match rather than rebinding it.
    let state = State::from_claims(vec![
        ClaimInstance {
            predicate: "Member".into(),
            args: vec![EvalValue::Subject("alice".into())],
        },
        flag("on"),
    ]);
    let t = transformation(
        "probe",
        params(&["who"]),
        vec![require(eq(
            cond(
                claim("Member", vec![var("who")]),
                term(dec("1")),
                term(dec("2")),
            ),
            term(dec("1")),
        ))],
    );
    let accepted = propose_with_test_actor(
        &t,
        vec![EvalValue::Subject("alice".into())],
        &state,
        &[],
        &[],
    )
    .expect("evaluates");
    assert!(matches!(accepted, Outcome::Accepted { .. }));
    let rejected =
        propose_with_test_actor(&t, vec![EvalValue::Subject("bob".into())], &state, &[], &[])
            .expect("evaluates");
    assert!(
        matches!(rejected, Outcome::Rejected { .. }),
        "bob is not a member, so the condition is false and 1 != 2"
    );
}

#[test]
fn subject_branches_are_lawful() {
    // Selection is not ordering: a conditional over subject tags is
    // the whole point (the five-bodies collapse).
    holds_against(
        eq(
            cond(when_flag(), term(subj("meter")), term(subj("book"))),
            term(subj("meter")),
        ),
        &State::from_claims(vec![flag("on")]),
    );
}

#[test]
fn nested_conditionals_select_through_both_levels() {
    let inner = cond(
        claim("Flag", vec![subj("aux")]),
        term(dec("2")),
        term(dec("3")),
    );
    let outer = cond(when_flag(), term(dec("1")), inner);
    holds_against(
        eq(outer.clone(), term(dec("1"))),
        &State::from_claims(vec![flag("on")]),
    );
    holds_against(
        eq(outer.clone(), term(dec("2"))),
        &State::from_claims(vec![flag("aux")]),
    );
    holds_against(eq(outer, term(dec("3"))), &State::default());
}

#[test]
fn mismatched_branch_kinds_are_refused_by_name() {
    let p = program("mixed")
        .predicates(vec![
            predicate("Flag").subject("x").build(),
            predicate("Out").decimal("v").build(),
        ])
        .transformations(vec![transformation(
            "mixed",
            params(&[]),
            vec![
                let_(
                    "picked",
                    cond(when_flag(), term(subj("meter")), term(dec("100"))),
                ),
                assert_("Out", vec![var("picked")]),
            ],
        )])
        .build();
    let errors = p.validate().expect_err("subject vs decimal branches");
    assert!(
        errors
            .iter()
            .any(|e| format!("{e}").contains("branches of `if`")),
        "got: {errors:?}"
    );
}

#[test]
fn the_parameter_kind_walk_sees_evidence_inside_the_condition() {
    // A parameter whose ONLY kind evidence is a claim slot inside
    // `when` must still land on that kind: the flat parameter walk
    // descends into the condition. (Cross-BRANCH refinement, by
    // contrast, deliberately stays out of the flat walk - the same
    // conservative posture Eq operands take - so a parameter whose
    // only evidence is the other branch stays Unconstrained there;
    // the checker still refines it for validation.)
    let p = program("evidence")
        .predicates(vec![
            predicate("Member").subject("who").build(),
            predicate("Out").decimal("v").build(),
        ])
        .transformations(vec![transformation(
            "evidenced",
            params(&["who"]),
            vec![
                let_(
                    "picked",
                    cond(
                        claim("Member", vec![var("who")]),
                        term(dec("1")),
                        term(dec("2")),
                    ),
                ),
                assert_("Out", vec![var("picked")]),
            ],
        )])
        .build();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
    let compiled = morpholog_core::CompiledProgram::new(p).expect("compiles");
    let kinds =
        morpholog_core::transformation_param_kinds(&compiled.validated(), &"evidenced".into())
            .expect("kinds resolve");
    let (_, kind) = &kinds[0];
    assert!(
        format!("{kind:?}").contains("Subject"),
        "the condition's claim slot is the evidence; got {kind:?}"
    );
}

#[test]
fn a_defined_call_is_lawful_inside_the_condition() {
    use morpholog_core::ir_builder::{defined, definition};
    let defs = vec![definition(
        "is_on",
        params(&["x"]),
        claim("Flag", vec![var("x")]),
    )];
    let t = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            cond(
                defined("is_on", vec![subj("on")]),
                term(dec("1")),
                term(dec("2")),
            ),
            term(dec("1")),
        ))],
    );
    let outcome = propose_with_test_actor(
        &t,
        vec![],
        &State::from_claims(vec![flag("on")]),
        &[],
        &defs,
    )
    .expect("evaluates");
    assert!(matches!(outcome, Outcome::Accepted { .. }));
}

#[test]
fn the_conditional_round_trips_through_the_formatter() {
    let inv = invariant(
        "picked",
        eq(
            cond(when_flag(), term(subj("meter")), term(subj("book"))),
            term(subj("meter")),
        ),
    );
    let p = program("fmt")
        .predicates(vec![predicate("Flag").subject("x").build()])
        .invariants(vec![inv])
        .build();
    let rendered = morpholog_core::format::format_program(&p);
    assert!(
        rendered.contains("if(Flag(#on), #meter, #book)"),
        "{rendered}"
    );
}
