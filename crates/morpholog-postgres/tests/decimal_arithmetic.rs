//! The compiled checks compute decimal arithmetic in SQL, so each SQL
//! function must give the kernel's value, or fail exactly where the
//! kernel errs: a pseudo-random sweep weighted to wide operands, where
//! the exact result often does not fit, plus the known edges. The same
//! sweep holds the kernel to its contract: `+`, `-` and `*` answer with
//! the exact result or refuse, never a rounded figure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use morpholog_core::{ArithOp, Builtin, EvalValue, Term, ValueExpr, Var, eval_value_over};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Clone, Copy, Debug)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Round,
    Min,
    Max,
}

const OPS: [Op; 8] = [
    Op::Add,
    Op::Sub,
    Op::Mul,
    Op::Div,
    Op::Rem,
    Op::Round,
    Op::Min,
    Op::Max,
];

const EDGES: &[(&str, &str)] = &[
    ("0.0000000000000000000000000001", "0.5"),
    ("0.0000000000000000000000000001", "1.5"),
    ("0.0000000000000000000000000001", "2.5"),
    ("79228162514264337593543950335", "0.4"),
    ("79228162514264337593543950335", "0.6"),
    ("79228162514264337593543950335", "19"),
    ("79228162514264337593543950335", "10"),
    ("-79228162514264337593543950335", "19"),
    ("1", "3"),
    ("2", "3"),
    ("-2", "3"),
    ("100", "3"),
    ("1", "20000000000000000000000000000"),
    ("3", "20000000000000000000000000000"),
    ("10", "4"),
    ("5.5", "2"),
    ("-10", "3"),
    ("10", "-3"),
    ("1", "0"),
    ("0", "0"),
    ("1.23456789012345", "1.23456789012345"),
    (
        "7922816251426433759354395033.5",
        "0.0000000000000000000000000001",
    ),
    // round judged on the multiple it selects: the doubled remainder and
    // the unchosen candidate lie outside the range, the answer does not.
    (
        "60000000000000000000000000000",
        "79228162514264337593543950335",
    ),
    (
        "50000000000000000000000000000",
        "40000000000000000000000000000",
    ),
    // The nearest multiple of 0.3 has a tenth and 29 digits: refused.
    ("79228162514264337593543950334", "0.3"),
    // Exact once trailing zeros go, though the raw scale is 38.
    ("2.0000000000000000000", "1.0000000000000000000"),
    // Thirty significant digits: refused where it once rounded.
    ("0.9876543219", "9876543210.9876543219"),
    // The product of the mantissas, and the multiple round selects, is
    // the most negative i128: refused, never a panic on its magnitude.
    ("-922337203.6854775808", "1844674407.3709551616"),
    (
        "-17014118346046923173168730371",
        "3961408125713216879.6771975168",
    ),
];

fn kernel(op: Op, a: Decimal, b: Decimal) -> Result<EvalValue, morpholog_core::EvalError> {
    let var = |n: &str| Box::new(ValueExpr::Term(Term::Var(Var::from(n))));
    let expr = match op {
        Op::Round | Op::Min | Op::Max => ValueExpr::Call {
            builtin: match op {
                Op::Round => Builtin::Round,
                Op::Min => Builtin::Min,
                _ => Builtin::Max,
            },
            args: vec![*var("a"), *var("b")],
        },
        other => ValueExpr::Arith {
            op: match other {
                Op::Add => ArithOp::Add,
                Op::Sub => ArithOp::Sub,
                Op::Mul => ArithOp::Mul,
                Op::Div => ArithOp::Div,
                Op::Rem | Op::Round | Op::Min | Op::Max => ArithOp::Mod,
            },
            left: var("a"),
            right: var("b"),
        },
    };
    let bindings: HashMap<Var, EvalValue> = [
        (Var::from("a"), EvalValue::Decimal(a)),
        (Var::from("b"), EvalValue::Decimal(b)),
    ]
    .into_iter()
    .collect();
    eval_value_over(&expr, &bindings, &[]).expect("pure arithmetic evaluates alone")
}

fn sql(op: Op) -> &'static str {
    match op {
        Op::Add => "morpholog.decimal_exact_v1(a + b)",
        Op::Sub => "morpholog.decimal_exact_v1(a - b)",
        Op::Mul => "morpholog.decimal_exact_v1(a * b)",
        Op::Div => "morpholog.decimal_div_v1(a, b)",
        Op::Rem => "CASE WHEN b = 0 THEN NULL ELSE morpholog.decimal_exact_v1(mod(a, b)) END",
        Op::Round => "morpholog.decimal_round_v2(a, b)",
        Op::Min => "morpholog.decimal_min_v1(a, b)",
        Op::Max => "morpholog.decimal_max_v1(a, b)",
    }
}

/// Pseudo-random decimals over every digit count and scale the kernel
/// holds, signed.
fn sweep() -> Vec<(Decimal, Decimal)> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut decimal = || {
        let digits = 1 + usize::try_from(next() % 29).unwrap();
        let text: String = (0..digits)
            .map(|i| {
                let d = u8::try_from(next() % 10).unwrap();
                char::from(b'0' + if i == 0 && digits > 1 && d == 0 { 1 } else { d })
            })
            .collect();
        let scale = u32::try_from(next() % 29).unwrap();
        let mantissa = i128::from_str(&text).unwrap();
        let value = Decimal::try_from_i128_with_scale(mantissa, scale).unwrap_or(Decimal::ONE);
        if next() % 2 == 0 { -value } else { value }
    };
    (0..12_000).map(|_| (decimal(), decimal())).collect()
}

#[tokio::test]
async fn each_sql_function_gives_the_kernels_value_or_fails_where_it_errs() {
    let pool = test_pool().await;
    let mut pairs: Vec<(Decimal, Decimal)> = EDGES
        .iter()
        .map(|(a, b)| (Decimal::from_str(a).unwrap(), Decimal::from_str(b).unwrap()))
        .collect();
    pairs.extend(sweep());
    let (lefts, rights): (Vec<String>, Vec<String>) = pairs
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .unzip();
    let mut checked = 0;
    let mut refused_inexact = 0;
    for op in OPS {
        let query = format!(
            "SELECT trim_scale({})::text
             FROM unnest($1::numeric[], $2::numeric[]) WITH ORDINALITY AS t(a, b, i)
             ORDER BY i",
            sql(op)
        );
        let got: Vec<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .bind(&lefts)
            .bind(&rights)
            .fetch_all(&pool)
            .await
            .expect("the sweep runs");
        for ((a, b), got) in pairs.iter().zip(got) {
            let want = kernel(op, *a, *b);
            let got = got.map(|t| Decimal::from_str(&t).expect("the function returns a decimal"));
            match (&want, got) {
                (Ok(EvalValue::Decimal(w)), Some(g)) => {
                    assert_eq!(*w, g, "{} over {a} and {b}", sql(op));
                }
                (Err(_), None) => {}
                (want, got) => panic!("{} over {a} and {b}: kernel {want:?}, SQL {got:?}", sql(op)),
            }
            checked += 1;
            if matches!(op, Op::Add | Op::Sub | Op::Mul) {
                match (&want, exact(op, *a, *b)) {
                    (Ok(EvalValue::Decimal(w)), Some(e)) => {
                        assert_eq!(*w, e, "{op:?} over {a} and {b} must be exact");
                    }
                    (Err(_), None) => refused_inexact += 1,
                    (want, exact) => panic!(
                        "{op:?} over {a} and {b}: kernel {want:?}, exact {exact:?}; \
                         the kernel answers exactly where a decimal holds the result"
                    ),
                }
            }
        }
    }
    assert!(checked > 70_000, "anti-vacuity: checked {checked}");
    assert!(
        refused_inexact > 1_000,
        "anti-vacuity: the sweep must reach results that do not fit, got {refused_inexact}"
    );
}

/// The exact sum, difference or product where a decimal holds it, with
/// trailing zeros dropped first; `None` where no decimal does.
fn exact(op: Op, a: Decimal, b: Decimal) -> Option<Decimal> {
    use num_bigint::BigInt;
    let ten = BigInt::from(10u32);
    let at = |d: Decimal, s: u32| BigInt::from(d.mantissa()) * ten.pow(s - d.scale());
    let (mut m, mut s) = match op {
        Op::Add | Op::Sub => {
            let s = a.scale().max(b.scale());
            let (x, y) = (at(a, s), at(b, s));
            (if matches!(op, Op::Add) { x + y } else { x - y }, s)
        }
        Op::Mul => (
            BigInt::from(a.mantissa()) * BigInt::from(b.mantissa()),
            a.scale() + b.scale(),
        ),
        Op::Div | Op::Rem | Op::Round | Op::Min | Op::Max => return None,
    };
    while s > 0 && (&m % &ten) == BigInt::from(0) {
        m /= &ten;
        s -= 1;
    }
    if s > 28 {
        return None;
    }
    Decimal::try_from_i128_with_scale(i128::try_from(m).ok()?, s).ok()
}
