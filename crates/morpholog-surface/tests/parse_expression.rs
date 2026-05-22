//! Integration tests for the v0 expression parser (P2a).
//!
//! Covers: atoms (vars, literals, wildcards, actor, claim calls),
//! arithmetic, comparators, boolean composition, precedence,
//! associativity, the term-only restriction on `!=`.
//!
//! Deferred to P2b and not tested here: bool literals (`true` /
//! `false`), date literals, subject literals, `in`, `exists`,
//! `forall`, `sum`, `value`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Expr, Term, Value};
use morpholog_surface::parse_expression;

// ---- Helpers ----

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

fn dec(s: &str) -> Term {
    Term::Literal(Value::Decimal(s.to_string()))
}

fn dec_expr(s: &str) -> Expr {
    Expr::Term(dec(s))
}

fn var_expr(name: &str) -> Expr {
    Expr::Term(var(name))
}

// ---- Atoms ----

#[test]
fn parses_variable() {
    let got = parse_expression("amount").unwrap();
    assert_eq!(got, var_expr("amount"));
}

#[test]
fn parses_decimal_literal_integer() {
    let got = parse_expression("42").unwrap();
    assert_eq!(got, dec_expr("42"));
}

#[test]
fn parses_decimal_literal_with_point() {
    let got = parse_expression("1250.75").unwrap();
    assert_eq!(got, dec_expr("1250.75"));
}

#[test]
fn parses_actor_as_special_term() {
    let got = parse_expression("actor").unwrap();
    assert_eq!(got, Expr::Term(Term::Actor));
}

#[test]
fn parses_wildcard() {
    let got = parse_expression("_").unwrap();
    assert_eq!(got, Expr::Term(Term::Wildcard));
}

#[test]
fn parses_claim_call_no_args() {
    let got = parse_expression("Marker()").unwrap();
    assert_eq!(
        got,
        Expr::Claim {
            predicate: "Marker".to_string(),
            args: vec![],
        }
    );
}

#[test]
fn parses_claim_call_with_args() {
    let got = parse_expression("Policy(policy_id, limit)").unwrap();
    assert_eq!(
        got,
        Expr::Claim {
            predicate: "Policy".to_string(),
            args: vec![var("policy_id"), var("limit")],
        }
    );
}

#[test]
fn parses_claim_call_with_actor() {
    let got = parse_expression("MayApprove(actor, doc_type)").unwrap();
    assert_eq!(
        got,
        Expr::Claim {
            predicate: "MayApprove".to_string(),
            args: vec![Term::Actor, var("doc_type")],
        }
    );
}

#[test]
fn parses_claim_call_with_wildcards_and_literals() {
    let got = parse_expression("ClaimReported(claim_id, _, 100)").unwrap();
    assert_eq!(
        got,
        Expr::Claim {
            predicate: "ClaimReported".to_string(),
            args: vec![var("claim_id"), Term::Wildcard, dec("100")],
        }
    );
}

#[test]
fn parens_change_grouping() {
    let inner = parse_expression("(amount)").unwrap();
    assert_eq!(inner, var_expr("amount"));
}

// ---- Arithmetic ----

#[test]
fn parses_addition() {
    let got = parse_expression("a + b").unwrap();
    assert_eq!(
        got,
        Expr::Add(Box::new(var_expr("a")), Box::new(var_expr("b")))
    );
}

#[test]
fn parses_subtraction() {
    let got = parse_expression("a - b").unwrap();
    assert_eq!(
        got,
        Expr::Sub(Box::new(var_expr("a")), Box::new(var_expr("b")))
    );
}

#[test]
fn arithmetic_is_left_associative() {
    // `a + b - c` parses as `(a + b) - c`.
    let got = parse_expression("a + b - c").unwrap();
    assert_eq!(
        got,
        Expr::Sub(
            Box::new(Expr::Add(Box::new(var_expr("a")), Box::new(var_expr("b")),)),
            Box::new(var_expr("c")),
        )
    );
}

// ---- Comparators ----

#[test]
fn parses_le_with_decimal_literal() {
    let got = parse_expression("amount <= 100").unwrap();
    assert_eq!(
        got,
        Expr::Le(Box::new(var_expr("amount")), Box::new(dec_expr("100")))
    );
}

#[test]
fn parses_eq() {
    let got = parse_expression("a = b").unwrap();
    assert_eq!(
        got,
        Expr::Eq(Box::new(var_expr("a")), Box::new(var_expr("b")))
    );
}

#[test]
fn parses_neq_between_variables() {
    let got = parse_expression("a != b").unwrap();
    assert_eq!(got, Expr::Neq(var("a"), var("b")));
}

#[test]
fn neq_rejects_arithmetic_lhs() {
    // `Expr::Neq(Term, Term)` cannot represent `a + 1 != b`. The
    // parser must surface a clean diagnostic, not silently
    // produce ill-shaped IR.
    let errs = parse_expression("a + 1 != b").expect_err("term-only restriction should fire");
    assert!(
        errs.iter()
            .any(|d| d.message.contains("!=") && d.message.contains("terms")),
        "expected a diagnostic about the term-only restriction; got: {errs:?}"
    );
}

#[test]
fn arithmetic_binds_tighter_than_comparison() {
    // `a + 5 <= limit` parses as `(a + 5) <= limit`.
    let got = parse_expression("a + 5 <= limit").unwrap();
    assert_eq!(
        got,
        Expr::Le(
            Box::new(Expr::Add(Box::new(var_expr("a")), Box::new(dec_expr("5")),)),
            Box::new(var_expr("limit")),
        )
    );
}

// ---- Boolean composition ----

#[test]
fn parses_not_over_claim() {
    let got = parse_expression("not Netted(line)").unwrap();
    assert_eq!(
        got,
        Expr::Not(Box::new(Expr::Claim {
            predicate: "Netted".to_string(),
            args: vec![var("line")],
        }))
    );
}

#[test]
fn double_negation() {
    let got = parse_expression("not not Done(x)").unwrap();
    assert_eq!(
        got,
        Expr::Not(Box::new(Expr::Not(Box::new(Expr::Claim {
            predicate: "Done".to_string(),
            args: vec![var("x")],
        }))))
    );
}

#[test]
fn parses_and_two_operands() {
    let got = parse_expression("A(x) and B(x)").unwrap();
    let expected = Expr::And(vec![
        Expr::Claim {
            predicate: "A".to_string(),
            args: vec![var("x")],
        },
        Expr::Claim {
            predicate: "B".to_string(),
            args: vec![var("x")],
        },
    ]);
    assert_eq!(got, expected);
}

#[test]
fn and_flattens_three_operands_into_single_vec() {
    // `A and B and C` should be a single `And([A, B, C])`, not
    // `And([And([A, B]), C])`.
    let got = parse_expression("A() and B() and C()").unwrap();
    let Expr::And(operands) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(operands.len(), 3, "expected flat 3-operand And");
}

#[test]
fn not_binds_tighter_than_and() {
    // `not A() and B()` parses as `(not A()) and B()`.
    let got = parse_expression("not A() and B()").unwrap();
    let Expr::And(ops) = &got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Expr::Not(_)));
    assert!(matches!(ops[1], Expr::Claim { .. }));
}

#[test]
fn comparators_bind_tighter_than_not() {
    // `not a <= b` parses as `not (a <= b)`.
    let got = parse_expression("not a <= b").unwrap();
    assert_eq!(
        got,
        Expr::Not(Box::new(Expr::Le(
            Box::new(var_expr("a")),
            Box::new(var_expr("b")),
        )))
    );
}

#[test]
fn and_binds_tighter_than_implies() {
    // `A and B implies C` parses as `(A and B) implies C`.
    let got = parse_expression("A() and B() implies C()").unwrap();
    let Expr::Implies { left, right } = got else {
        panic!("expected Implies");
    };
    assert!(matches!(*left, Expr::And(_)));
    assert!(matches!(*right, Expr::Claim { .. }));
}

#[test]
fn implies_is_right_associative() {
    // `A implies B implies C` parses as `A implies (B implies C)`.
    let got = parse_expression("A() implies B() implies C()").unwrap();
    let Expr::Implies { left, right } = got else {
        panic!("expected Implies");
    };
    assert!(matches!(*left, Expr::Claim { .. }));
    assert!(matches!(*right, Expr::Implies { .. }));
}

// ---- Real-shape examples (from the worked examples) ----

#[test]
fn realistic_insurance_cap_rule() {
    // The cap rule from the insurance settlement example, but
    // simplified to the P2a-supported fragment (no sum yet).
    // `already_paid + proposed <= limit`.
    let got = parse_expression("already_paid + proposed <= limit").unwrap();
    let Expr::Le(lhs, rhs) = got else {
        panic!("expected Le");
    };
    let Expr::Add(a, b) = *lhs else {
        panic!("expected Add on LHS");
    };
    assert_eq!(*a, var_expr("already_paid"));
    assert_eq!(*b, var_expr("proposed"));
    assert_eq!(*rhs, var_expr("limit"));
}

#[test]
fn realistic_netting_require_fragment() {
    // From settlement_netting: the per-line conjunct.
    // `ApprovedSettlementLine(line) and not Netted(line)`.
    let got = parse_expression("ApprovedSettlementLine(line) and not Netted(line)").unwrap();
    let Expr::And(ops) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(
        ops[0],
        Expr::Claim {
            ref predicate,
            ..
        } if predicate == "ApprovedSettlementLine"
    ));
    assert!(matches!(ops[1], Expr::Not(_)));
}

// ---- Error cases ----

#[test]
fn empty_input_is_error() {
    let errs = parse_expression("").expect_err("empty input should fail");
    assert!(!errs.is_empty());
}

#[test]
fn dangling_operator_is_error() {
    let errs = parse_expression("a +").expect_err("dangling operator should fail");
    assert!(!errs.is_empty());
}

#[test]
fn claim_call_with_arithmetic_arg_is_error() {
    // `Foo(x + 1, y)` is not representable: claim args are Terms,
    // not Exprs. The parser rejects with a clean diagnostic.
    let errs =
        parse_expression("Foo(x + 1, y)").expect_err("claim-call arithmetic arg should fail");
    assert!(!errs.is_empty());
}

/// `true` and `false` are reserved at the lexer level but not
/// parseable in v0 (no `Value::Bool` in the IR). They must fail
/// to parse with a clear "unexpected" diagnostic rather than
/// silently lower to `Term::Var("true")` and explode at runtime
/// as `UnboundVariable`. Lifts to bool-literal parsing when a
/// worked example forces `Value::Bool`.
#[test]
fn true_and_false_are_reserved_not_parseable() {
    for source in ["true", "false", "require true", "Le(true, false)"] {
        let errs = parse_expression(source).expect_err("bool literals not parseable in v0");
        assert!(!errs.is_empty());
    }
}

// ============================================================
// P2b-lite: bounded forms + literals + membership
// ============================================================

// ---- Date and subject literals ----

#[test]
fn parses_date_literal_as_term() {
    let got = parse_expression("@2026-05-22").unwrap();
    assert_eq!(
        got,
        Expr::Term(Term::Literal(Value::Date("2026-05-22".to_string())))
    );
}

#[test]
fn parses_subject_literal_as_term() {
    let got = parse_expression("#BANK_DEBT_SERVICE").unwrap();
    assert_eq!(
        got,
        Expr::Term(Term::Literal(Value::Subject(
            "BANK_DEBT_SERVICE".to_string()
        )))
    );
}

#[test]
fn date_literal_in_claim_args() {
    let got = parse_expression("EffectiveFrom(verification, @2026-05-22)").unwrap();
    let Expr::Claim { predicate, args } = got else {
        panic!("expected Claim");
    };
    assert_eq!(predicate, "EffectiveFrom");
    assert_eq!(args[0], Term::Var("verification".to_string()));
    assert_eq!(
        args[1],
        Term::Literal(Value::Date("2026-05-22".to_string()))
    );
}

#[test]
fn subject_literal_in_claim_args() {
    let got = parse_expression("Purpose(asset, #BANK_DEBT_SERVICE)").unwrap();
    let Expr::Claim { predicate, args } = got else {
        panic!("expected Claim");
    };
    assert_eq!(predicate, "Purpose");
    assert_eq!(args[0], Term::Var("asset".to_string()));
    assert_eq!(
        args[1],
        Term::Literal(Value::Subject("BANK_DEBT_SERVICE".to_string()))
    );
}

// ---- Membership comparator (x in xs) ----

#[test]
fn parses_membership_between_variables() {
    let got = parse_expression("line in lines").unwrap();
    assert_eq!(
        got,
        Expr::In(
            Term::Var("line".to_string()),
            Term::Var("lines".to_string()),
        )
    );
}

#[test]
fn in_rejects_arithmetic_operand() {
    // `In(Term, Term)` cannot represent `a + 1 in xs`.
    let errs = parse_expression("a + 1 in xs").expect_err("term-only restriction should fire");
    assert!(
        errs.iter()
            .any(|d| d.message.contains("`in`") && d.message.contains("terms")),
        "expected diagnostic about in-membership term-only restriction; got: {errs:?}"
    );
}

// ---- exists (no source clause) ----

#[test]
fn parses_exists_with_simple_body() {
    let got = parse_expression("exists x: Foo(x)").unwrap();
    assert_eq!(
        got,
        Expr::Exists {
            binding: "x".to_string(),
            body: Box::new(Expr::Claim {
                predicate: "Foo".to_string(),
                args: vec![Term::Var("x".to_string())],
            }),
        }
    );
}

#[test]
fn exists_body_extends_greedily() {
    // `exists x: A(x) and B(x)` -> body is the whole conjunction.
    let got = parse_expression("exists x: A(x) and B(x)").unwrap();
    let Expr::Exists { binding, body } = got else {
        panic!("expected Exists");
    };
    assert_eq!(binding, "x");
    assert!(matches!(*body, Expr::And(_)));
}

// ---- forall with bare-variable source (auto-wrapped as In) ----

#[test]
fn forall_with_variable_source_wraps_as_in() {
    let got = parse_expression("forall line in lines: ApprovedSettlementLine(line)").unwrap();
    let Expr::Forall {
        binding,
        source,
        body,
    } = got
    else {
        panic!("expected Forall");
    };
    assert_eq!(binding, "line");
    // Source must be lifted from `lines` (a Term::Var) to
    // `In(Var("line"), Var("lines"))` so the kernel can iterate.
    assert_eq!(
        *source,
        Expr::In(
            Term::Var("line".to_string()),
            Term::Var("lines".to_string()),
        )
    );
    assert!(matches!(*body, Expr::Claim { .. }));
}

#[test]
fn forall_with_claim_source_used_as_is() {
    let got = parse_expression(
        "forall claim_id in ClaimReported(claim_id, _, amount): AmountPaid(claim_id, amount)",
    )
    .unwrap();
    let Expr::Forall {
        binding,
        source,
        body,
    } = got
    else {
        panic!("expected Forall");
    };
    assert_eq!(binding, "claim_id");
    // Source is a claim, used as-is (not wrapped in In).
    assert!(matches!(*source, Expr::Claim { .. }));
    if let Expr::Claim { predicate, .. } = *source {
        assert_eq!(predicate, "ClaimReported");
    }
    assert!(matches!(*body, Expr::Claim { .. }));
}

#[test]
fn forall_body_extends_greedily() {
    // body is `A and not B`, not just `A`.
    let got =
        parse_expression("forall line in lines: ApprovedSettlementLine(line) and not Netted(line)")
            .unwrap();
    let Expr::Forall { body, .. } = got else {
        panic!("expected Forall");
    };
    assert!(matches!(*body, Expr::And(_)));
}

#[test]
fn nested_forall() {
    // forall x in xs: forall y in ys: P(x, y)
    let got = parse_expression("forall x in xs: forall y in ys: P(x, y)").unwrap();
    let Expr::Forall {
        binding: outer_b,
        body: outer_body,
        ..
    } = got
    else {
        panic!("expected outer Forall");
    };
    assert_eq!(outer_b, "x");
    let Expr::Forall {
        binding: inner_b, ..
    } = *outer_body
    else {
        panic!("expected inner Forall");
    };
    assert_eq!(inner_b, "y");
}

// ---- sum aggregator ----

#[test]
fn parses_sum() {
    let got = parse_expression("sum(amount | SettlementPaid(claim, amount))").unwrap();
    let Expr::Sum {
        value,
        binding,
        body,
    } = got
    else {
        panic!("expected Sum");
    };
    assert_eq!(value, Term::Var("amount".to_string()));
    assert_eq!(binding, "amount");
    assert!(matches!(*body, Expr::Claim { .. }));
}

#[test]
fn sum_body_can_be_compound() {
    // sum(amount | Paid(claim, amount) and not Refunded(claim))
    let got =
        parse_expression("sum(amount | SettlementPaid(claim, amount) and not Refunded(claim))")
            .unwrap();
    let Expr::Sum { body, .. } = got else {
        panic!("expected Sum");
    };
    assert!(matches!(*body, Expr::And(_)));
}

#[test]
fn sum_target_must_be_variable_not_literal() {
    // `sum(5 | ...)` is not valid: parser requires an Ident for
    // the target. The parser fails with an unexpected-token diagnostic.
    let errs = parse_expression("sum(5 | Foo())").expect_err("literal target should fail");
    assert!(!errs.is_empty());
}

#[test]
fn sum_target_must_be_variable_not_wildcard() {
    let errs = parse_expression("sum(_ | Foo())").expect_err("wildcard target should fail");
    assert!(!errs.is_empty());
}

// ---- value lookup ----

#[test]
fn parses_value_without_default() {
    let got = parse_expression("value Policy(policy_id, _)").unwrap();
    let Expr::ValueOf {
        predicate,
        args,
        default,
    } = got
    else {
        panic!("expected ValueOf");
    };
    assert_eq!(predicate, "Policy");
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], Term::Var("policy_id".to_string()));
    assert_eq!(args[1], Term::Wildcard);
    assert!(default.is_none());
}

#[test]
fn parses_value_with_default() {
    let got = parse_expression("value Policy(policy_id, _) default 0").unwrap();
    let Expr::ValueOf {
        predicate, default, ..
    } = got
    else {
        panic!("expected ValueOf");
    };
    assert_eq!(predicate, "Policy");
    let default = default.expect("expected default expr");
    assert_eq!(*default, dec_expr("0"));
}

// ---- Combinations / realistic fragments ----

#[test]
fn realistic_insurance_aggregate_cap() {
    // The insurance cap rule using sum:
    // sum(paid | SettlementPaid(claim, paid)) + proposed <= limit
    let got =
        parse_expression("sum(paid | SettlementPaid(claim, paid)) + proposed <= limit").unwrap();
    let Expr::Le(lhs, rhs) = got else {
        panic!("expected Le");
    };
    let Expr::Add(a, b) = *lhs else {
        panic!("expected Add on LHS of Le");
    };
    assert!(matches!(*a, Expr::Sum { .. }));
    assert_eq!(*b, var_expr("proposed"));
    assert_eq!(*rhs, var_expr("limit"));
}

#[test]
fn realistic_netting_forall() {
    // The forall fragment from settlement_netting:
    // forall line in lines: ApprovedSettlementLine(line) and not Netted(line)
    let got =
        parse_expression("forall line in lines: ApprovedSettlementLine(line) and not Netted(line)")
            .unwrap();
    assert!(matches!(got, Expr::Forall { .. }));
}

#[test]
fn realistic_verified_revenue_admissibility() {
    // From verified_revenue: at least one StandingGrantedBy must
    // exist for an admissible verification.
    // exists g: StandingGrantedBy(verification, purpose, _, g)
    let got = parse_expression("exists g: StandingGrantedBy(verification, purpose, _, g)").unwrap();
    let Expr::Exists { binding, body } = got else {
        panic!("expected Exists");
    };
    assert_eq!(binding, "g");
    assert!(matches!(*body, Expr::Claim { .. }));
}

#[test]
fn realistic_clinical_trial_window_via_claims() {
    // Without DateLe in P2b-lite, the date-window check is
    // represented purely as claim queries. This pins that the
    // claim-args date-literal flow works.
    let got = parse_expression(
        "Protocol(version, @2026-05-22) and InvestigatorDelegation(investigator, @2026-05-22)",
    )
    .unwrap();
    let Expr::And(ops) = got else {
        panic!("expected And");
    };
    assert_eq!(ops.len(), 2);
    // Both operands are claims with date literals in arg position.
    for op in &ops {
        let Expr::Claim { args, .. } = op else {
            panic!("expected Claim");
        };
        assert!(
            args.iter()
                .any(|t| matches!(t, Term::Literal(Value::Date(_)))),
            "expected a date-literal arg"
        );
    }
}

// ---- Quantifier-body greediness boundary ----

#[test]
fn forall_inside_parens_composes_with_outer() {
    // (forall x in xs: P(x)) and Q(z)
    // Without parens, body would greedily consume `and Q(z)`.
    let got = parse_expression("(forall x in xs: P(x)) and Q(z)").unwrap();
    let Expr::And(ops) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Expr::Forall { .. }));
    assert!(matches!(ops[1], Expr::Claim { .. }));
}

// ---- in (membership) vs forall-in (structural) disambiguation ----

#[test]
fn in_as_comparator_inside_forall_body() {
    // forall x in xs: y in zs
    // Outer `in` is structural (binds source); inner `in` is
    // the membership comparator.
    let got = parse_expression("forall x in xs: y in zs").unwrap();
    let Expr::Forall { body, .. } = got else {
        panic!("expected Forall");
    };
    assert!(matches!(*body, Expr::In(_, _)));
}

// ============================================================
// PR #58 review tightenings: forall source restriction +
// strict date literal lexing
// ============================================================

/// The parser must refuse value-shaped primaries in
/// unparenthesised `forall` source position, even though they
/// would parse as `primary` elsewhere. Per the surface doctrine,
/// the kernel's `Forall.source` is predicate-shaped (calls
/// `find_matches`), so the parser cannot let surface syntax
/// produce ill-shaped IR.
#[test]
fn forall_source_rejects_decimal_literal() {
    let errs = parse_expression("forall x in 5: P(x)").expect_err("decimal source should fail");
    assert!(!errs.is_empty());
}

#[test]
fn forall_source_rejects_date_literal() {
    let errs = parse_expression("forall x in @2026-05-22: P(x)")
        .expect_err("date-literal source should fail");
    assert!(!errs.is_empty());
}

#[test]
fn forall_source_rejects_subject_literal() {
    let errs = parse_expression("forall x in #BANK_DEBT_SERVICE: P(x)")
        .expect_err("subject-literal source should fail");
    assert!(!errs.is_empty());
}

#[test]
fn forall_source_rejects_wildcard() {
    let errs = parse_expression("forall x in _: P(x)").expect_err("wildcard source should fail");
    assert!(!errs.is_empty());
}

#[test]
fn forall_source_rejects_value_expression() {
    let errs = parse_expression("forall x in value Foo(_): P(x)")
        .expect_err("value-shaped source should fail");
    assert!(!errs.is_empty());
}

#[test]
fn forall_source_rejects_sum_expression() {
    let errs = parse_expression("forall x in sum(v | Foo(v)): P(x)")
        .expect_err("sum-shaped source should fail");
    assert!(!errs.is_empty());
}

/// Parenthesised sources pass through as-is. The user signalled
/// explicit intent by parenthesising. If they put a value-shaped
/// expression inside parens, the kernel will reject it at runtime;
/// the parser does not second-guess.
#[test]
fn forall_source_accepts_parenthesised_predicate_form() {
    let got = parse_expression("forall x in (x in lines): P(x)").unwrap();
    let Expr::Forall {
        binding, source, ..
    } = got
    else {
        panic!("expected Forall");
    };
    assert_eq!(binding, "x");
    // Source was an explicit `In(_, _)` inside parens; passes
    // through without auto-lift.
    assert!(matches!(*source, Expr::In(_, _)));
}

/// Date-literal lexer is strict about 4-2-2 digit shape. Wrong
/// digit counts surface as lex errors at parse time, not at
/// runtime when the date is interpreted.
#[test]
fn date_literal_requires_exactly_yyyy_mm_dd() {
    // Each of these has the wrong digit count somewhere.
    for source in [
        "@2026-5-22",   // month = 1 digit
        "@2026-05-2",   // day = 1 digit
        "@26-05-22",    // year = 2 digits
        "@20260-05-22", // year = 5 digits
        "@2026-005-22", // month = 3 digits
    ] {
        let errs = parse_expression(source)
            .expect_err(&format!("expected `{source}` to fail strict date lexing"));
        assert!(!errs.is_empty(), "expected diagnostics for `{source}`");
    }
}

#[test]
fn date_literal_strict_shape_accepts_valid() {
    // Sanity: the strict shape still accepts well-formed dates.
    for source in ["@2026-05-22", "@1999-01-01", "@9999-12-31"] {
        let got =
            parse_expression(source).unwrap_or_else(|_| panic!("expected `{source}` to parse"));
        assert!(matches!(got, Expr::Term(Term::Literal(Value::Date(_)))));
    }
}

/// `actor` is reserved as the special term that resolves to the
/// proposing transition's actor (`Term::Actor`). Using it as a
/// binder name in `exists`, `forall`, or as a `sum` target would
/// silently change its meaning - references inside the body would
/// either always resolve to `Term::Actor` or to a regular
/// `Term::Var("actor")` depending on parse path. The parser
/// refuses these cases with clear diagnostics.
#[test]
fn exists_rejects_actor_as_binder() {
    let errs = parse_expression("exists actor: Foo(actor)")
        .expect_err("actor as exists binder should fail");
    assert!(
        errs.iter().any(|d| d.message.contains("`actor`")),
        "expected actor-binder diagnostic; got: {errs:?}"
    );
}

#[test]
fn forall_rejects_actor_as_binder() {
    let errs = parse_expression("forall actor in actors: Foo(actor)")
        .expect_err("actor as forall binder should fail");
    assert!(
        errs.iter().any(|d| d.message.contains("`actor`")),
        "expected actor-binder diagnostic; got: {errs:?}"
    );
}

#[test]
fn sum_rejects_actor_as_target() {
    let errs = parse_expression("sum(actor | MayApprove(actor, _))")
        .expect_err("actor as sum target should fail");
    assert!(
        errs.iter().any(|d| d.message.contains("`actor`")),
        "expected actor-as-sum-target diagnostic; got: {errs:?}"
    );
}

/// Quantifier bodies (exists, forall) accept indented bodies via
/// the `(Indent body Dedent | body)` choice. P3a-era tests only
/// exercised the inline form; this pins the indented-body path
/// and the layout pass's interaction with nested quantifier
/// scoping.
#[test]
fn forall_body_can_be_indented_on_next_line() {
    let source = "program demo\n\
                  invariant cap:\n\
                  \x20\x20\x20\x20forall x in xs:\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20Foo(x)\n";
    let program =
        morpholog_surface::parse_program(source).expect("indented quantifier body should parse");
    let body = &program.invariants[0].body;
    assert!(matches!(body, Expr::Forall { .. }));
}
