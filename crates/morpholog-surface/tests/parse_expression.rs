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
