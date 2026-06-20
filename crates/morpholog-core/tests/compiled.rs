//! Tests for `CompiledProgram` - the validated, indexed programme. The
//! fixture exercises every vocabulary (predicate, intent, definition,
//! invariant, transformation, derived claim) so each by-name accessor is
//! checked, alongside the validate-on-construct gate and the
//! `validated()` bridge to the analysis API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    assert_, claim, definition, implies, invariant, params, predicate, program, transformation, var,
};
use morpholog_core::{
    CompiledProgram, DerivedClaim, IntentDecl, IntentName, Program, TransformationName, Var,
};

/// A small but complete valid programme: a `Trade` claim, a derived
/// `TradeTotal` over it, an intent, a definition, an invariant, and the
/// `capture` transformation that admits a trade.
fn fixture() -> Program {
    program("fixture")
        .predicates(vec![
            predicate("Trade").subject("t").build(),
            predicate("TradeTotal").subject("t").build(),
        ])
        .intents(vec![IntentDecl {
            name: "Notified".into(),
            args: vec![],
        }])
        .definitions(vec![definition(
            "is_captured",
            params(&["t"]),
            claim("Trade", vec![var("t")]),
        )])
        .invariants(vec![invariant(
            "total_needs_trade",
            implies(
                claim("TradeTotal", vec![var("t")]),
                claim("Trade", vec![var("t")]),
            ),
        )])
        .transformations(vec![transformation(
            "capture",
            params(&["t"]),
            vec![assert_("Trade", vec![var("t")])],
        )])
        .derived_claims(vec![DerivedClaim {
            predicate: "TradeTotal".into(),
            keys: vec![Var::from("t")],
            values: vec![],
            domain: claim("Trade", vec![var("t")]),
        }])
        .build()
}

#[test]
fn new_validates_and_indexes_every_vocabulary() {
    let c = CompiledProgram::new(fixture()).expect("the fixture is valid");

    assert!(c.transformation(&"capture".into()).is_some());
    assert!(c.invariant(&"total_needs_trade".into()).is_some());
    assert!(c.definition(&"is_captured".into()).is_some());
    assert!(c.predicate(&"Trade".into()).is_some());
    assert!(c.predicate(&"TradeTotal".into()).is_some());
    assert!(c.intent(&"Notified".into()).is_some());
    // Derived claims are keyed by their output predicate.
    assert!(c.derived_claim(&"TradeTotal".into()).is_some());

    // The accessor returns the right item, not just any.
    assert_eq!(
        c.transformation(&"capture".into()).unwrap().name,
        TransformationName::from("capture")
    );
}

#[test]
fn an_unknown_name_is_none_for_every_accessor() {
    let c = CompiledProgram::new(fixture()).unwrap();
    assert!(c.transformation(&"nope".into()).is_none());
    assert!(c.invariant(&"nope".into()).is_none());
    assert!(c.definition(&"nope".into()).is_none());
    assert!(c.predicate(&"Nope".into()).is_none());
    assert!(c.intent(&"Nope".into()).is_none());
    assert!(c.derived_claim(&"Nope".into()).is_none());
}

#[test]
fn new_rejects_an_invalid_programme() {
    // A transformation that admits an undeclared predicate fails
    // validation, so constructing a CompiledProgram returns the same
    // error list `Program::validate` would.
    let invalid = program("bad")
        .transformations(vec![transformation(
            "t",
            params(&["x"]),
            vec![assert_("Undeclared", vec![var("x")])],
        )])
        .build();
    let err = CompiledProgram::new(invalid).expect_err("undeclared predicate is invalid");
    assert!(!err.is_empty());
}

#[test]
fn validated_bridges_to_the_analysis_api() {
    let c = CompiledProgram::new(fixture()).unwrap();
    let validated = c.validated();
    // The borrowed proof view points at the same owned programme.
    assert_eq!(validated.as_program(), c.program());
    // And it carries the validity guarantee the analysis surface needs:
    // intent_arg_schema accepts a ValidatedProgram and finds the intent.
    assert!(morpholog_core::intent_arg_schema(&validated, &IntentName::from("Notified")).is_some());
}
