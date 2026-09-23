//! Tests for the per-transformation argument-kind analysis, the input
//! contract embedders build against. The `trade_lifecycle` integration
//! tests rely on the smaller rules pinned here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::*;
use morpholog_core::{
    AnalysisError, ParamKind, PredicateArgKind, TransformationName, Var, transformation_param_kinds,
};

/// A parameter seen only inside a `require` still gets its kind. The
/// checker walks `require` in a scoped copy, so simply reusing its walk
/// would report `Unconstrained`. Authority gates
/// (`require can_approve(actor, asset)`) hit this most often.
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

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("approve"),
    )
    .unwrap();

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
/// never hash order. The names sort differently from their declaration
/// order so a HashMap bug fails here.
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

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("act"),
    )
    .unwrap();
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

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("log"),
    )
    .unwrap();
    assert_eq!(kinds, vec![(Var::from("payload"), ParamKind::Polymorphic)]);
}

/// A parameter never observed in any kind-bearing position resolves
/// to `Unconstrained` - the modelling-smell signal.
#[test]
fn param_never_observed_is_unconstrained() {
    let prog = program("dead_param_test")
        .transformations(vec![transformation("noop", params(&["unused"]), vec![])])
        .build();

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("noop"),
    )
    .unwrap();
    assert_eq!(kinds, vec![(Var::from("unused"), ParamKind::Unconstrained)],);
}

/// Each declared concrete kind (Subject / Decimal / Date / Bool /
/// Collection) round-trips through the accessor, as the schema layer
/// relies on.
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
        let kinds = transformation_param_kinds(
            &prog.validated().expect("test programme validates"),
            &TransformationName::from(transformation_name),
        )
        .unwrap();
        assert_eq!(
            kinds,
            vec![(Var::from(param), ParamKind::Concrete(expected.clone()))],
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

    let err = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("ghost"),
    )
    .expect_err("expected UnknownTransformation");
    let AnalysisError::UnknownTransformation { name } = err;
    assert_eq!(name.as_str(), "ghost");
}

/// A parameter seen at different kinds in different `Or` branches is
/// `Ambiguous`, never one of the kinds with the other dropped. Such a
/// programme validates (branch scopes do not export) and is lawful at
/// runtime, which picks the branch that matches, so the embedder needs
/// the either-or contract, not a false narrowing or an error.
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

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("either_shape"),
    )
    .unwrap();

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

/// `let amt = amount; admit Payment(amt)`: the kind is observed on the
/// alias `amt`, and must reach the parameter `amount` rather than leave
/// it `Unconstrained`.
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

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("process"),
    )
    .unwrap();
    assert_eq!(
        kinds,
        vec![(
            Var::from("amount"),
            ParamKind::Concrete(PredicateArgKind::Decimal),
        )],
    );
}

/// Aliases can also create ambiguity: if `x` and `y` are aliased and
/// one is seen at Decimal and the other at Subject, the parameter is
/// `Ambiguous`. Aliasing does not hide the disagreement.
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

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("diverge"),
    )
    .unwrap();
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

/// Aliases do not change the output: parameters come back in
/// `transformation.parameters` order, with no alias names and no hash
/// order.
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

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("act"),
    )
    .unwrap();
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

/// Rebinding breaks an alias. A transformation like
///
///     let y = x        -- alias (y, x)
///     let y = literal  -- rebind: alias broken, y is now a fresh value
///     admit DecimalSlot(y)
///
/// must not pass the later Decimal observation back to `x`. Aliases are
/// tracked in statement order, not unioned at the end.
#[test]
fn param_alias_broken_by_let_rebinding_does_not_inherit_later_observations() {
    let prog = program("rebinding_test")
        .predicates(vec![predicate("decimal_slot").decimal("d").build()])
        .transformations(vec![transformation(
            "rebind",
            params(&["x"]),
            vec![
                let_("y", term(var("x"))),
                let_("y", term(dec("1"))),
                assert_("decimal_slot", vec![var("y")]),
            ],
        )])
        .build();

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("rebind"),
    )
    .unwrap();
    assert_eq!(
        kinds,
        vec![(Var::from("x"), ParamKind::Unconstrained)],
        "after `let y = literal` rebinds y, the later DecimalSlot(y) \
         observation must not propagate back to `x`. The rebinding \
         broke the alias.",
    );
}

/// A `for` binding shadows an outer name of the same name inside the
/// body. So in
///
///     transformation t(x, items):
///         for x in items:
///             assert decimal_slot(x)
///
/// the body observes the loop's x, not the parameter x, which must stay
/// `Unconstrained` rather than become `Concrete(Decimal)`.
#[test]
fn for_binding_reusing_param_name_does_not_type_the_external_param() {
    let prog = program("for_shadow_test")
        .predicates(vec![
            predicate("items").collection("items").build(),
            predicate("decimal_slot").decimal("d").build(),
        ])
        .transformations(vec![transformation(
            "loop_it",
            params(&["x", "items"]),
            vec![for_(
                "x",
                term(var("items")),
                vec![assert_("decimal_slot", vec![var("x")])],
            )],
        )])
        .build();

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("loop_it"),
    )
    .unwrap();
    assert_eq!(
        kinds,
        vec![
            (Var::from("x"), ParamKind::Unconstrained),
            (
                Var::from("items"),
                ParamKind::Collection(Box::new(ParamKind::Concrete(PredicateArgKind::Decimal))),
            ),
        ],
        "the For-loop binding `x` shadows the external parameter `x`, so \
         observations inside the loop body must not propagate to the \
         external parameter `x` (which stays Unconstrained) - but they DO \
         become `items`'s element kind: the body uses the binding at a \
         Decimal slot, so `items` is a collection of Decimal",
    );
}

/// A collection parameter iterated by `for`, whose loop binding lands at
/// a Subject slot, infers `Collection(Concrete(Subject))`, so an
/// embedder can type a whole submitted batch as `list[str]`.
#[test]
fn an_iterated_collection_infers_its_element_kind() {
    let prog = program("batch_test")
        .predicates(vec![
            predicate("settled")
                .subject("batch")
                .subject("line")
                .build(),
        ])
        .transformations(vec![transformation(
            "settle",
            params(&["batch", "lines"]),
            vec![for_(
                "line",
                term(var("lines")),
                vec![assert_("settled", vec![var("batch"), var("line")])],
            )],
        )])
        .build();

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("settle"),
    )
    .unwrap();
    assert_eq!(
        kinds,
        vec![
            (
                Var::from("batch"),
                ParamKind::Concrete(PredicateArgKind::Subject),
            ),
            (
                Var::from("lines"),
                ParamKind::Collection(Box::new(ParamKind::Concrete(PredicateArgKind::Subject))),
            ),
        ],
    );
}

/// A collection whose loop binding is never used at a kind-bearing
/// position stays the opaque `Concrete(Collection)`: an element kind is
/// inferred only when the body constrains the binding.
#[test]
fn an_iterated_collection_with_an_unused_binding_stays_opaque() {
    let prog = program("opaque_batch")
        .transformations(vec![transformation(
            "iterate",
            params(&["xs"]),
            vec![for_("x", term(var("xs")), vec![])],
        )])
        .build();

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("iterate"),
    )
    .unwrap();
    assert_eq!(
        kinds,
        vec![(
            Var::from("xs"),
            ParamKind::Concrete(PredicateArgKind::Collection),
        )],
    );
}

/// `forall x in xs: ...` infers the element kind the same way `for`
/// does, so a batch checked with `require forall acct in batch: ...`
/// is typed as a collection of subjects. The IR below is what the
/// surface lowers that to: a `Forall` over `In(acct, called_accounts)`.
#[test]
fn a_forall_over_a_collection_infers_its_element_kind() {
    let prog = program("forall_batch")
        .predicates(vec![
            predicate("margin_eligible").subject("account").build(),
        ])
        .transformations(vec![transformation(
            "check_batch",
            params(&["called_accounts"]),
            vec![require(forall(
                "acct",
                in_(var("acct"), var("called_accounts")),
                claim("margin_eligible", vec![var("acct")]),
            ))],
        )])
        .build();

    let kinds = transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("check_batch"),
    )
    .unwrap();
    assert_eq!(
        kinds,
        vec![(
            Var::from("called_accounts"),
            ParamKind::Collection(Box::new(ParamKind::Concrete(PredicateArgKind::Subject))),
        )],
    );
}

/// An invalid programme stops at `Program::validated`:
/// `transformation_param_kinds` takes a `ValidatedProgram`, which it
/// cannot construct.
#[test]
fn invalid_program_surfaces_at_the_validated_gate() {
    let prog = program("invalid_test")
        .transformations(vec![transformation(
            "broken",
            params(&[]),
            vec![assert_("undeclared_predicate", vec![])],
        )])
        .build();

    let errors = prog
        .validated()
        .expect_err("an invalid programme must not yield a ValidatedProgram");
    assert!(
        !errors.is_empty(),
        "validation should report at least one error for an undeclared predicate",
    );
}

use morpholog_core::{ArithOp, Term, Value, ValueExpr};

fn qty_lit(amount: &str, unit: &str) -> ValueExpr {
    ValueExpr::Term(Term::Literal(Value::Quantity {
        amount: amount.to_string(),
        unit: unit.into(),
    }))
}

fn var_value(name: &str) -> ValueExpr {
    ValueExpr::Term(Term::Var(Var::from(name)))
}

fn kinds_of(prog: &morpholog_core::Program, t: &str) -> Vec<(Var, ParamKind)> {
    transformation_param_kinds(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from(t),
    )
    .unwrap()
}

/// A comparison against a quantity LITERAL types the other operand:
/// the decimal-domain comparison admits same-unit quantities, and the
/// literal's unit is the only place the unit can come from.
#[test]
fn a_quantity_literal_comparand_types_the_parameter() {
    let prog = program("qty_compare")
        .predicates(vec![predicate("Cap").quantity("cap", "t").build()])
        .transformations(vec![transformation(
            "load",
            params(&["amount"]),
            vec![require(le(var_value("amount"), qty_lit("100", "t")))],
        )])
        .build();
    assert_eq!(
        kinds_of(&prog, "load"),
        vec![(
            Var::from("amount"),
            ParamKind::Concrete(PredicateArgKind::Quantity("t".into())),
        )],
    );
}

/// One-side-known arithmetic refinement, right side known: a parameter
/// multiplied by a quantity literal is the bare-decimal scaling factor
/// - and never conjures the unit onto itself.
#[test]
fn scaling_a_known_quantity_types_the_scalar_as_decimal() {
    let arith = ValueExpr::Arith {
        op: ArithOp::Mul,
        left: Box::new(var_value("factor")),
        right: Box::new(qty_lit("1", "t")),
    };
    let prog = program("scaling")
        .predicates(vec![predicate("Cap").quantity("cap", "t").build()])
        .transformations(vec![transformation(
            "scale",
            params(&["factor"]),
            vec![require(le(arith, qty_lit("100", "t")))],
        )])
        .build();
    assert_eq!(
        kinds_of(&prog, "scale"),
        vec![(
            Var::from("factor"),
            ParamKind::Concrete(PredicateArgKind::Decimal),
        )],
    );
}

/// With neither side known, the decimal-only operators (`*`, `/`, `%`)
/// force both operands decimal - the additive operators must not,
/// because they also carry the time and unit rules.
#[test]
fn decimal_only_operators_force_both_unknown_operands() {
    let product = ValueExpr::Arith {
        op: ArithOp::Mul,
        left: Box::new(var_value("x")),
        right: Box::new(var_value("y")),
    };
    let prog = program("product")
        .predicates(vec![predicate("Marker").subject("m").build()])
        .transformations(vec![transformation(
            "check_product",
            params(&["x", "y"]),
            vec![require(le(
                product,
                ValueExpr::Term(Term::Literal(Value::Decimal("100".to_string()))),
            ))],
        )])
        .build();
    assert_eq!(
        kinds_of(&prog, "check_product"),
        vec![
            (
                Var::from("x"),
                ParamKind::Concrete(PredicateArgKind::Decimal)
            ),
            (
                Var::from("y"),
                ParamKind::Concrete(PredicateArgKind::Decimal)
            ),
        ],
    );
}

/// A parameter that reaches the outside world only through an emitted
/// intent still takes its kind from the intent's declaration.
#[test]
fn intent_arguments_type_their_parameters() {
    let prog = program("intents")
        .predicates(vec![predicate("Marker").subject("m").build()])
        .intents(vec![morpholog_core::IntentDecl {
            name: "AmountReported".into(),
            args: vec![morpholog_core::ArgDecl {
                name: "amount".to_string(),
                kind: PredicateArgKind::Decimal,
            }],
        }])
        .transformations(vec![transformation(
            "report",
            params(&["amount"]),
            vec![emit("AmountReported", vec![var("amount")])],
        )])
        .build();
    assert_eq!(
        kinds_of(&prog, "report"),
        vec![(
            Var::from("amount"),
            ParamKind::Concrete(PredicateArgKind::Decimal),
        )],
    );
}
