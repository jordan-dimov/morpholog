use super::*;

#[test]
fn refine_keeps_the_more_specific_kind_or_reports_conflict() {
    use InferredKind::{Known, UnknownOrAny};
    use PredicateArgKind::{Any, Decimal, Subject};
    // `a.refine(b)`: the unknown/any side yields to the other, the
    // more specific kind wins (an `Any` slot refines to a later
    // concrete use), and an incompatible pair reports itself.
    let cases = [
        (UnknownOrAny, Known(Decimal), Ok(Known(Decimal))),
        (Known(Decimal), UnknownOrAny, Ok(Known(Decimal))),
        (Known(Any), Known(Decimal), Ok(Known(Decimal))),
        (Known(Decimal), Known(Any), Ok(Known(Decimal))),
        (Known(Decimal), Known(Subject), Err((Decimal, Subject))),
    ];
    for (a, b, expected) in cases {
        let desc = format!("{a:?}.refine({b:?})");
        assert_eq!(a.refine(b), expected, "{desc}");
    }
}

#[test]
fn kindenv_observe_then_lookup_returns_refined_kind() {
    let mut env = KindEnv::default();
    env.observe(
        &Var::from("amount"),
        InferredKind::Known(PredicateArgKind::Decimal),
    )
    .expect("first observation always succeeds against UnknownOrAny");
    assert_eq!(
        env.lookup(&Var::from("amount")),
        InferredKind::Known(PredicateArgKind::Decimal)
    );
}

#[test]
fn kindenv_observe_refines_through_any() {
    let mut env = KindEnv::default();
    env.observe(&Var::from("x"), InferredKind::Known(PredicateArgKind::Any))
        .unwrap();
    env.observe(
        &Var::from("x"),
        InferredKind::Known(PredicateArgKind::Decimal),
    )
    .unwrap();
    assert_eq!(
        env.lookup(&Var::from("x")),
        InferredKind::Known(PredicateArgKind::Decimal)
    );
}

#[test]
fn kindenv_observe_reports_conflict_with_previous_kinds() {
    let mut env = KindEnv::default();
    env.observe(
        &Var::from("x"),
        InferredKind::Known(PredicateArgKind::Decimal),
    )
    .unwrap();
    let err = env
        .observe(
            &Var::from("x"),
            InferredKind::Known(PredicateArgKind::Subject),
        )
        .expect_err("conflict");
    assert_eq!(err, (PredicateArgKind::Decimal, PredicateArgKind::Subject));
}

// ============================================================
// check_program: claim arg checking + statement flow
// ============================================================

use crate::ir::{ArgDecl, Program};
use crate::ir_builder::*;

/// Build a `PredicateDecl` shorthand for tests.
fn pdecl(name: &str, args: &[(&str, PredicateArgKind)]) -> crate::ir::PredicateDecl {
    crate::ir::PredicateDecl {
        name: name.into(),
        disciplines: Vec::new(),
        args: args
            .iter()
            .map(|(n, k)| ArgDecl {
                name: n.to_string(),
                kind: k.clone(),
            })
            .collect(),
    }
}

fn empty_program() -> Program {
    program("test").build()
}

#[test]
fn clean_programme_returns_no_kind_errors() {
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "Policy",
        &[
            ("policy_id", PredicateArgKind::Subject),
            ("limit", PredicateArgKind::Decimal),
        ],
    )];
    p.invariants = vec![invariant(
        "any_policy_has_positive_limit",
        claim("Policy", vec![var("p"), var("l")]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "clean programme should report no errors; got {errs:?}"
    );
}

#[test]
fn decimal_literal_in_subject_slot_is_flagged() {
    let mut p = empty_program();
    p.predicates = vec![pdecl("Policy", &[("policy_id", PredicateArgKind::Subject)])];
    p.invariants = vec![invariant("bad", claim("Policy", vec![dec("123")]))];
    let errs = check_program(&p);
    assert_eq!(errs.len(), 1, "expected one kind error; got {errs:?}");
    match &errs[0] {
        ValidationError::ArgKindMismatch {
            vocabulary: VocabularyKind::Predicate,
            name,
            position,
            expected,
            actual,
            ..
        } => {
            assert_eq!(name, "Policy");
            assert_eq!(*position, 0);
            assert_eq!(*expected, PredicateArgKind::Subject);
            assert_eq!(*actual, PredicateArgKind::Decimal);
        }
        other => panic!("expected ArgKindMismatch, got {other:?}"),
    }
}

#[test]
fn variable_kind_refined_across_claim_uses() {
    // Pattern: bind variable `x` from a Decimal-slot Claim,
    // then use it in another Decimal-slot Claim. Should pass.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("B", &[("v", PredicateArgKind::Decimal)]),
    ];
    p.invariants = vec![invariant(
        "refine",
        and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "consistent refinement should pass; got {errs:?}"
    );
}

#[test]
fn variable_kind_conflict_across_claim_uses_is_flagged() {
    // Pattern: bind `x` from a Decimal slot, then use in a
    // Subject slot. Conflict.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("B", &[("v", PredicateArgKind::Subject)]),
    ];
    p.invariants = vec![invariant(
        "conflict",
        and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
    )];
    let errs = check_program(&p);
    assert_eq!(errs.len(), 1, "expected one conflict; got {errs:?}");
    match &errs[0] {
        ValidationError::VariableKindConflict {
            variable,
            previous,
            new,
            ..
        } => {
            assert_eq!(variable, "x");
            assert_eq!(*previous, PredicateArgKind::Decimal);
            assert_eq!(*new, PredicateArgKind::Subject);
        }
        other => panic!("expected VariableKindConflict, got {other:?}"),
    }
}

#[test]
fn any_slot_observes_variable_without_constraining_it() {
    // `A` declares its slot as `Any`. Variable `x` should
    // not be pinned to Any; later use in a Decimal slot
    // should refine it cleanly.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Any)]),
        pdecl("B", &[("v", PredicateArgKind::Decimal)]),
    ];
    p.invariants = vec![invariant(
        "refines_through_any",
        and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "Any-then-Decimal should refine; got {errs:?}"
    );
}

#[test]
fn actor_term_carries_subject_kind() {
    // `actor` in a Decimal slot is a kind mismatch. In a
    // transformation body `actor` is available, so that is the only
    // error.
    let mut p = empty_program();
    p.predicates = vec![pdecl("Limit", &[("amount", PredicateArgKind::Decimal)])];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![assert_("Limit", vec![actor()])],
    )];
    let errs = check_program(&p);
    assert_eq!(
        errs.len(),
        1,
        "actor-in-decimal-slot must flag; got {errs:?}"
    );
    match &errs[0] {
        ValidationError::ArgKindMismatch {
            vocabulary: VocabularyKind::Predicate,
            expected,
            actual,
            ..
        } => {
            assert_eq!(*expected, PredicateArgKind::Decimal);
            assert_eq!(*actual, PredicateArgKind::Subject);
        }
        other => panic!("expected ArgKindMismatch, got {other:?}"),
    }
}

// ----- actor-in-wrong-context -----

#[test]
fn actor_in_invariant_body_flags_actor_not_available() {
    // An invariant has no proposing actor: the kernel raises
    // UnboundActor at runtime, the check flags it statically. The
    // slot is Subject so no kind error interferes.
    let mut p = empty_program();
    p.predicates = vec![pdecl("Approver", &[("who", PredicateArgKind::Subject)])];
    p.invariants = vec![invariant(
        "mentions_actor",
        claim("Approver", vec![actor()]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::ActorNotAvailable {
                context: ValidationContext::Invariant { .. }
            }
        )),
        "actor in an invariant body must flag ActorNotAvailable; got {errs:?}"
    );
}

#[test]
fn two_derived_declarations_of_one_head_are_refused() {
    // The parser refuses this, but hand-built IR reaches validation
    // directly.
    let mut p = empty_program();
    p.predicates = vec![pdecl("Src", &[("k", PredicateArgKind::Subject)])];
    let row = crate::ir::DerivedClaim {
        predicate: "Row".into(),
        keys: vec!["k".into()],
        values: vec![],
        domain: claim("Src", vec![var("k")]),
    };
    p.derived_claims = vec![row.clone(), row];
    let errs = p
        .validate()
        .expect_err("a duplicate derived head is refused");
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::DuplicateDecl {
                vocabulary: VocabularyKind::Derived,
                name,
            } if name == "Row"
        )),
        "got {errs:?}"
    );
}

#[test]
fn actor_in_derived_claim_value_flags_actor_not_available() {
    // `actor` in a derived-claim value expression - same
    // unavailability as an invariant body.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl(
            "Row",
            &[
                ("k", PredicateArgKind::Subject),
                ("v", PredicateArgKind::Subject),
            ],
        ),
        pdecl("Src", &[("k", PredicateArgKind::Subject)]),
    ];
    p.derived_claims = vec![crate::ir::DerivedClaim {
        predicate: "Row".into(),
        keys: vec!["k".into()],
        values: vec![crate::ir::DerivedValue {
            name: "v".into(),
            expr: term(actor()),
        }],
        domain: claim("Src", vec![var("k")]),
    }];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::ActorNotAvailable {
                context: ValidationContext::DerivedClaim { .. }
            }
        )),
        "actor in a derived-claim value must flag ActorNotAvailable; got {errs:?}"
    );
}

/// A derived claim is a read model, so any rule that names one is
/// refused at authoring time rather than failing against a live
/// database. Covers every position a rule can name a predicate.
#[test]
fn no_rule_may_name_a_derived_claim() {
    let derived = |p: &mut crate::ir::Program| {
        p.predicates = vec![
            pdecl("Src", &[("k", PredicateArgKind::Subject)]),
            pdecl("Row", &[("k", PredicateArgKind::Subject)]),
            pdecl("Out", &[("k", PredicateArgKind::Subject)]),
        ];
        p.derived_claims = vec![crate::ir::DerivedClaim {
            predicate: "Row".into(),
            keys: vec!["k".into()],
            values: vec![],
            domain: claim("Src", vec![var("k")]),
        }];
    };

    // Each case names `Row` - the derived - somewhere a rule reads.
    let mut invariant_case = empty_program();
    derived(&mut invariant_case);
    invariant_case.invariants = vec![invariant("reads_derived", claim("Row", vec![var("k")]))];

    let mut bind_case = empty_program();
    derived(&mut bind_case);
    bind_case.transformations = vec![transformation(
        "binds_derived",
        params(&["k"]),
        vec![bind_one(claim("Row", vec![var("k")]))],
    )];

    let mut require_case = empty_program();
    derived(&mut require_case);
    require_case.transformations = vec![transformation(
        "requires_derived",
        params(&["k"]),
        vec![require(claim("Row", vec![var("k")]))],
    )];

    let mut admit_case = empty_program();
    derived(&mut admit_case);
    admit_case.transformations = vec![transformation(
        "admits_derived",
        params(&["k"]),
        vec![assert_("Row", vec![var("k")])],
    )];

    // Deriveds do not compose: a derived's domain also reads only
    // admitted claims.
    let mut derived_case = empty_program();
    derived(&mut derived_case);
    derived_case.derived_claims.push(crate::ir::DerivedClaim {
        predicate: "Out".into(),
        keys: vec!["k".into()],
        values: vec![],
        domain: claim("Row", vec![var("k")]),
    });

    for (label, program) in [
        ("invariant", invariant_case),
        ("bind", bind_case),
        ("require", require_case),
        ("admit", admit_case),
        ("another derived's domain", derived_case),
    ] {
        let errs = check_program(&program);
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::DerivedInRule { .. })),
            "{label} over a derived must be refused; got {errs:?}"
        );
    }
}

/// A discipline is a promise about governed state, and a derived
/// output is not governed state.
///
/// Both clause shapes are covered: `unique by` would otherwise surface
/// as an error in a generated rule the author never wrote, and
/// `append only` would pass silently on a view that refresh replaces
/// wholesale.
#[test]
fn a_derived_output_cannot_carry_a_discipline() {
    for discipline in [
        crate::ir::Discipline::AppendOnly,
        crate::ir::Discipline::UniqueBy {
            fields: vec!["k".into()],
        },
    ] {
        let mut p = empty_program();
        let mut row = pdecl("Row", &[("k", PredicateArgKind::Subject)]);
        row.disciplines = vec![discipline.clone()];
        p.predicates = vec![pdecl("Src", &[("k", PredicateArgKind::Subject)]), row];
        p.derived_claims = vec![crate::ir::DerivedClaim {
            predicate: "Row".into(),
            keys: vec!["k".into()],
            values: vec![],
            domain: claim("Src", vec![var("k")]),
        }];
        let errs = check_program(&p);
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::DisciplineOnDerived { .. })),
            "{discipline:?} on a derived head must be refused; got {errs:?}"
        );
    }
}

/// The acceptance side: a derived claim reading ordinary admitted
/// claims is fine.
#[test]
fn a_derived_may_read_the_claims_it_is_computed_from() {
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("Src", &[("k", PredicateArgKind::Subject)]),
        pdecl("Row", &[("k", PredicateArgKind::Subject)]),
    ];
    p.derived_claims = vec![crate::ir::DerivedClaim {
        predicate: "Row".into(),
        keys: vec!["k".into()],
        values: vec![],
        domain: claim("Src", vec![var("k")]),
    }];
    let errs = check_program(&p);
    assert!(
        !errs
            .iter()
            .any(|e| matches!(e, ValidationError::DerivedInRule { .. })),
        "a derived reading its own sources must pass; got {errs:?}"
    );
}

#[test]
fn actor_in_transformation_body_is_allowed() {
    // `actor` resolves inside transformation bodies - no
    // ActorNotAvailable. Slot is Subject so no kind error either.
    let mut p = empty_program();
    p.predicates = vec![pdecl("Approver", &[("who", PredicateArgKind::Subject)])];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![assert_("Approver", vec![actor()])],
    )];
    let errs = check_program(&p);
    assert!(
        !errs
            .iter()
            .any(|e| matches!(e, ValidationError::ActorNotAvailable { .. })),
        "actor in a transformation body must not flag; got {errs:?}"
    );
}

// ----- Statement flow (require / bind_one / let / assert) -----

#[test]
fn bind_one_extends_env_for_subsequent_statements() {
    // bind_one A(x) (x: Decimal); assert B(x) (B's slot: Decimal). Clean.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("B", &[("v", PredicateArgKind::Decimal)]),
    ];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            bind_one(claim("A", vec![var("x")])),
            assert_("B", vec![var("x")]),
        ],
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "bind_one then matching assert should pass; got {errs:?}"
    );
}

#[test]
fn bind_one_then_conflicting_assert_flags_variable_conflict() {
    // bind_one binds x: Decimal; then assert pushes x into Subject slot.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("B", &[("v", PredicateArgKind::Subject)]),
    ];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            bind_one(claim("A", vec![var("x")])),
            assert_("B", vec![var("x")]),
        ],
    )];
    let errs = check_program(&p);
    assert_eq!(errs.len(), 1, "expected conflict; got {errs:?}");
    assert!(matches!(
        errs[0],
        ValidationError::VariableKindConflict {
            variable: ref v,
            previous: PredicateArgKind::Decimal,
            new: PredicateArgKind::Subject,
            ..
        } if v == "x"
    ));
}

#[test]
fn require_does_not_export_bindings_to_subsequent_statements() {
    // `require A(x)` binds x only within the require, so the later
    // `assert B(x)` uses an unbound x and must flag UnboundVariable,
    // as the runtime would.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("B", &[("v", PredicateArgKind::Subject)]),
    ];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            require(claim("A", vec![var("x")])),
            assert_("B", vec![var("x")]),
        ],
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::UnboundVariable { variable: v, .. } if v == "x"
        )),
        "require must not export x to the later assert; got {errs:?}"
    );
}

#[test]
fn params_flow_to_admit_without_a_binding_statement() {
    // Parameters are bound at transformation entry, so an `admit`
    // using them directly is clean.
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "Payment",
        &[
            ("payer", PredicateArgKind::Subject),
            ("limit", PredicateArgKind::Decimal),
        ],
    )];
    p.transformations = vec![transformation(
        "t",
        params(&["p", "limit"]),
        vec![assert_("Payment", vec![var("p"), var("limit")])],
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "parameters are bound at entry; the admit is clean. got {errs:?}"
    );
}

// Branch binding export, for an invariant
// `A(x, n) implies (B(x, m) <op> C(...)) and n <= m`: `m` reaches the
// comparator only if every branch binds it, since the runtime may
// carry forward a witness from a branch that did not. Checked for `or`
// and `xor` and both binding shapes.
#[derive(Clone, Copy)]
enum BranchOp {
    Or,
    Xor,
}

fn branch_export_program(op: BranchOp, both_branches_bind_m: bool) -> Program {
    let mut p = empty_program();
    let (c_decl, c_args) = if both_branches_bind_m {
        (
            pdecl(
                "C",
                &[
                    ("x", PredicateArgKind::Subject),
                    ("m", PredicateArgKind::Decimal),
                ],
            ),
            vec![var("x"), var("m")],
        )
    } else {
        (
            pdecl("C", &[("x", PredicateArgKind::Subject)]),
            vec![var("x")],
        )
    };
    p.predicates = vec![
        pdecl(
            "A",
            &[
                ("x", PredicateArgKind::Subject),
                ("n", PredicateArgKind::Decimal),
            ],
        ),
        pdecl(
            "B",
            &[
                ("x", PredicateArgKind::Subject),
                ("m", PredicateArgKind::Decimal),
            ],
        ),
        c_decl,
    ];
    let b = claim("B", vec![var("x"), var("m")]);
    let c = claim("C", c_args);
    let branches = match op {
        BranchOp::Or => or(vec![b, c]),
        BranchOp::Xor => xor(b, c),
    };
    p.invariants = vec![invariant(
        "inv",
        implies(
            claim("A", vec![var("x"), var("n")]),
            and(vec![branches, le(term(var("n")), term(var("m")))]),
        ),
    )];
    p
}

#[test]
fn branch_binding_exports_only_when_every_branch_binds() {
    for (op, both_bind, expect_ok, label) in [
        (
            BranchOp::Or,
            true,
            true,
            "or: both branches bind m -> exports",
        ),
        (
            BranchOp::Or,
            false,
            false,
            "or: one branch binds m -> no export",
        ),
        (
            BranchOp::Xor,
            true,
            true,
            "xor: both operands bind m -> exports",
        ),
        (
            BranchOp::Xor,
            false,
            false,
            "xor: one operand binds m -> no export",
        ),
    ] {
        let errs = check_program(&branch_export_program(op, both_bind));
        if expect_ok {
            assert!(errs.is_empty(), "{label}: expected no errors; got {errs:?}");
        } else {
            assert!(
                errs.iter().any(|e| matches!(
                    e,
                    ValidationError::UnboundVariable { variable: v, .. } if v == "m"
                )),
                "{label}: expected UnboundVariable(m); got {errs:?}"
            );
        }
    }
}

#[test]
fn in_generator_binds_unbound_element_in_sum_body() {
    // The settlement shape: `sum(x | line in lines and P(line, x))`.
    // `line` is not pre-bound; `in` binds it to each item (it is a
    // generator, not a use), so `P(line, x)` matches cleanly.
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "P",
        &[
            ("line", PredicateArgKind::Subject),
            ("amount", PredicateArgKind::Decimal),
        ],
    )];
    p.transformations = vec![transformation(
        "t",
        params(&["lines"]),
        vec![let_(
            "total",
            sum(
                var("x"),
                and(vec![
                    in_(var("line"), var("lines")),
                    claim("P", vec![var("line"), var("x")]),
                ]),
            ),
        )],
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "`in` binds the unbound element; the sum body is clean. got {errs:?}"
    );
}

#[test]
fn let_new_subject_binds_name_as_subject() {
    // let_new_subject names a fresh subject; using it in a
    // Decimal slot must flag.
    let mut p = empty_program();
    p.predicates = vec![pdecl("Amt", &[("v", PredicateArgKind::Decimal)])];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![let_new_subject("fresh"), assert_("Amt", vec![var("fresh")])],
    )];
    let errs = check_program(&p);
    assert_eq!(
        errs.len(),
        1,
        "subject-into-decimal must flag; got {errs:?}"
    );
    assert!(matches!(
        errs[0],
        ValidationError::VariableKindConflict {
            previous: PredicateArgKind::Subject,
            new: PredicateArgKind::Decimal,
            ..
        }
    ));
}

#[test]
fn retract_args_are_kind_checked() {
    // Retract is the read side of the assert pair; same kind rules apply.
    let mut p = empty_program();
    p.predicates = vec![pdecl("P", &[("id", PredicateArgKind::Subject)])];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![retract("P", vec![dec("99")])],
    )];
    let errs = check_program(&p);
    assert_eq!(errs.len(), 1);
    assert!(matches!(
        errs[0],
        ValidationError::ArgKindMismatch {
            vocabulary: VocabularyKind::Predicate,
            ..
        }
    ));
}

// ============================================================
// Value-expression inference + comparators / arithmetic
// ============================================================

/// A date and a decimal under each comparator: `<=` flags the date,
/// `on_or_before` flags the decimal. At runtime this is a TypeMismatch.
#[test]
fn comparator_operand_mismatches_flag_operator_and_kinds() {
    type Comparator = fn(ValueExpr, ValueExpr) -> Prop;
    let cases: [(Comparator, &str, PredicateArgKind, PredicateArgKind); 2] = [
        (le, "<=", PredicateArgKind::Decimal, PredicateArgKind::Date),
        (
            date_le,
            "on_or_before",
            PredicateArgKind::Date,
            PredicateArgKind::Decimal,
        ),
    ];
    for (comparator, want_op, want_expected, want_actual) in cases {
        let mut p = empty_program();
        p.invariants = vec![invariant(
            "bad_compare",
            comparator(term(date("2026-01-01")), term(dec("100"))),
        )];
        let errs = check_program(&p);
        assert_eq!(
            errs.len(),
            1,
            "{want_op}: expected one operand error; got {errs:?}"
        );
        match &errs[0] {
            ValidationError::OperandKindMismatch {
                operator,
                expected,
                actual,
                ..
            } => {
                assert_eq!(*operator, want_op);
                assert_eq!(*expected, want_expected);
                assert_eq!(*actual, want_actual);
            }
            other => panic!("expected OperandKindMismatch, got {other:?}"),
        }
    }
}

#[test]
fn abs_of_a_subject_flags_abs_kind() {
    // abs is defined on signed numeric kinds; a subject has no
    // magnitude.
    let mut p = empty_program();
    p.invariants = vec![invariant(
        "bad_abs",
        le(abs(term(subj("not_a_number"))), term(dec("100"))),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::AbsKind {
                kind: PredicateArgKind::Subject,
                ..
            }
        )),
        "expected AbsKind on a subject, got {errs:?}"
    );
}

#[test]
fn abs_of_a_decimal_is_accepted() {
    let mut p = empty_program();
    p.invariants = vec![invariant(
        "ok_abs",
        le(abs(term(dec("10"))), term(dec("100"))),
    )];
    assert!(
        check_program(&p).is_empty(),
        "abs of a decimal should type-check"
    );
}

#[test]
fn abs_refines_the_variable_it_wraps() {
    // `x` is used in a Subject slot and inside `abs(x) <= 10`.
    // Refinement through abs pins x to Decimal, which conflicts.
    let mut p = empty_program();
    p.predicates = vec![pdecl("S", &[("v", PredicateArgKind::Subject)])];
    p.invariants = vec![invariant(
        "abs_refine",
        and(vec![
            claim("S", vec![var("x")]),
            le(abs(term(var("x"))), term(dec("10"))),
        ]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict { variable, .. } if variable == "x"
        )),
        "abs(x) should refine x to Decimal and conflict with the Subject use: {errs:?}"
    );
}

#[test]
fn add_with_subject_literal_operand_flags_no_arith_rule() {
    // Arithmetic on a subject literal. The report names both operand
    // kinds rather than assuming Decimal was intended.
    let mut p = empty_program();
    p.invariants = vec![invariant(
        "bad_add",
        le(
            add(term(dec("10")), term(subj("not_a_number"))),
            term(dec("100")),
        ),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::NoArithRule {
                operator: "+",
                left: PredicateArgKind::Decimal,
                right: PredicateArgKind::Subject,
                ..
            }
        )),
        "expected NoArithRule on `+`, got {errs:?}"
    );
}

#[test]
fn comparator_refines_variable_for_subsequent_uses() {
    // `require A(x) and x <= 100` should refine `x` from
    // unconstrained (via A's Any slot) to Decimal. A later use
    // of `x` in a Subject slot must conflict.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Any)]),
        pdecl("B", &[("v", PredicateArgKind::Subject)]),
    ];
    p.invariants = vec![invariant(
        "refine_via_le",
        and(vec![
            claim("A", vec![var("x")]),
            le(term(var("x")), term(dec("100"))),
            claim("B", vec![var("x")]),
        ]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                variable: v,
                previous: PredicateArgKind::Decimal,
                new: PredicateArgKind::Subject,
                ..
            } if v == "x"
        )),
        "expected x to refine to Decimal via Le then conflict on Subject use; got {errs:?}"
    );
}

#[test]
fn strict_equality_flags_distinct_operand_kinds() {
    // Eq and Neq are strict: Decimal vs Subject must surface as a
    // kind mismatch, not be silently coerced.
    for (name, body, operator) in [
        ("bad_eq", eq(term(dec("100")), term(subj("S"))), "="),
        ("bad_neq", neq(dec("100"), subj("S")), "!="),
    ] {
        let mut p = empty_program();
        p.invariants = vec![invariant(name, body)];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1, "{operator}: {errs:?}");
        assert!(
            matches!(
                errs[0],
                ValidationError::EqualityKindMismatch { operator: op, .. } if op == operator
            ),
            "{operator}: got {errs:?}"
        );
    }
}

#[test]
fn eq_refines_variable_to_concrete_kind_for_subsequent_uses() {
    // `x == 100` against an otherwise unconstrained `x` should
    // pin `x` to Decimal; a later Subject-slot use conflicts.
    let mut p = empty_program();
    p.predicates = vec![pdecl("B", &[("v", PredicateArgKind::Subject)])];
    p.invariants = vec![invariant(
        "refine_via_eq",
        and(vec![
            eq(term(var("x")), term(dec("100"))),
            claim("B", vec![var("x")]),
        ]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                previous: PredicateArgKind::Decimal,
                new: PredicateArgKind::Subject,
                ..
            }
        )),
        "expected refinement via Eq then conflict; got {errs:?}"
    );
}

#[test]
fn let_binds_name_at_inferred_value_kind() {
    // `let y = x - 1` where `x` was already Decimal binds `y`
    // as Decimal; using `y` in a Subject slot must conflict.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("S", &[("id", PredicateArgKind::Subject)]),
    ];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            bind_one(claim("A", vec![var("x")])),
            let_("y", sub(term(var("x")), term(dec("1")))),
            assert_("S", vec![var("y")]),
        ],
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                variable: v,
                previous: PredicateArgKind::Decimal,
                new: PredicateArgKind::Subject,
                ..
            } if v == "y"
        )),
        "expected y to inherit Decimal from let-expression; got {errs:?}"
    );
}

#[test]
fn comparator_clean_when_operands_match_expected_kind() {
    // The happy path: amount <= limit, both bound from
    // Decimal slots. No errors expected.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("L", &[("v", PredicateArgKind::Decimal)]),
    ];
    p.invariants = vec![invariant(
        "ok",
        and(vec![
            claim("A", vec![var("amount")]),
            claim("L", vec![var("limit")]),
            le(term(var("amount")), term(var("limit"))),
        ]),
    )];
    let errs = check_program(&p);
    assert!(errs.is_empty(), "happy path should pass; got {errs:?}");
}

// ============================================================
// Sum + ValueOf
// ============================================================

#[test]
fn sum_with_body_refined_value_term_passes() {
    // The canonical aggregation shape: `sum(amount | P(_, amount))`
    // where P's value slot is Decimal. Sum's body refines
    // `amount` to Decimal; the value term resolves to Decimal;
    // Sum is happy.
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "Payment",
        &[
            ("policy", PredicateArgKind::Subject),
            ("amount", PredicateArgKind::Decimal),
        ],
    )];
    p.invariants = vec![invariant(
        "ok_sum",
        le(
            sum(
                var("amount"),
                claim("Payment", vec![wildcard(), var("amount")]),
            ),
            term(dec("1000")),
        ),
    )];
    let errs = check_program(&p);
    assert!(errs.is_empty(), "well-typed Sum should pass; got {errs:?}");
}

#[test]
fn sum_with_subject_value_term_flags_operand_mismatch() {
    // `sum(p | Payment(p, _))` - the value term is a Subject
    // (refined from P's first slot), but Sum demands Decimal.
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "Payment",
        &[
            ("policy", PredicateArgKind::Subject),
            ("amount", PredicateArgKind::Decimal),
        ],
    )];
    p.invariants = vec![invariant(
        "bad_sum",
        le(
            sum(var("p"), claim("Payment", vec![var("p"), wildcard()])),
            term(dec("1000")),
        ),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::OperandKindMismatch {
                operator: "sum",
                expected: PredicateArgKind::Decimal,
                actual: PredicateArgKind::Subject,
                ..
            }
        )),
        "expected sum's value term to flag Subject vs Decimal; got {errs:?}"
    );
}

#[test]
fn sum_with_date_literal_value_term_flags_operand_mismatch() {
    // A literal in the value position that is not Decimal.
    let mut p = empty_program();
    p.predicates = vec![pdecl("X", &[("v", PredicateArgKind::Subject)])];
    p.invariants = vec![invariant(
        "bad_sum_lit",
        le(
            sum(date("2026-01-01"), claim("X", vec![var("x")])),
            term(dec("100")),
        ),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::OperandKindMismatch {
                operator: "sum",
                expected: PredicateArgKind::Decimal,
                actual: PredicateArgKind::Date,
                ..
            }
        )),
        "expected sum's date literal to flag; got {errs:?}"
    );
}

#[test]
fn sum_body_bindings_do_not_leak_to_surrounding_env() {
    // `bind_one Q(x); require x <= sum(amount | P(_, amount))`.
    // The sum binds `amount` only inside its body.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("Q", &[("v", PredicateArgKind::Decimal)]),
        pdecl(
            "P",
            &[
                ("policy", PredicateArgKind::Subject),
                ("amount", PredicateArgKind::Decimal),
            ],
        ),
        pdecl("S", &[("id", PredicateArgKind::Subject)]),
    ];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            bind_one(claim("Q", vec![var("x")])),
            require(le(
                term(var("x")),
                sum(var("amount"), claim("P", vec![wildcard(), var("amount")])),
            )),
            // Here `amount` is unbound again: the sum binding did
            // not escape.
            assert_("S", vec![var("amount")]),
        ],
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::UnboundVariable { variable: v, .. } if v == "amount"
        )),
        "Sum's `amount` binding must not leak to the later assert; got {errs:?}"
    );
}

#[test]
fn value_of_resolves_to_wildcard_slot_kind() {
    // `value Policy(p, _)` reads Policy's Decimal slot, so `<= 100`
    // is fine. `p` is a transformation parameter, so it is bound.
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "Policy",
        &[
            ("policy", PredicateArgKind::Subject),
            ("limit", PredicateArgKind::Decimal),
        ],
    )];
    p.transformations = vec![transformation(
        "t",
        vec!["p".into()],
        vec![require(le(
            value_of("Policy", vec![var("p"), wildcard()]),
            term(dec("100")),
        ))],
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "ValueOf at decimal slot should be Decimal; got {errs:?}"
    );
}

#[test]
fn value_of_with_subject_slot_in_comparator_flags_operand_mismatch() {
    // `value Owner(p, _) <= 100` - wildcard is Owner's
    // Subject slot; Le's LHS is Subject, not Decimal.
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "Owner",
        &[
            ("policy", PredicateArgKind::Subject),
            ("owner", PredicateArgKind::Subject),
        ],
    )];
    p.invariants = vec![invariant(
        "bad_value_of",
        le(
            value_of("Owner", vec![var("p"), wildcard()]),
            term(dec("100")),
        ),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::OperandKindMismatch {
                operator: "<=",
                expected: PredicateArgKind::Decimal,
                actual: PredicateArgKind::Subject,
                ..
            }
        )),
        "expected Le LHS to flag Subject vs Decimal; got {errs:?}"
    );
}

// ============================================================
// Derived-claim output args vs declared kinds
// ============================================================

use crate::ir::{DerivedClaim, DerivedValue};

#[test]
fn derived_claim_key_var_with_wrong_kind_flags_predicate_arg_kind_mismatch() {
    // Out predicate Row(account: Subject, ...); `over P(account)`
    // where P binds account at Decimal. Output position 0
    // expects Subject; actual is Decimal.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl(
            "Row",
            &[
                ("account", PredicateArgKind::Subject),
                ("balance", PredicateArgKind::Decimal),
            ],
        ),
        pdecl("P", &[("v", PredicateArgKind::Decimal)]),
    ];
    p.derived_claims = vec![DerivedClaim {
        predicate: "Row".into(),
        keys: vec!["account".into()],
        values: vec![DerivedValue {
            name: "balance".into(),
            expr: term(dec("0")),
        }],
        domain: claim("P", vec![var("account")]),
    }];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::ArgKindMismatch {
                vocabulary: VocabularyKind::Predicate,
                name: pn,
                position: 0,
                expected: PredicateArgKind::Subject,
                actual: PredicateArgKind::Decimal,
                ..
            } if pn == "Row"
        )),
        "derived key vs declared kind mismatch must flag; got {errs:?}"
    );
}

#[test]
fn derived_claim_value_expr_with_wrong_kind_flags_predicate_arg_kind_mismatch() {
    // Out predicate Row(account: Subject, count: Subject);
    // value expr returns Decimal. Position 1 mismatch.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl(
            "Row",
            &[
                ("account", PredicateArgKind::Subject),
                ("count", PredicateArgKind::Subject),
            ],
        ),
        pdecl(
            "P",
            &[
                ("acct", PredicateArgKind::Subject),
                ("amt", PredicateArgKind::Decimal),
            ],
        ),
    ];
    p.derived_claims = vec![DerivedClaim {
        predicate: "Row".into(),
        keys: vec!["account".into()],
        values: vec![DerivedValue {
            name: "count".into(),
            expr: sum(var("amt"), claim("P", vec![var("account"), var("amt")])),
        }],
        domain: claim("P", vec![var("account"), wildcard()]),
    }];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::ArgKindMismatch {
                vocabulary: VocabularyKind::Predicate,
                name: pn,
                position: 1,
                expected: PredicateArgKind::Subject,
                actual: PredicateArgKind::Decimal,
                ..
            } if pn == "Row"
        )),
        "derived value vs declared kind mismatch must flag; got {errs:?}"
    );
}

#[test]
fn derived_claim_clean_when_keys_and_values_match_declared_kinds() {
    // Mirror of the TrialBalanceRow shape in the ledger example.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl(
            "Row",
            &[
                ("account", PredicateArgKind::Subject),
                ("balance", PredicateArgKind::Decimal),
            ],
        ),
        pdecl(
            "Line",
            &[
                ("account", PredicateArgKind::Subject),
                ("amount", PredicateArgKind::Decimal),
            ],
        ),
    ];
    p.derived_claims = vec![DerivedClaim {
        predicate: "Row".into(),
        keys: vec!["account".into()],
        values: vec![DerivedValue {
            name: "balance".into(),
            expr: sum(var("amt"), claim("Line", vec![var("account"), var("amt")])),
        }],
        domain: claim("Line", vec![var("account"), wildcard()]),
    }];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "well-typed derived claim should pass; got {errs:?}"
    );
}

// ============================================================
// For collection + In non-Collection literal
// ============================================================

#[test]
fn for_with_non_collection_variable_flags_operand_mismatch() {
    // `bind_one Q(x); for x in ...` - x is bound at Decimal
    // (Q's slot). The `for` collection slot demands Collection.
    let mut p = empty_program();
    p.predicates = vec![pdecl("Q", &[("v", PredicateArgKind::Decimal)])];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            bind_one(claim("Q", vec![var("x")])),
            for_("e", term(var("x")), vec![]),
        ],
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                variable: v,
                previous: PredicateArgKind::Decimal,
                new: PredicateArgKind::Collection,
                ..
            } if v == "x"
        )),
        "for on a Decimal variable must flag conflict; got {errs:?}"
    );
}

#[test]
fn for_collection_variable_refines_to_collection() {
    // `for e in xs: assert P(xs)` where P expects Decimal
    // should conflict on `xs` - the for refined xs to
    // Collection, then assert tries Decimal.
    let mut p = empty_program();
    p.predicates = vec![pdecl("P", &[("v", PredicateArgKind::Decimal)])];
    p.transformations = vec![transformation(
        "t",
        vec!["xs".into()],
        vec![
            for_("e", term(var("xs")), vec![]),
            assert_("P", vec![var("xs")]),
        ],
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                variable: v,
                previous: PredicateArgKind::Collection,
                new: PredicateArgKind::Decimal,
                ..
            } if v == "xs"
        )),
        "for must refine xs to Collection; got {errs:?}"
    );
}

#[test]
fn in_with_non_collection_literal_flags_operand_mismatch() {
    // `x in 100` - the collection side is a decimal literal,
    // which runtime would reject as "In expects a collection".
    let mut p = empty_program();
    p.invariants = vec![invariant(
        "in_lit",
        Prop::In(Term::Var("x".into()), dec("100")),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::OperandKindMismatch {
                operator: "in",
                expected: PredicateArgKind::Collection,
                actual: PredicateArgKind::Decimal,
                ..
            }
        )),
        "non-collection literal in `in` must flag; got {errs:?}"
    );
}

// ============================================================
// Intent emit arg-kind checking
// ============================================================

use crate::IntentDecl;

fn intent(name: &str, args: &[(&str, PredicateArgKind)]) -> IntentDecl {
    IntentDecl {
        name: name.into(),
        args: args
            .iter()
            .map(|(n, k)| ArgDecl {
                name: n.to_string(),
                kind: k.clone(),
            })
            .collect(),
    }
}

#[test]
fn emit_with_literal_in_wrong_kind_slot_flags_arg_kind_mismatch() {
    // `emit X(100)` against `intent X(id: Subject)` - decimal
    // literal in a Subject slot. Same shape of error as the
    // predicate-side `Assert` case, but tagged Intent.
    let mut p = empty_program();
    p.intents = vec![intent("Notify", &[("id", PredicateArgKind::Subject)])];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![emit("Notify", vec![dec("100")])],
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::ArgKindMismatch {
                vocabulary: VocabularyKind::Intent,
                name,
                position: 0,
                expected: PredicateArgKind::Subject,
                actual: PredicateArgKind::Decimal,
                ..
            } if name == "Notify"
        )),
        "expected Intent ArgKindMismatch; got {errs:?}"
    );
}

#[test]
fn emit_variable_observed_against_declared_intent_arg_kind() {
    // `bind_one P(x); emit Notify(x)` where P binds x:Decimal
    // and Notify expects x:Subject. The conflict surfaces via
    // VariableKindConflict (variable already had Decimal kind).
    let mut p = empty_program();
    p.predicates = vec![pdecl("P", &[("v", PredicateArgKind::Decimal)])];
    p.intents = vec![intent("Notify", &[("v", PredicateArgKind::Subject)])];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            bind_one(claim("P", vec![var("x")])),
            emit("Notify", vec![var("x")]),
        ],
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                variable: v,
                previous: PredicateArgKind::Decimal,
                new: PredicateArgKind::Subject,
                ..
            } if v == "x"
        )),
        "expected VariableKindConflict on x; got {errs:?}"
    );
}

#[test]
fn emit_with_arg_kinds_matching_declared_intent_is_clean() {
    // Happy path: emit args agree with intent decl.
    let mut p = empty_program();
    p.predicates = vec![pdecl("P", &[("v", PredicateArgKind::Subject)])];
    p.intents = vec![intent(
        "Notify",
        &[
            ("subject", PredicateArgKind::Subject),
            ("count", PredicateArgKind::Decimal),
        ],
    )];
    p.transformations = vec![transformation(
        "t",
        vec![],
        vec![
            bind_one(claim("P", vec![var("x")])),
            emit("Notify", vec![var("x"), dec("5")]),
        ],
    )];
    let errs = check_program(&p);
    assert!(errs.is_empty(), "well-typed emit should pass; got {errs:?}");
}

// ============================================================
// Or branch independence
// ============================================================
//
// `Or` evaluates each branch against the same context. The check
// mirrors this: a refinement in one branch is invisible to the others
// and does not leak out.

#[test]
fn or_branches_with_disjoint_kind_constraints_do_not_conflict() {
    // `A(x) or B(x)` with A:Decimal, B:Subject - each branch
    // observes x at its own kind independently. Conjunctive
    // logic would flag a conflict; disjunctive logic must not.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("B", &[("v", PredicateArgKind::Subject)]),
    ];
    p.invariants = vec![invariant(
        "or_disjoint",
        or(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "Or branches must check independently; got {errs:?}"
    );
}

#[test]
fn or_branch_refinements_do_not_leak_after_or() {
    // `(A(x) or B(x)) and C(x)`: A refines x to Decimal, B to
    // Subject. `C(x)` (Subject) must not conflict, because `or`
    // exports neither branch's refinement.
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("A", &[("v", PredicateArgKind::Decimal)]),
        pdecl("B", &[("v", PredicateArgKind::Subject)]),
        pdecl("C", &[("v", PredicateArgKind::Subject)]),
    ];
    p.invariants = vec![invariant(
        "or_no_leak",
        and(vec![
            or(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
            claim("C", vec![var("x")]),
        ]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.is_empty(),
        "Or branch refinements must not leak; got {errs:?}"
    );
}

#[test]
fn or_still_walks_branches_for_in_branch_kind_errors() {
    // A literal-vs-slot mismatch inside a branch still surfaces:
    // branches are independent for variables, not for errors.
    let mut p = empty_program();
    p.predicates = vec![pdecl("A", &[("v", PredicateArgKind::Subject)])];
    p.invariants = vec![invariant(
        "or_inner_error",
        or(vec![claim("A", vec![dec("100")])]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::ArgKindMismatch {
                vocabulary: VocabularyKind::Predicate,
                expected: PredicateArgKind::Subject,
                actual: PredicateArgKind::Decimal,
                ..
            }
        )),
        "in-branch literal mismatch must still surface; got {errs:?}"
    );
}

// ============================================================
// Quantifier bindings unify with outer (no shadowing)
// ============================================================
//
// The evaluator does not shadow quantifier bindings: an outer `x`
// reused as a forall / exists / sum binding constrains it. A kind
// mismatch between the two uses is flagged as `VariableKindConflict`.

#[test]
fn forall_with_kind_conflicting_outer_variable_flags_conflict() {
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("S", &[("v", PredicateArgKind::Subject)]),
        pdecl("P", &[("v", PredicateArgKind::Decimal)]),
        pdecl("C", &[("v", PredicateArgKind::Decimal)]),
    ];
    p.invariants = vec![invariant(
        "forall_unify",
        and(vec![
            claim("S", vec![var("x")]),
            forall("x", claim("P", vec![var("x")]), claim("C", vec![var("x")])),
        ]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                variable: v,
                previous: PredicateArgKind::Subject,
                new: PredicateArgKind::Decimal,
                ..
            } if v == "x"
        )),
        "outer x:Subject must conflict with forall source P(x:Decimal); got {errs:?}"
    );
}

#[test]
fn exists_with_kind_conflicting_outer_variable_flags_conflict() {
    let mut p = empty_program();
    p.predicates = vec![
        pdecl("S", &[("v", PredicateArgKind::Subject)]),
        pdecl("D", &[("v", PredicateArgKind::Decimal)]),
    ];
    p.invariants = vec![invariant(
        "exists_unify",
        and(vec![
            claim("S", vec![var("x")]),
            exists("x", claim("D", vec![var("x")])),
        ]),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict {
                variable: v,
                previous: PredicateArgKind::Subject,
                new: PredicateArgKind::Decimal,
                ..
            } if v == "x"
        )),
        "outer x:Subject must conflict with exists body D(x:Decimal); got {errs:?}"
    );
}

#[test]
fn value_of_default_kind_mismatch_flags_operand_mismatch() {
    // ValueOf's default must match the slot's kind. Here the slot is
    // Decimal but the default is a Subject.
    let mut p = empty_program();
    p.predicates = vec![pdecl(
        "Policy",
        &[
            ("policy", PredicateArgKind::Subject),
            ("limit", PredicateArgKind::Decimal),
        ],
    )];
    p.invariants = vec![invariant(
        "bad_default",
        le(
            value_of_with_default(
                "Policy",
                vec![var("p"), wildcard()],
                term(subj("UNLIMITED")),
            ),
            term(dec("100")),
        ),
    )];
    let errs = check_program(&p);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::OperandKindMismatch {
                operator: "value default",
                expected: PredicateArgKind::Decimal,
                actual: PredicateArgKind::Subject,
                ..
            }
        )),
        "expected value-default mismatch; got {errs:?}"
    );
}
