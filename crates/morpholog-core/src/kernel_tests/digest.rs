//! The bytes `digest` hashes, pinned: one case per stored kind and the
//! edges of each, in a golden the Python client's test reads too, so the
//! two encoders are held to one file. The adapter's differential holds
//! the same cases to `morpholog.value_key_v1`. `UPDATE_GOLDENS=1`
//! rewrites the golden; a change to an existing case's bytes is a change
//! to every outstanding commitment and is never re-pinned quietly.

use crate::digest::{digest, preimage};
use crate::ir::{Subject, Unit};
use crate::state::EvalValue;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

const GOLDEN: &str = include_str!("../../tests/golden/digest_v1.json");

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Case {
    name: String,
    args: Vec<EvalValue>,
    preimage: String,
    digest: String,
}

fn s(x: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(x))
}

fn d(x: &str) -> EvalValue {
    EvalValue::Decimal(Decimal::from_str(x).unwrap())
}

fn t(x: &str) -> EvalValue {
    EvalValue::Timestamp(x.parse().unwrap())
}

fn corpus() -> Vec<(&'static str, Vec<EvalValue>)> {
    vec![
        ("subject", vec![s("r1")]),
        ("decimal", vec![d("30")]),
        ("bool", vec![EvalValue::Bool(true)]),
        ("date", vec![EvalValue::Date("2026-06-01".parse().unwrap())]),
        ("timestamp", vec![t("2026-06-01T12:00:00Z")]),
        (
            "timestamp_with_a_fraction",
            vec![t("2026-06-01T12:00:00.000000001Z")],
        ),
        (
            "timestamp_with_a_short_fraction",
            vec![t("2026-06-01T12:00:00.1Z")],
        ),
        (
            "duration",
            vec![EvalValue::Duration("PT6H".parse().unwrap())],
        ),
        (
            "quantity",
            vec![EvalValue::Quantity {
                amount: Decimal::from_str("25000.00").unwrap(),
                unit: Unit::from("USD"),
            }],
        ),
        ("trailing_zeros", vec![d("30.00")]),
        ("negative_zero", vec![d("-0.0")]),
        ("negative", vec![d("-1.50")]),
        ("smallest", vec![d("0.0000000000000000000000000001")]),
        ("largest", vec![d("79228162514264337593543950335")]),
        ("unicode_subject", vec![s("caf\u{e9} \u{2615}")]),
        ("escaped_subject", vec![s("a\"b\\c\nd\te\u{1}f/g")]),
        (
            "several",
            vec![s("sealed_bid_v1"), s("lot"), s("alice"), d("100"), s("s1")],
        ),
        ("order_a_b", vec![s("a"), s("b")]),
        ("order_b_a", vec![s("b"), s("a")]),
        ("bool_true", vec![EvalValue::Bool(true)]),
        ("decimal_one", vec![d("1")]),
        ("subject_true", vec![s("true")]),
        ("subject_one", vec![s("1")]),
    ]
}

fn computed() -> Vec<Case> {
    corpus()
        .into_iter()
        .map(|(name, args)| Case {
            name: name.to_string(),
            preimage: preimage(&args),
            digest: digest(&args),
            args,
        })
        .collect()
}

#[test]
fn the_preimage_bytes_are_pinned() {
    let cases = computed();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/digest_v1.json");
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        let text = serde_json::to_string_pretty(&cases).unwrap();
        std::fs::write(path, format!("{text}\n")).unwrap();
        return;
    }
    let pinned: Vec<Case> = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(cases, pinned, "the digest's bytes moved; see {path}");
}

/// Two spellings of one stored value share a digest; two values of one
/// text and different kinds do not; order counts.
#[test]
fn the_digest_follows_the_stored_values_equality() {
    assert_eq!(digest(&[d("30")]), digest(&[d("30.0")]));
    assert_eq!(digest(&[d("30")]), digest(&[d("30.00")]));
    assert_eq!(digest(&[d("0")]), digest(&[d("-0.0")]));
    assert_eq!(preimage(&[d("-0.0")]), r#"[["decimal", "0"]]"#);
    assert_eq!(
        digest(&[t("2026-06-01T12:00:00Z")]),
        digest(&[t("2026-06-01T13:00:00+01:00")])
    );
    assert_ne!(digest(&[EvalValue::Bool(true)]), digest(&[s("true")]));
    assert_ne!(digest(&[d("1")]), digest(&[s("1")]));
    assert_ne!(digest(&[s("a"), s("b")]), digest(&[s("b"), s("a")]));
    assert_ne!(digest(&[s("ab"), s("c")]), digest(&[s("a"), s("bc")]));
    assert_eq!(
        preimage(&[s("r1"), d("30.0"), EvalValue::Bool(true)]),
        r#"[["subject", "r1"], ["decimal", "30"], ["bool", true]]"#
    );
    let out = digest(&[s("r1")]);
    assert!(out.starts_with("sha256:") && out.len() == 7 + 64, "{out}");
}
