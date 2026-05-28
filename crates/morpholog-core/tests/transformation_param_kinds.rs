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

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("approve")).unwrap();

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
        .transformations(vec![transformation("noop", params(&["unused"]), vec![])])
        .build();

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("noop")).unwrap();
    assert_eq!(kinds, vec![(Var::from("unused"), ParamKind::Unconstrained)],);
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

/// **The regression test for silent-conflict-dropping** (the
/// blocking issue from ChatGPT's review and Copilot's comment on
/// `observe`). The static checker walks `Or` branches in cloned
/// scopes whose refinements do not export, so a programme can
/// validate even when the same parameter is observed at one
/// concrete kind in one branch and a different concrete kind in
/// another. An earlier implementation refined into one
/// `InferredKind` per variable and silently dropped the second
/// observation on conflict, which produced a `Concrete(_)` result
/// that lied about the contract. The fix accumulates a *set* of
/// observed kinds per variable and projects multi-kind sets to
/// `Ambiguous`. The disjunctive shape is legitimate at runtime
/// (the runtime picks the `Or` branch that matches the actual
/// input), so the embedder needs the disjunctive contract, not a
/// false narrowing or a hard error.
#[test]
fn param_observed_in_different_kinds_across_or_branches_is_ambiguous() {
    let prog = program("ambiguous_test")
        .predicates(vec![
            predicate("by_decimal").decimal("d").build(),
            predicate("by_subject").subject("s").build(),
        ])
        .transformations(vec![transformation(
            "either_shape",
            params(&["x"]),
            vec![require(or(vec![
                claim("by_decimal", vec![var("x")]),
                claim("by_subject", vec![var("x")]),
            ]))],
        )])
        .build();

    let kinds =
        transformation_param_kinds(&prog, &TransformationName::from("either_shape")).unwrap();

    let (param, kind) = &kinds[0];
    assert_eq!(param, &Var::from("x"));
    match kind {
        ParamKind::Ambiguous(observed) => {
            assert_eq!(
                observed,
                &vec![PredicateArgKind::Subject, PredicateArgKind::Decimal],
                "Ambiguous must carry the distinct observed kinds in PredicateArgKind \
                 declaration order (Subject, Decimal, Date, Bool, Collection, Any)",
            );
        }
        other => panic!(
            "expected Ambiguous([Subject, Decimal]) for an Or-of-different-kind-slots; \
             got {other:?}. This is the silent-conflict-dropping bug.",
        ),
    }
}

/// **The regression test for the let-alias bug** (Copilot's
/// comment). `let amt = amount; admit Payment(amt)` - the
/// externally-supplied parameter is `amount`, but the
/// kind-bearing observation lands on `amt`. Without alias
/// tracking the param would resolve to `Unconstrained` and the
/// embedder would learn nothing about a parameter the model is
/// actually using.
#[test]
fn param_aliased_through_let_inherits_the_aliased_observation() {
    let prog = program("alias_test")
        .predicates(vec![predicate("payment").decimal("p").build()])
        .transformations(vec![transformation(
            "process",
            params(&["amount"]),
            vec![
                let_("amt", term(var("amount"))),
                assert_("payment", vec![var("amt")]),
            ],
        )])
        .build();

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("process")).unwrap();
    assert_eq!(
        kinds,
        vec![(
            Var::from("amount"),
            ParamKind::Concrete(PredicateArgKind::Decimal),
        )],
    );
}

/// Alias propagation can ALSO create ambiguity (ChatGPT's
/// nuance): if `x` and `y` are aliased and one is observed at
/// Decimal while the other is observed at Subject, the
/// equivalence-class projection naturally unions the two
/// observations and the parameter falls out as `Ambiguous`. The
/// alias chain does not hide branch-level disagreement.
#[test]
fn param_aliased_to_disagreeing_observations_is_ambiguous() {
    let prog = program("alias_ambiguous_test")
        .predicates(vec![
            predicate("by_decimal").decimal("d").build(),
            predicate("by_subject").subject("s").build(),
        ])
        .transformations(vec![transformation(
            "diverge",
            params(&["x"]),
            vec![
                let_("y", term(var("x"))),
                require(or(vec![
                    claim("by_decimal", vec![var("x")]),
                    claim("by_subject", vec![var("y")]),
                ])),
            ],
        )])
        .build();

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("diverge")).unwrap();
    match &kinds[0].1 {
        ParamKind::Ambiguous(observed) => {
            assert_eq!(
                observed,
                &vec![PredicateArgKind::Subject, PredicateArgKind::Decimal],
            );
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

/// Alias expansion must not affect declaration-order projection:
/// the returned vec carries parameters in `transformation.parameters`
/// order, never alias-iteration order or HashMap-iteration order.
/// ChatGPT explicitly requested this test - without it, a refactor
/// could quietly start returning alias names or scrambled order
/// without breaking other tests.
#[test]
fn aliased_params_preserve_declaration_order() {
    let prog = program("alias_order_test")
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
            vec![
                let_("z", term(var("zebra"))),
                let_("a", term(var("apple"))),
                let_("m", term(var("mango"))),
                assert_("triple", vec![var("z"), var("a"), var("m")]),
            ],
        )])
        .build();

    let kinds = transformation_param_kinds(&prog, &TransformationName::from("act")).unwrap();
    let names: Vec<&str> = kinds.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(
        names,
        vec!["zebra", "apple", "mango"],
        "declaration order must survive alias expansion",
    );
    let kind_only: Vec<&ParamKind> = kinds.iter().map(|(_, k)| k).collect();
    assert_eq!(
        kind_only,
        vec![
            &ParamKind::Concrete(PredicateArgKind::Subject),
            &ParamKind::Concrete(PredicateArgKind::Decimal),
            &ParamKind::Concrete(PredicateArgKind::Date),
        ],
    );
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
