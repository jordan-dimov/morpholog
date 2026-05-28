//! Tests for the per-transformation argument-kind analysis - the
//! embedder-facing input contract. Companion to the worked-example
//! integration tests over `trade_lifecycle`; these pin the smaller
//! invariants that the example test will assume.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::*;
use morpholog_core::{
    AnalysisError, ParamKind, PredicateArgKind, TransformationName, Var, transformation_param_kinds,
};

/// **The regression test for the `Require` trap** (ChatGPT's
/// must-have). The static checker walks `Require` in a cloned scope,
/// so a parameter observed only inside a `require` is dropped from
/// the outer kind environment. A naive "re-run the checker's walk"
/// implementation of the param-kind accessor would report this
/// parameter as `Unconstrained`. Authority-gated transformations
/// (the `require can_approve(actor, asset)` shape) make this the
/// most-likely real failure mode.
#[test]
fn param_used_only_inside_require_resolves_to_concrete() {
    let prog = program("authority_test")
        .predicates(vec![
            predicate("authorised_for")
                .subject("principal")
                .subject("asset")
                .build(),
        ])
        .transformations(vec![transformation(
            "approve",
            params(&["principal", "asset"]),
            vec![require(claim(
                "authorised_for",
                vec![var("principal"), var("asset")],
            ))],
        )])
        .build();

    let kinds =
        transformation_param_kinds(&prog, &TransformationName::from("approve")).unwrap();

    assert_eq!(
        kinds,
        vec![
            (
                Var::from("principal"),
                ParamKind::Concrete(PredicateArgKind::Subject),
            ),
            (
                Var::from("asset"),
                ParamKind::Concrete(PredicateArgKind::Subject),
            ),
        ],
    );
}

/// Returned vec is in `transformation.parameters` declaration order,
/// never hash-iteration order. Picked param names whose alphabetical
/// order differs from declaration order, so a HashMap-iteration bug
/// would fail the test loudly.
#[test]
fn returned_order_matches_declaration_order() {
    let prog = program("ordering_test")
        .predicates(vec![
            predicate("triple")
                .subject("a")
                .decimal("b")
                .date("c")
                .build(),
        ])
        .transformations(vec![transformation(
            "act",
            params(&["zebra", "apple", "mango"]),
            vec![assert_(
                "triple",
                vec![var("zebra"), var("apple"), var("mango")],
            )],
        )])
        .build();

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("act")).unwrap();
    let names: Vec<&str> = kinds.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(names, vec!["zebra", "apple", "mango"]);
}

/// A parameter that flows only through `PredicateArgKind::Any` slots
/// resolves to `Polymorphic` - distinct from both `Concrete(Any)` and
/// `Unconstrained`. The embedder accepts input but cannot narrow it.
#[test]
fn param_observed_only_at_any_slot_is_polymorphic() {
    let prog = program("polymorphic_test")
        .predicates(vec![predicate("audit").any("payload").build()])
        .transformations(vec![transformation(
            "log",
            params(&["payload"]),
            vec![assert_("audit", vec![var("payload")])],
        )])
        .build();

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("log")).unwrap();
    assert_eq!(kinds, vec![(Var::from("payload"), ParamKind::Polymorphic)]);
}

/// A parameter never observed in any kind-bearing position resolves
/// to `Unconstrained` - the modelling-smell signal.
#[test]
fn param_never_observed_is_unconstrained() {
    let prog = program("dead_param_test")
        .transformations(vec![transformation(
            "noop",
            params(&["unused"]),
            vec![],
        )])
        .build();

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("noop")).unwrap();
    assert_eq!(
        kinds,
        vec![(Var::from("unused"), ParamKind::Unconstrained)],
    );
}

/// Each declared concrete kind (Subject / Decimal / Date / Bool /
/// Collection) round-trips through the accessor. Coverage of the
/// kind-mapping path that the schema layer will lean on.
#[test]
fn every_concrete_kind_round_trips() {
    let prog = program("kinds_test")
        .predicates(vec![
            predicate("subj_pred").subject("x").build(),
            predicate("dec_pred").decimal("x").build(),
            predicate("date_pred").date("x").build(),
            predicate("bool_pred").boolean("x").build(),
            predicate("coll_pred").collection("x").build(),
        ])
        .transformations(vec![
            transformation(
                "with_subject",
                params(&["s"]),
                vec![assert_("subj_pred", vec![var("s")])],
            ),
            transformation(
                "with_decimal",
                params(&["d"]),
                vec![assert_("dec_pred", vec![var("d")])],
            ),
            transformation(
                "with_date",
                params(&["t"]),
                vec![assert_("date_pred", vec![var("t")])],
            ),
            transformation(
                "with_bool",
                params(&["b"]),
                vec![assert_("bool_pred", vec![var("b")])],
            ),
            transformation(
                "with_collection",
                params(&["c"]),
                vec![assert_("coll_pred", vec![var("c")])],
            ),
        ])
        .build();

    let cases = [
        ("with_subject", "s", PredicateArgKind::Subject),
        ("with_decimal", "d", PredicateArgKind::Decimal),
        ("with_date", "t", PredicateArgKind::Date),
        ("with_bool", "b", PredicateArgKind::Bool),
        ("with_collection", "c", PredicateArgKind::Collection),
    ];
    for (transformation_name, param, expected) in cases {
        let kinds =
            transformation_param_kinds(&prog, &TransformationName::from(transformation_name))
                .unwrap();
        assert_eq!(
            kinds,
            vec![(Var::from(param), ParamKind::Concrete(expected))],
            "transformation `{transformation_name}` should resolve `{param}` to {expected:?}",
        );
    }
}

/// Unknown transformation name returns the typed error rather than a
/// guess.
#[test]
fn unknown_transformation_returns_error() {
    let prog = program("unknown_test")
        .transformations(vec![transformation("declared", params(&[]), vec![])])
        .build();

    let err = transformation_param_kinds(&prog, &TransformationName::from("ghost"))
        .expect_err("expected UnknownTransformation");
    match err {
        AnalysisError::UnknownTransformation { name } => {
            assert_eq!(name.as_str(), "ghost");
        }
        other => panic!("expected UnknownTransformation, got {other:?}"),
    }
}

/// An invalid programme surfaces its validation errors rather than
/// being analysed best-effort. The accessor refuses to guess past
/// problems the kernel itself would refuse to run.
#[test]
fn invalid_program_bubbles_validation_errors() {
    let prog = program("invalid_test")
        .transformations(vec![transformation(
            "broken",
            params(&[]),
            vec![assert_("undeclared_predicate", vec![])],
        )])
        .build();

    let err = transformation_param_kinds(&prog, &TransformationName::from("broken"))
        .expect_err("expected ProgramInvalid");
    match err {
        AnalysisError::ProgramInvalid(errors) => {
            assert!(
                !errors.is_empty(),
                "ProgramInvalid should carry at least one validation error",
            );
        }
        other => panic!("expected ProgramInvalid, got {other:?}"),
    }
}
