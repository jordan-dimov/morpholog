//! A name bound inside `not`, `implies`, `exists` or `forall` is gone
//! after it, and a quantifier's name is bound only where its body or
//! source binds it. The runtime gives such a name no value, so `check`
//! refuses its use rather than let the proposal fail with an unbound
//! variable. The acceptance tests propose too, so a shape `check` lets
//! through is shown to run.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{coll, must_accept, subj};
use morpholog_core::{Program, State, ValidationError};
use morpholog_surface::parse_program;

const PREDICATES: &str = "
predicate Listed(item: Subject)
predicate Seen(item: Subject)
predicate Ok(item: Subject)
predicate Noted(tag: Subject)

transformation list(x):
    admit Listed(x)

transformation see(x):
    admit Seen(x)
";

fn source(rest: &str) -> String {
    format!("program binding_scope\n{PREDICATES}\n{rest}")
}

fn unbound_names(rest: &str) -> Vec<String> {
    let errors = parse_program(&source(rest))
        .expect("programme should parse")
        .validate()
        .expect_err("programme should fail validation");
    errors
        .iter()
        .map(|e| match e {
            ValidationError::UnboundVariable { variable, .. } => variable.to_string(),
            other => panic!("expected only unbound variables, got {other}"),
        })
        .collect()
}

fn valid(rest: &str) -> Program {
    let program = parse_program(&source(rest)).expect("programme should parse");
    if let Err(errors) = program.validate() {
        panic!("programme should validate: {errors:?}");
    }
    program
}

fn seen(program: &Program, item: &str) -> State {
    must_accept(
        program.transformation("see").unwrap(),
        vec![subj(item)],
        State::default(),
        program,
    )
}

#[test]
fn a_name_bound_only_under_not_is_unbound_after_it() {
    let names = unbound_names(
        "
transformation note(tag):
    require not Seen(item) and item = tag
    admit Noted(tag)
",
    );
    assert_eq!(names, ["item"]);
}

#[test]
fn a_name_bound_only_in_an_implication_is_unbound_after_it() {
    let names = unbound_names(
        "
transformation note(tag):
    require (Seen(item) implies Ok(item)) and item = tag
    admit Noted(tag)
",
    );
    assert_eq!(names, ["item"]);
}

#[test]
fn a_name_bound_only_in_an_exists_is_unbound_after_it() {
    let names = unbound_names(
        "
transformation note(tag):
    require (exists item: Seen(item)) and item = tag
    admit Noted(tag)
",
    );
    assert_eq!(names, ["item"]);
}

#[test]
fn a_name_bound_only_in_a_forall_is_unbound_after_it() {
    let names = unbound_names(
        "
transformation note(tag, tags):
    require (forall item in tags: Seen(item)) and item = tag
    admit Noted(tag)
",
    );
    assert_eq!(names, ["item"]);
}

#[test]
fn declaring_an_exists_name_does_not_bind_it() {
    let names = unbound_names(
        "
transformation note(tag):
    require exists item: item = tag
    admit Noted(tag)
",
    );
    assert_eq!(names, ["item"]);
}

#[test]
fn a_forall_source_that_does_not_bind_the_name_leaves_it_unbound() {
    let names = unbound_names(
        "
transformation note(tag):
    require forall item in (Seen(other)): item = tag
    admit Noted(tag)
",
    );
    assert_eq!(names, ["item"]);
}

// The body binds `x` only under `not`, so a call must pass it bound.
#[test]
fn a_parameter_bound_only_under_not_must_arrive_bound() {
    let names = unbound_names(
        "
define unseen(x): not Seen(x)

transformation note(tag):
    require unseen(item)
    admit Noted(tag)
",
    );
    assert_eq!(names, ["item"]);
}

#[test]
fn a_name_bound_before_not_is_still_bound_inside_and_after_it() {
    let program = valid(
        "
transformation note(tag):
    require Listed(item) and not Seen(item) and item = tag
    admit Noted(tag)
",
    );
    let state = must_accept(
        program.transformation("list").unwrap(),
        vec![subj("a")],
        State::default(),
        &program,
    );
    must_accept(
        program.transformation("note").unwrap(),
        vec![subj("a")],
        state,
        &program,
    );
}

// `Seen(tag)` under `not` is the only place `tag` gets a kind, and a
// comparison after it must still meet that kind.
#[test]
fn kind_evidence_from_under_not_still_reaches_a_bound_name() {
    let errors = parse_program(&source(
        "
transformation note(tag):
    require not Seen(tag) and tag < 5
",
    ))
    .unwrap()
    .validate()
    .expect_err("a subject compared as a decimal");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::OperandKindMismatch { .. }
                | ValidationError::VariableKindConflict { .. }
        )),
        "{errors:?}"
    );
}

#[test]
fn a_name_bound_inside_a_quantifier_serves_its_own_body() {
    let program = valid(
        "
transformation note(tag, tags):
    require exists item: Seen(item) and item = tag
    require forall each in tags: Seen(each)
    admit Noted(tag)
",
    );
    let state = seen(&program, "a");
    must_accept(
        program.transformation("note").unwrap(),
        vec![subj("a"), coll(vec![subj("a")])],
        state,
        &program,
    );
}

#[test]
fn pre_still_passes_its_bindings_on() {
    let program = valid(
        "
transformation note(tag):
    admit Noted(tag)

invariant noted_was_seen_before:
    Noted(t) implies (pre(Seen(x)) and x = t)
",
    );
    let state = seen(&program, "a");
    must_accept(
        program.transformation("note").unwrap(),
        vec![subj("a")],
        state,
        &program,
    );
}

#[test]
fn a_definition_that_binds_its_parameter_still_hands_it_out() {
    let program = valid(
        "
define seen_one(x): Seen(x)
define unseen(x): not Seen(x)

transformation note(tag):
    require seen_one(item) and item = tag
    admit Noted(tag)

transformation note_new(tag):
    require unseen(tag)
    admit Noted(tag)
",
    );
    let state = seen(&program, "a");
    let state = must_accept(
        program.transformation("note").unwrap(),
        vec![subj("a")],
        state,
        &program,
    );
    must_accept(
        program.transformation("note_new").unwrap(),
        vec![subj("b")],
        state,
        &program,
    );
}
