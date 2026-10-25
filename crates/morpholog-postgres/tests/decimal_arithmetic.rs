//! The compiled checks compute decimal arithmetic in SQL, so each SQL
//! function must give the kernel's value, or fail exactly where the
//! kernel errs: a pseudo-random sweep weighted to wide operands, where
//! the exact result often does not fit, plus the known edges.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use morpholog_core::{ArithOp, Builtin, EvalValue, Term, ValueExpr, Var, eval_value_over};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Clone, Copy)]
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
        Op::Add => "morpholog.decimal_fit_v1(a + b)",
        Op::Sub => "morpholog.decimal_fit_v1(a - b)",
        Op::Mul => "morpholog.decimal_fit_v1(a * b)",
        Op::Div => "morpholog.decimal_div_v1(a, b)",
        Op::Rem => "CASE WHEN b = 0 THEN NULL ELSE morpholog.decimal_fit_v1(mod(a, b)) END",
        Op::Round => "morpholog.decimal_round_v1(a, b)",
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
    let mut inexact = 0;
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
            if let Ok(EvalValue::Decimal(w)) = &want
                && matches!(op, Op::Add | Op::Sub | Op::Mul)
                && exact(op, *a, *b) != Some(*w)
            {
                inexact += 1;
            }
        }
    }
    assert!(checked > 70_000, "anti-vacuity: checked {checked}");
    assert!(
        inexact > 1_000,
        "anti-vacuity: the sweep must reach results that do not fit, got {inexact}"
    );
}

/// The exact sum, difference or product where a decimal holds it, so
/// the sweep can count how often it reached the rounding the functions
/// must mirror.
fn exact(op: Op, a: Decimal, b: Decimal) -> Option<Decimal> {
    let (ma, sa, mb, sb) = (a.mantissa(), a.scale(), b.mantissa(), b.scale());
    let (m, s) = match op {
        Op::Add | Op::Sub => {
            let s = sa.max(sb);
            let x = ma.checked_mul(10i128.pow(s - sa))?;
            let y = mb.checked_mul(10i128.pow(s - sb))?;
            (
                if matches!(op, Op::Add) {
                    x.checked_add(y)?
                } else {
                    x.checked_sub(y)?
                },
                s,
            )
        }
        Op::Mul => (ma.checked_mul(mb)?, sa + sb),
        Op::Div | Op::Rem | Op::Round | Op::Min | Op::Max => return None,
    };
    Decimal::try_from_i128_with_scale(m, s).ok()
}
