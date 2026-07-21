//! Integration tests for the v0 expression parsers.
//!
//! Covers: atoms (vars, literals, wildcards, actor, claim calls),
//! arithmetic, comparators, boolean composition, precedence,
//! associativity, the term-only restriction on `in`.
//!
//! The two-sort split means tests target the right entry point:
//! proposition-shaped surface (`require`/invariant bodies - claims,
//! comparators, boolean composition, quantifiers, `pre`) goes through
//! `parse_expression`, which returns a [`Prop`]; value-shaped surface
//! (a bare variable / literal / `actor` / `_`, arithmetic, `sum`,
//! `value`) goes through `parse_value_expr`, which returns a
//! [`ValueExpr`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::format_prop_inline;
use morpholog_core::{ArithOp, CompareOp, OrderedDomain, Prop, Term, Value, ValueExpr};

/// Build a `Prop::Compare` for the assertions below (the eight comparator
/// variants were collapsed into one `Compare { op, domain }`).
fn cmp(op: CompareOp, domain: OrderedDomain, l: ValueExpr, r: ValueExpr) -> Prop {
    Prop::Compare {
        op,
        domain,
        left: Box::new(l),
        right: Box::new(r),
    }
}

/// Build a `ValueExpr::Arith` for the assertions below (the per-operator
/// arithmetic variants were collapsed into one `Arith { op, .. }`).
fn arith(op: ArithOp, l: ValueExpr, r: ValueExpr) -> ValueExpr {
    ValueExpr::Arith {
        op,
        left: Box::new(l),
        right: Box::new(r),
    }
}
use morpholog_surface::{parse_expression, parse_value_expr};

// ---- Helpers ----

fn var(name: &str) -> Term {
    Term::Var(name.into())
}

fn dec(s: &str) -> Term {
    Term::Literal(Value::Decimal(s.to_string()))
}

fn dec_value(s: &str) -> ValueExpr {
    ValueExpr::Term(dec(s))
}

fn var_value(name: &str) -> ValueExpr {
    ValueExpr::Term(var(name))
}

// ---- Test machinery: one line per uniform parse case ----
//
// `value_ok!`/`prop_ok!` pin `parse -> assert_eq` cases; `*_err!` pin
// inputs the parser must reject. Tests that inspect only part of the
// tree (via `matches!` / `let-else`) or assert a specific diagnostic
// stay written out in full below.
macro_rules! value_ok {
    ($name:ident, $src:expr, $expected:expr) => {
        #[test]
        fn $name() {
            assert_eq!(
                parse_value_expr($src).unwrap(),
                $expected,
                "source: {}",
                $src
            );
        }
    };
}
macro_rules! prop_ok {
    ($name:ident, $src:expr, $expected:expr) => {
        #[test]
        fn $name() {
            assert_eq!(
                parse_expression($src).unwrap(),
                $expected,
                "source: {}",
                $src
            );
        }
    };
}
macro_rules! prop_err {
    ($name:ident, $src:expr) => {
        #[test]
        fn $name() {
            assert!(
                !parse_expression($src).unwrap_err().is_empty(),
                "source: {}",
                $src
            );
        }
    };
}
macro_rules! value_err {
    ($name:ident, $src:expr) => {
        #[test]
        fn $name() {
            assert!(
                !parse_value_expr($src).unwrap_err().is_empty(),
                "source: {}",
                $src
            );
        }
    };
}

// ---- Atoms (value-shaped: parse_value_expr) ----

value_ok!(parses_variable, "amount", var_value("amount"));
value_ok!(parses_decimal_literal_integer, "42", dec_value("42"));
value_ok!(
    parses_decimal_literal_with_point,
    "1250.75",
    dec_value("1250.75")
);
value_ok!(
    parses_actor_as_special_term,
    "actor",
    ValueExpr::Term(Term::Actor)
);
value_ok!(parses_wildcard, "_", ValueExpr::Term(Term::Wildcard));

// ---- Claim calls (proposition-shaped: parse_expression) ----

prop_ok!(
    parses_claim_call_no_args,
    "Marker()",
    Prop::Claim {
        predicate: "Marker".into(),
        args: vec![]
    }
);
prop_ok!(
    parses_claim_call_with_args,
    "Policy(policy_id, limit)",
    Prop::Claim {
        predicate: "Policy".into(),
        args: vec![var("policy_id"), var("limit")]
    }
);
prop_ok!(
    parses_claim_call_with_actor,
    "MayApprove(actor, doc_type)",
    Prop::Claim {
        predicate: "MayApprove".into(),
        args: vec![Term::Actor, var("doc_type")]
    }
);
prop_ok!(
    parses_claim_call_with_wildcards_and_literals,
    "ClaimReported(claim_id, _, 100)",
    Prop::Claim {
        predicate: "ClaimReported".into(),
        args: vec![var("claim_id"), Term::Wildcard, dec("100")]
    }
);
// A parenthesised value expression in value position.
value_ok!(parens_change_grouping, "(amount)", var_value("amount"));

// ---- Arithmetic (value-shaped) ----

value_ok!(
    parses_addition,
    "a + b",
    arith(ArithOp::Add, var_value("a"), var_value("b"))
);
value_ok!(
    parses_subtraction,
    "a - b",
    arith(ArithOp::Sub, var_value("a"), var_value("b"))
);
// `a + b - c` parses as `(a + b) - c`.
value_ok!(
    arithmetic_is_left_associative,
    "a + b - c",
    arith(
        ArithOp::Sub,
        arith(ArithOp::Add, var_value("a"), var_value("b")),
        var_value("c")
    )
);
value_ok!(
    parses_multiplication,
    "a * b",
    arith(ArithOp::Mul, var_value("a"), var_value("b"))
);
value_ok!(
    parses_division,
    "a / b",
    arith(ArithOp::Div, var_value("a"), var_value("b"))
);
// `a + b * c` parses as `Add(a, Mul(b, c))` - the multiplicative layer
// binds tighter than the additive one.
value_ok!(
    multiplication_binds_tighter_than_addition,
    "a + b * c",
    arith(
        ArithOp::Add,
        var_value("a"),
        arith(ArithOp::Mul, var_value("b"), var_value("c"))
    )
);
// `a / b / c` parses as `Div(Div(a, b), c)`.
value_ok!(
    division_is_left_associative,
    "a / b / c",
    arith(
        ArithOp::Div,
        arith(ArithOp::Div, var_value("a"), var_value("b")),
        var_value("c")
    )
);
// `a * b / c` parses as `Div(Mul(a, b), c)` - same level, left-assoc.
value_ok!(
    mul_and_div_share_one_precedence_level,
    "a * b / c",
    arith(
        ArithOp::Div,
        arith(ArithOp::Mul, var_value("a"), var_value("b")),
        var_value("c")
    )
);
value_ok!(
    parses_modulo,
    "a % b",
    arith(ArithOp::Mod, var_value("a"), var_value("b"))
);
// `a + b % c` parses as `Add(a, Mod(b, c))` - `%` binds with `*`/`/`,
// tighter than `+`. The parity shape `(file + rank) % 2` relies on the
// explicit parens, since `+` is the looser operator.
value_ok!(
    modulo_shares_multiplicative_precedence,
    "a + b % c",
    arith(
        ArithOp::Add,
        var_value("a"),
        arith(ArithOp::Mod, var_value("b"), var_value("c"))
    )
);
value_ok!(
    parses_min,
    "min(a, b)",
    arith(ArithOp::Min, var_value("a"), var_value("b"))
);
// `max(0, a - b)` - the second arg is a full value expression.
value_ok!(
    parses_max_with_arithmetic_arg,
    "max(0, a - b)",
    arith(
        ArithOp::Max,
        dec_value("0"),
        arith(ArithOp::Sub, var_value("a"), var_value("b"))
    )
);
// `min(cap, max(floor, x))` - the collar shape.
value_ok!(
    parses_nested_min_max,
    "min(cap, max(floor, x))",
    arith(
        ArithOp::Min,
        var_value("cap"),
        arith(ArithOp::Max, var_value("floor"), var_value("x"))
    )
);
value_ok!(
    parses_abs,
    "abs(x)",
    ValueExpr::Abs(Box::new(var_value("x")))
);

#[test]
fn parses_abs_of_a_sum() {
    // `abs(sum(q | P(q)))` - the two-sided-bound shape: the operand is a
    // full value expression, evaluated once.
    let got = parse_value_expr("abs(sum(q | Position(q)))").unwrap();
    let ValueExpr::Abs(operand) = got else {
        panic!("expected abs, got {got:?}");
    };
    assert!(matches!(*operand, ValueExpr::Sum { .. }));
}

// ---- Comparators ----

prop_ok!(
    parses_le_with_decimal_literal,
    "amount <= 100",
    cmp(
        CompareOp::Le,
        OrderedDomain::Decimal,
        var_value("amount"),
        dec_value("100")
    )
);
prop_ok!(
    parses_eq,
    "a = b",
    Prop::Eq(Box::new(var_value("a")), Box::new(var_value("b")))
);
prop_ok!(
    parses_neq_between_variables,
    "a != b",
    Prop::Neq(Box::new(var_value("a")), Box::new(var_value("b")))
);
// `!=` is symmetric with `=`: `Prop::Neq` takes full expressions, so
// `a + 1 != b` parses to `Neq(Add(a, 1), b)` rather than being rejected
// as it was when Neq operated on terms only.
prop_ok!(
    neq_accepts_arithmetic_operand,
    "a + 1 != b",
    Prop::Neq(
        Box::new(arith(ArithOp::Add, var_value("a"), dec_value("1"))),
        Box::new(var_value("b"))
    )
);
// `a + 5 <= limit` parses as `(a + 5) <= limit`.
prop_ok!(
    arithmetic_binds_tighter_than_comparison,
    "a + 5 <= limit",
    cmp(
        CompareOp::Le,
        OrderedDomain::Decimal,
        arith(ArithOp::Add, var_value("a"), dec_value("5")),
        var_value("limit")
    )
);

// ---- Boolean composition ----

#[test]
fn parses_not_over_claim() {
    let got = parse_expression("not Netted(line)").unwrap();
    assert_eq!(
        got,
        Prop::Not(Box::new(Prop::Claim {
            predicate: "Netted".into(),
            args: vec![var("line")],
        }))
    );
}

#[test]
fn double_negation() {
    let got = parse_expression("not not Done(x)").unwrap();
    assert_eq!(
        got,
        Prop::Not(Box::new(Prop::Not(Box::new(Prop::Claim {
            predicate: "Done".into(),
            args: vec![var("x")],
        }))))
    );
}

#[test]
fn parses_and_two_operands() {
    let got = parse_expression("A(x) and B(x)").unwrap();
    let expected = Prop::And(vec![
        Prop::Claim {
            predicate: "A".into(),
            args: vec![var("x")],
        },
        Prop::Claim {
            predicate: "B".into(),
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
    let Prop::And(operands) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(operands.len(), 3, "expected flat 3-operand And");
}

#[test]
fn not_binds_tighter_than_and() {
    // `not A() and B()` parses as `(not A()) and B()`.
    let got = parse_expression("not A() and B()").unwrap();
    let Prop::And(ops) = &got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Prop::Not(_)));
    assert!(matches!(ops[1], Prop::Claim { .. }));
}

#[test]
fn comparators_bind_tighter_than_not() {
    // `not a <= b` parses as `not (a <= b)`.
    let got = parse_expression("not a <= b").unwrap();
    assert_eq!(
        got,
        Prop::Not(Box::new(cmp(
            CompareOp::Le,
            OrderedDomain::Decimal,
            var_value("a"),
            var_value("b"),
        )))
    );
}

#[test]
fn and_binds_tighter_than_implies() {
    // `A and B implies C` parses as `(A and B) implies C`.
    let got = parse_expression("A() and B() implies C()").unwrap();
    let Prop::Implies { left, right } = got else {
        panic!("expected Implies");
    };
    assert!(matches!(*left, Prop::And(_)));
    assert!(matches!(*right, Prop::Claim { .. }));
}

#[test]
fn parses_or_two_operands() {
    let got = parse_expression("A(x) or B(x)").unwrap();
    let expected = Prop::Or(vec![
        Prop::Claim {
            predicate: "A".into(),
            args: vec![var("x")],
        },
        Prop::Claim {
            predicate: "B".into(),
            args: vec![var("x")],
        },
    ]);
    assert_eq!(got, expected);
}

#[test]
fn or_flattens_three_operands_into_single_vec() {
    // `A or B or C` should be a single `Or([A, B, C])`, not
    // `Or([Or([A, B]), C])`. Mirrors `and_flattens_three_operands_...`
    // for the And flattening.
    let got = parse_expression("A() or B() or C()").unwrap();
    let Prop::Or(operands) = got else {
        panic!("expected Or, got {got:?}");
    };
    assert_eq!(operands.len(), 3, "expected flat 3-operand Or");
}

#[test]
fn and_binds_tighter_than_or() {
    // `A and B or C` parses as `(A and B) or C` (standard logical
    // precedence). The disjunction's first branch is an And, the
    // second is a leaf Claim.
    let got = parse_expression("A() and B() or C()").unwrap();
    let Prop::Or(ops) = got else {
        panic!("expected Or, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Prop::And(_)));
    assert!(matches!(ops[1], Prop::Claim { .. }));
}

#[test]
fn parses_xor_two_operands() {
    let got = parse_expression("A(x) xor B(x)").unwrap();
    let expected = Prop::Xor(
        Box::new(Prop::Claim {
            predicate: "A".into(),
            args: vec![var("x")],
        }),
        Box::new(Prop::Claim {
            predicate: "B".into(),
            args: vec![var("x")],
        }),
    );
    assert_eq!(got, expected);
}

#[test]
fn xor_does_not_flatten_it_nests() {
    // Unlike `and`/`or`, `xor` is binary: `A xor B xor C` nests
    // left-associatively into `Xor(Xor(A, B), C)`, not a flat node.
    let got = parse_expression("A() xor B() xor C()").unwrap();
    let Prop::Xor(left, right) = got else {
        panic!("expected Xor, got {got:?}");
    };
    assert!(matches!(*left, Prop::Xor(_, _)), "left should nest a Xor");
    assert!(matches!(*right, Prop::Claim { .. }));
}

#[test]
fn and_binds_tighter_than_xor() {
    // `A and B xor C and D` parses as `(A and B) xor (C and D)` - the
    // natural "exactly one of these two conjunctions" reading.
    let got = parse_expression("A() and B() xor C() and D()").unwrap();
    let Prop::Xor(left, right) = got else {
        panic!("expected Xor, got {got:?}");
    };
    assert!(matches!(*left, Prop::And(_)));
    assert!(matches!(*right, Prop::And(_)));
}

#[test]
fn xor_binds_tighter_than_or() {
    // `A xor B or C` parses as `(A xor B) or C`: xor sits between and
    // and or, so the disjunction's first branch is the Xor.
    let got = parse_expression("A() xor B() or C()").unwrap();
    let Prop::Or(ops) = got else {
        panic!("expected Or, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Prop::Xor(_, _)));
    assert!(matches!(ops[1], Prop::Claim { .. }));
}

#[test]
fn or_binds_tighter_than_implies() {
    // `A or B implies C` parses as `(A or B) implies C`.
    let got = parse_expression("A() or B() implies C()").unwrap();
    let Prop::Implies { left, right } = got else {
        panic!("expected Implies");
    };
    assert!(matches!(*left, Prop::Or(_)));
    assert!(matches!(*right, Prop::Claim { .. }));
}

#[test]
fn not_binds_tighter_than_or() {
    // `not A() or B()` parses as `(not A()) or B()`.
    let got = parse_expression("not A() or B()").unwrap();
    let Prop::Or(ops) = &got else {
        panic!("expected Or, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Prop::Not(_)));
    assert!(matches!(ops[1], Prop::Claim { .. }));
}

#[test]
fn parses_pre_over_claim() {
    let got = parse_expression("pre(Balance(a, b))").unwrap();
    let expected = Prop::Pre(Box::new(Prop::Claim {
        predicate: "Balance".into(),
        args: vec![var("a"), var("b")],
    }));
    assert_eq!(got, expected);
}

#[test]
fn pre_composes_with_and_inside() {
    // `pre(A(x) and B(x))` parses as Pre wrapping the And.
    let got = parse_expression("pre(A(x) and B(x))").unwrap();
    let Prop::Pre(inner) = got else {
        panic!("expected Pre, got {got:?}");
    };
    assert!(matches!(*inner, Prop::And(_)));
}

#[test]
fn pre_at_primary_level_composes_with_outer_and() {
    // `pre(A(x)) and B(x)` parses as And([Pre(A(x)), B(x)])
    // because `pre(...)` is a function-call-shape primary, no
    // outer parens needed.
    let got = parse_expression("pre(A(x)) and B(x)").unwrap();
    let Prop::And(ops) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Prop::Pre(_)));
    assert!(matches!(ops[1], Prop::Claim { .. }));
}

#[test]
fn pre_inside_implies_with_disjunction() {
    // The textbook chess SingleCapturePerMove shape:
    // `PieceCount(after) and pre(PieceCount(before))
    //  implies (after = before) or (after = before - 1)`
    let source = "PieceCount(after) and pre(PieceCount(before)) \
                  implies after = before or after = before - 1";
    let got = parse_expression(source).unwrap();
    let Prop::Implies { left, right } = got else {
        panic!("expected Implies, got {got:?}");
    };
    assert!(matches!(*left, Prop::And(_)));
    assert!(matches!(*right, Prop::Or(_)));
}

#[test]
fn pre_value_position_inside_sum() {
    // pre composes with Sum's body: `sum(amount | pre(Posting(_, amount)))`
    // counts amounts that were in pre-state.
    let got = parse_value_expr("sum(amount | pre(Posting(_, amount)))").unwrap();
    let ValueExpr::Sum { body, .. } = got else {
        panic!("expected Sum, got {got:?}");
    };
    assert!(matches!(*body, Prop::Pre(_)));
}

/// Round-trip property over a mixed-precedence boolean expression:
/// parse, format, parse again, and the IR must be unchanged. Pins the
/// formatter's behaviour for `Or` operands that are themselves
/// composite (an `And`, an `Implies`) - they must be parenthesised so
/// the surface text reparses to the original tree, not a precedence-
/// reshuffled one.
///
/// The kernel-wide `every_worked_example_round_trips` test will cover
/// this transitively once a worked example uses `or`; until then, this
/// is the local pin.
#[test]
fn formatter_preserves_mixed_and_or_implies_precedence() {
    // `A and B or C implies D` parses as `((A and B) or C) implies D`
    // under the standard precedence (and > or > implies).
    let source = "A() and B() or C() implies D()";
    let parsed = parse_expression(source).unwrap();
    let formatted = format_prop_inline(&parsed);
    let reparsed = parse_expression(&formatted).unwrap_or_else(|errs| {
        panic!("formatted text did not reparse: {formatted}\nerrors: {errs:?}")
    });
    assert_eq!(
        reparsed, parsed,
        "formatter must round-trip mixed and/or/implies precedence; formatted text was: {formatted}"
    );
}

#[test]
fn implies_is_right_associative() {
    // `A implies B implies C` parses as `A implies (B implies C)`.
    let got = parse_expression("A() implies B() implies C()").unwrap();
    let Prop::Implies { left, right } = got else {
        panic!("expected Implies");
    };
    assert!(matches!(*left, Prop::Claim { .. }));
    assert!(matches!(*right, Prop::Implies { .. }));
}

// ---- Real-shape examples (from the worked examples) ----

#[test]
fn realistic_insurance_cap_rule() {
    // The cap rule from the insurance settlement example, but
    // simplified (no sum yet (no sum yet).
    // `already_paid + proposed <= limit`.
    let got = parse_expression("already_paid + proposed <= limit").unwrap();
    let Prop::Compare {
        left: lhs,
        right: rhs,
        ..
    } = got
    else {
        panic!("expected Le");
    };
    let ValueExpr::Arith {
        op: ArithOp::Add,
        left: a,
        right: b,
    } = *lhs
    else {
        panic!("expected Add on LHS");
    };
    assert_eq!(*a, var_value("already_paid"));
    assert_eq!(*b, var_value("proposed"));
    assert_eq!(*rhs, var_value("limit"));
}

#[test]
fn realistic_netting_require_fragment() {
    // From settlement_netting: the per-line conjunct.
    // `ApprovedSettlementLine(line) and not Netted(line)`.
    let got = parse_expression("ApprovedSettlementLine(line) and not Netted(line)").unwrap();
    let Prop::And(ops) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(
        ops[0],
        Prop::Claim {
            ref predicate,
            ..
        } if predicate.as_str() == "ApprovedSettlementLine"
    ));
    assert!(matches!(ops[1], Prop::Not(_)));
}

// ---- Error cases ----

prop_err!(empty_input_is_error, "");
prop_err!(dangling_operator_is_error, "a +");
// `Foo(x + 1, y)` is not representable: claim args are Terms, not Exprs.
prop_err!(claim_call_with_arithmetic_arg_is_error, "Foo(x + 1, y)");

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
// Bounded forms + literals + membership
// ============================================================

// ---- Date and subject literals ----

#[test]
fn parses_date_literal_as_term() {
    let got = parse_value_expr("@2026-05-22").unwrap();
    assert_eq!(
        got,
        ValueExpr::Term(Term::Literal(Value::Date("2026-05-22".to_string())))
    );
}

#[test]
fn parses_subject_literal_as_term() {
    let got = parse_value_expr("#BANK_DEBT_SERVICE").unwrap();
    assert_eq!(
        got,
        ValueExpr::Term(Term::Literal(Value::Subject("BANK_DEBT_SERVICE".into())))
    );
}

#[test]
fn date_literal_in_claim_args() {
    let got = parse_expression("EffectiveFrom(verification, @2026-05-22)").unwrap();
    let Prop::Claim { predicate, args } = got else {
        panic!("expected Claim");
    };
    assert_eq!(predicate.as_str(), "EffectiveFrom");
    assert_eq!(args[0], Term::Var("verification".into()));
    assert_eq!(
        args[1],
        Term::Literal(Value::Date("2026-05-22".to_string()))
    );
}

#[test]
fn subject_literal_in_claim_args() {
    let got = parse_expression("Purpose(asset, #BANK_DEBT_SERVICE)").unwrap();
    let Prop::Claim { predicate, args } = got else {
        panic!("expected Claim");
    };
    assert_eq!(predicate.as_str(), "Purpose");
    assert_eq!(args[0], Term::Var("asset".into()));
    assert_eq!(
        args[1],
        Term::Literal(Value::Subject("BANK_DEBT_SERVICE".into()))
    );
}

// ---- Membership comparator (x in xs) ----

#[test]
fn parses_membership_between_variables() {
    let got = parse_expression("line in lines").unwrap();
    assert_eq!(
        got,
        Prop::In(Term::Var("line".into()), Term::Var("lines".into()),)
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
        Prop::Exists {
            binding: "x".into(),
            body: Box::new(Prop::Claim {
                predicate: "Foo".into(),
                args: vec![Term::Var("x".into())],
            }),
        }
    );
}

#[test]
fn exists_body_extends_greedily() {
    // `exists x: A(x) and B(x)` -> body is the whole conjunction.
    let got = parse_expression("exists x: A(x) and B(x)").unwrap();
    let Prop::Exists { binding, body } = got else {
        panic!("expected Exists");
    };
    assert_eq!(binding.as_str(), "x");
    assert!(matches!(*body, Prop::And(_)));
}

// ---- forall with bare-variable source (auto-wrapped as In) ----

#[test]
fn forall_with_variable_source_wraps_as_in() {
    let got = parse_expression("forall line in lines: ApprovedSettlementLine(line)").unwrap();
    let Prop::Forall {
        binding,
        source,
        body,
    } = got
    else {
        panic!("expected Forall");
    };
    assert_eq!(binding.as_str(), "line");
    // Source must be lifted from `lines` (a Term::Var) to
    // `In(Var("line"), Var("lines"))` so the kernel can iterate.
    assert_eq!(
        *source,
        Prop::In(Term::Var("line".into()), Term::Var("lines".into()),)
    );
    assert!(matches!(*body, Prop::Claim { .. }));
}

#[test]
fn forall_with_claim_source_used_as_is() {
    let got = parse_expression(
        "forall claim_id in ClaimReported(claim_id, _, amount): AmountPaid(claim_id, amount)",
    )
    .unwrap();
    let Prop::Forall {
        binding,
        source,
        body,
    } = got
    else {
        panic!("expected Forall");
    };
    assert_eq!(binding.as_str(), "claim_id");
    // Source is a claim, used as-is (not wrapped in In).
    assert!(matches!(*source, Prop::Claim { .. }));
    if let Prop::Claim { predicate, .. } = *source {
        assert_eq!(predicate.as_str(), "ClaimReported");
    }
    assert!(matches!(*body, Prop::Claim { .. }));
}

#[test]
fn forall_body_extends_greedily() {
    // body is `A and not B`, not just `A`.
    let got =
        parse_expression("forall line in lines: ApprovedSettlementLine(line) and not Netted(line)")
            .unwrap();
    let Prop::Forall { body, .. } = got else {
        panic!("expected Forall");
    };
    assert!(matches!(*body, Prop::And(_)));
}

#[test]
fn nested_forall() {
    // forall x in xs: forall y in ys: P(x, y)
    let got = parse_expression("forall x in xs: forall y in ys: P(x, y)").unwrap();
    let Prop::Forall {
        binding: outer_b,
        body: outer_body,
        ..
    } = got
    else {
        panic!("expected outer Forall");
    };
    assert_eq!(outer_b.as_str(), "x");
    let Prop::Forall {
        binding: inner_b, ..
    } = *outer_body
    else {
        panic!("expected inner Forall");
    };
    assert_eq!(inner_b.as_str(), "y");
}

// ---- sum aggregator ----

#[test]
fn parses_sum() {
    let got = parse_value_expr("sum(amount | SettlementPaid(claim, amount))").unwrap();
    let ValueExpr::Sum {
        value,
        body,
        seed: _,
    } = got
    else {
        panic!("expected Sum");
    };
    assert_eq!(value, Term::Var("amount".into()));
    assert!(matches!(*body, Prop::Claim { .. }));
}

#[test]
fn sum_body_can_be_compound() {
    // sum(amount | Paid(claim, amount) and not Refunded(claim))
    let got =
        parse_value_expr("sum(amount | SettlementPaid(claim, amount) and not Refunded(claim))")
            .unwrap();
    let ValueExpr::Sum { body, .. } = got else {
        panic!("expected Sum");
    };
    assert!(matches!(*body, Prop::And(_)));
}

#[test]
fn sum_target_can_be_a_decimal_literal_for_counting() {
    // `sum(1 | ...)` counts matches: the target is the literal 1, added
    // once per match. The parser accepts a decimal-literal target
    // alongside a variable.
    let got = parse_value_expr("sum(1 | Foo())").expect("literal target should parse");
    let ValueExpr::Sum { value, .. } = got else {
        panic!("expected Sum, got {got:?}");
    };
    assert_eq!(value, Term::Literal(Value::Decimal("1".to_string())));
}

value_err!(sum_target_must_be_variable_not_wildcard, "sum(_ | Foo())");

// ---- comparators ----

#[test]
fn parses_decimal_strict_comparators() {
    assert!(matches!(
        parse_expression("a < b").unwrap(),
        Prop::Compare {
            op: CompareOp::Lt,
            domain: OrderedDomain::Decimal,
            ..
        }
    ));
    assert!(matches!(
        parse_expression("a > b").unwrap(),
        Prop::Compare {
            op: CompareOp::Gt,
            domain: OrderedDomain::Decimal,
            ..
        }
    ));
    assert!(matches!(
        parse_expression("a >= b").unwrap(),
        Prop::Compare {
            op: CompareOp::Ge,
            domain: OrderedDomain::Decimal,
            ..
        }
    ));
    // The point of first-class comparators: each round-trips as written,
    // never as `not (a <= b)`.
    for src in ["a < b", "a > b", "a >= b", "a <= b"] {
        assert_eq!(format_prop_inline(&parse_expression(src).unwrap()), src);
    }
}

#[test]
fn parses_date_strict_comparators() {
    assert!(matches!(
        parse_expression("d1 before d2").unwrap(),
        Prop::Compare {
            op: CompareOp::Lt,
            domain: OrderedDomain::Date,
            ..
        }
    ));
    assert!(matches!(
        parse_expression("d1 after d2").unwrap(),
        Prop::Compare {
            op: CompareOp::Gt,
            domain: OrderedDomain::Date,
            ..
        }
    ));
    assert!(matches!(
        parse_expression("d1 on_or_after d2").unwrap(),
        Prop::Compare {
            op: CompareOp::Ge,
            domain: OrderedDomain::Date,
            ..
        }
    ));
    for src in [
        "d1 before d2",
        "d1 after d2",
        "d1 on_or_after d2",
        "d1 on_or_before d2",
    ] {
        assert_eq!(format_prop_inline(&parse_expression(src).unwrap()), src);
    }
}

#[test]
fn before_and_after_remain_usable_as_variable_names() {
    // `before`/`after` are contextual comparators, not reserved words:
    // in argument position they are ordinary variables, which the chess
    // and insurance examples rely on.
    let got = parse_expression("Headroom(before, after)").unwrap();
    let Prop::Claim { args, .. } = got else {
        panic!("expected Claim, got {got:?}");
    };
    assert_eq!(
        args,
        vec![Term::Var("before".into()), Term::Var("after".into()),]
    );
}

// ---- value lookup ----

#[test]
fn parses_value_without_default() {
    let got = parse_value_expr("value Policy(policy_id, _)").unwrap();
    let ValueExpr::ValueOf {
        predicate,
        args,
        default,
    } = got
    else {
        panic!("expected ValueOf");
    };
    assert_eq!(predicate.as_str(), "Policy");
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], Term::Var("policy_id".into()));
    assert_eq!(args[1], Term::Wildcard);
    assert!(default.is_none());
}

#[test]
fn parses_value_with_default() {
    let got = parse_value_expr("value Policy(policy_id, _) default 0").unwrap();
    let ValueExpr::ValueOf {
        predicate, default, ..
    } = got
    else {
        panic!("expected ValueOf");
    };
    assert_eq!(predicate.as_str(), "Policy");
    let default = default.expect("expected default expr");
    assert_eq!(*default, dec_value("0"));
}

// ---- Combinations / realistic fragments ----

#[test]
fn realistic_insurance_aggregate_cap() {
    // The insurance cap rule using sum:
    // sum(paid | SettlementPaid(claim, paid)) + proposed <= limit
    let got =
        parse_expression("sum(paid | SettlementPaid(claim, paid)) + proposed <= limit").unwrap();
    let Prop::Compare {
        left: lhs,
        right: rhs,
        ..
    } = got
    else {
        panic!("expected Le");
    };
    let ValueExpr::Arith {
        op: ArithOp::Add,
        left: a,
        right: b,
    } = *lhs
    else {
        panic!("expected Add on LHS of Le");
    };
    assert!(matches!(*a, ValueExpr::Sum { .. }));
    assert_eq!(*b, var_value("proposed"));
    assert_eq!(*rhs, var_value("limit"));
}

#[test]
fn realistic_netting_forall() {
    // The forall fragment from settlement_netting:
    // forall line in lines: ApprovedSettlementLine(line) and not Netted(line)
    let got =
        parse_expression("forall line in lines: ApprovedSettlementLine(line) and not Netted(line)")
            .unwrap();
    assert!(matches!(got, Prop::Forall { .. }));
}

#[test]
fn realistic_verified_revenue_admissibility() {
    // From verified_revenue: at least one StandingGrantedBy must
    // exist for an admissible verification.
    // exists g: StandingGrantedBy(verification, purpose, _, g)
    let got = parse_expression("exists g: StandingGrantedBy(verification, purpose, _, g)").unwrap();
    let Prop::Exists { binding, body } = got else {
        panic!("expected Exists");
    };
    assert_eq!(binding.as_str(), "g");
    assert!(matches!(*body, Prop::Claim { .. }));
}

#[test]
fn realistic_clinical_trial_window_via_claims() {
    // Before on_or_before existed in the surface, the date-window check is
    // represented purely as claim queries. This pins that the
    // claim-args date-literal flow works.
    let got = parse_expression(
        "Protocol(version, @2026-05-22) and InvestigatorDelegation(investigator, @2026-05-22)",
    )
    .unwrap();
    let Prop::And(ops) = got else {
        panic!("expected And");
    };
    assert_eq!(ops.len(), 2);
    // Both operands are claims with date literals in arg position.
    for op in &ops {
        let Prop::Claim { args, .. } = op else {
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
    let Prop::And(ops) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    assert!(matches!(ops[0], Prop::Forall { .. }));
    assert!(matches!(ops[1], Prop::Claim { .. }));
}

// ---- in (membership) vs forall-in (structural) disambiguation ----

#[test]
fn in_as_comparator_inside_forall_body() {
    // forall x in xs: y in zs
    // Outer `in` is structural (binds source); inner `in` is
    // the membership comparator.
    let got = parse_expression("forall x in xs: y in zs").unwrap();
    let Prop::Forall { body, .. } = got else {
        panic!("expected Forall");
    };
    assert!(matches!(*body, Prop::In(_, _)));
}

// ============================================================
// Review tightenings: forall source restriction +
// strict date literal lexing
// ============================================================

// The parser must refuse value-shaped primaries in unparenthesised
// `forall` source position, even though they would parse as `primary`
// elsewhere. The kernel's `Forall.source` is predicate-shaped (calls
// `find_matches`), so the parser cannot let surface syntax produce
// ill-shaped IR.
prop_err!(forall_source_rejects_decimal_literal, "forall x in 5: P(x)");
prop_err!(
    forall_source_rejects_date_literal,
    "forall x in @2026-05-22: P(x)"
);
prop_err!(
    forall_source_rejects_subject_literal,
    "forall x in #BANK_DEBT_SERVICE: P(x)"
);
prop_err!(forall_source_rejects_wildcard, "forall x in _: P(x)");
prop_err!(
    forall_source_rejects_value_expression,
    "forall x in value Foo(_): P(x)"
);
prop_err!(
    forall_source_rejects_sum_expression,
    "forall x in sum(v | Foo(v)): P(x)"
);

/// Parenthesised proposition sources pass through as-is - the user
/// signalled explicit intent by parenthesising. A value-shaped
/// expression inside parens is still not a proposition, so it fails at
/// parse time (the source production is proposition-shaped), not as an
/// ill-shaped case the kernel has to catch.
#[test]
fn forall_source_accepts_parenthesised_predicate_form() {
    let got = parse_expression("forall x in (x in lines): P(x)").unwrap();
    let Prop::Forall {
        binding, source, ..
    } = got
    else {
        panic!("expected Forall");
    };
    assert_eq!(binding.as_str(), "x");
    // Source was an explicit `In(_, _)` inside parens; passes
    // through without auto-lift.
    assert!(matches!(*source, Prop::In(_, _)));
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
            parse_value_expr(source).unwrap_or_else(|_| panic!("expected `{source}` to parse"));
        assert!(matches!(
            got,
            ValueExpr::Term(Term::Literal(Value::Date(_)))
        ));
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
    let errs = parse_value_expr("sum(actor | MayApprove(actor, _))")
        .expect_err("actor as sum target should fail");
    assert!(
        errs.iter().any(|d| d.message.contains("`actor`")),
        "expected actor-as-sum-target diagnostic; got: {errs:?}"
    );
}

/// Quantifier bodies (exists, forall) accept indented bodies via
/// the `(Indent body Dedent | body)` choice. earlier-era tests only
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
    assert!(matches!(body, Prop::Forall { .. }));
}

// ============================================================
// Civil-date `on_or_before` comparator
// ============================================================

#[test]
fn on_or_before_lowers_to_date_le() {
    let got = parse_expression("from_date on_or_before action_date").unwrap();
    assert_eq!(
        got,
        cmp(
            CompareOp::Le,
            OrderedDomain::Date,
            var_value("from_date"),
            var_value("action_date"),
        )
    );
}

#[test]
fn decimal_le_still_lowers_to_le() {
    // Regression: do not change decimal `<=` lowering.
    let got = parse_expression("amount <= limit").unwrap();
    assert_eq!(
        got,
        cmp(
            CompareOp::Le,
            OrderedDomain::Decimal,
            var_value("amount"),
            var_value("limit"),
        )
    );
}

#[test]
fn on_or_before_at_same_precedence_as_le() {
    // `a + 1 on_or_before b` parses as `(a + 1) on_or_before b`,
    // matching `<=`'s precedence (arithmetic binds tighter).
    let got = parse_expression("a + 1 on_or_before b").unwrap();
    let Prop::Compare {
        left: lhs,
        right: rhs,
        ..
    } = got
    else {
        panic!("expected a comparison, got non-Compare");
    };
    assert!(matches!(
        *lhs,
        ValueExpr::Arith {
            op: ArithOp::Add,
            ..
        }
    ));
    assert_eq!(*rhs, var_value("b"));
}

#[test]
fn on_or_before_inside_and_chain() {
    // Realistic shape from the clinical-trial example:
    // `from on_or_before date and date on_or_before to`.
    let got = parse_expression("from on_or_before date and date on_or_before to").unwrap();
    let Prop::And(ops) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 2);
    let is_date_le = |e: &Prop| {
        matches!(
            e,
            Prop::Compare {
                op: CompareOp::Le,
                domain: OrderedDomain::Date,
                ..
            }
        )
    };
    assert!(is_date_le(&ops[0]));
    assert!(is_date_le(&ops[1]));
}

// ---------------------------------------------------------------
// Chained comparisons: `a <= x <= b` lowers to the same And of
// pairwise comparisons as the spelled-out `and` form - no new IR.
// ---------------------------------------------------------------

#[test]
fn chained_comparison_lowers_to_the_spelled_out_and() {
    assert_eq!(
        parse_expression("0 <= rate <= 1").unwrap(),
        parse_expression("0 <= rate and rate <= 1").unwrap(),
    );
}

#[test]
fn chained_date_comparison_lowers_to_the_spelled_out_and() {
    assert_eq!(
        parse_expression("from on_or_before date on_or_before to").unwrap(),
        parse_expression("from on_or_before date and date on_or_before to").unwrap(),
    );
}

#[test]
fn chain_of_three_links_flattens_into_one_and() {
    let got = parse_expression("a <= b < c <= d").unwrap();
    let Prop::And(ops) = got else {
        panic!("expected And, got {got:?}");
    };
    assert_eq!(ops.len(), 3);
}

#[test]
fn downward_chain_formats_as_the_expanded_and() {
    // The formatter's canonical output is the expanded form; the
    // chained spelling is accepted on the way in, never re-sugared
    // on the way out.
    let chained = parse_expression("0 <= rate <= 1").unwrap();
    let expanded = parse_expression("0 <= rate and rate <= 1").unwrap();
    assert_eq!(format_prop_inline(&chained), format_prop_inline(&expanded));
}

prop_ok!(
    upward_chain_accepted,
    "cap >= drawn >= 0",
    Prop::And(vec![
        cmp(
            CompareOp::Ge,
            OrderedDomain::Decimal,
            var_value("cap"),
            var_value("drawn"),
        ),
        cmp(
            CompareOp::Ge,
            OrderedDomain::Decimal,
            var_value("drawn"),
            dec_value("0"),
        ),
    ])
);

prop_err!(mixed_direction_chain_is_refused, "a <= x >= b");
prop_err!(equality_does_not_chain, "a = b = c");
prop_err!(equality_inside_a_chain_is_refused, "a <= x = b");
prop_err!(membership_does_not_chain, "a in xs in ys");

#[test]
fn chain_composes_flat_inside_a_wider_and() {
    // A chain used as one conjunct of a wider `and` splices into the
    // same flat And the spelled-out form parses to - no nesting.
    let chained = parse_expression("0 <= rate <= 1 and A(x)").unwrap();
    let expanded = parse_expression("0 <= rate and rate <= 1 and A(x)").unwrap();
    assert_eq!(chained, expanded);
    let Prop::And(ops) = chained else {
        panic!("expected And, got {chained:?}");
    };
    assert_eq!(ops.len(), 3);
}

#[test]
fn parenthesised_conjunction_flattens_into_a_wider_and() {
    assert_eq!(
        parse_expression("(B(x) and C(x)) and A(x)").unwrap(),
        parse_expression("B(x) and C(x) and A(x)").unwrap(),
    );
}
