//! The bytes `digest` hashes, pinned: one case per stored kind and the
//! edges of each, in a golden the Python client's test reads too, so the
//! two encoders are held to one file, and the adapter's differential
//! holds the same cases to `morpholog.value_key_v1`. `UPDATE_GOLDENS=1`
//! rewrites the golden; a change to an existing case's bytes is a change
//! to every outstanding commitment and is never re-pinned quietly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::EvalValue;
use morpholog_core::digest::{digest, preimage};
use morpholog_test_support::{bool_, date, dec_str, dur, qty, subj, ts};
use serde_json::{Value, json};

const GOLDEN: &str = include_str!("../../morpholog-core/tests/golden/digest_v1.json");
const GOLDEN_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../morpholog-core/tests/golden/digest_v1.json"
);

fn corpus() -> Vec<(&'static str, Vec<EvalValue>)> {
    vec![
        ("subject", vec![subj("r1")]),
        ("decimal", vec![dec_str("30")]),
        ("bool", vec![bool_(true)]),
        ("date", vec![date("2026-06-01")]),
        ("timestamp", vec![ts("2026-06-01T12:00:00Z")]),
        (
            "timestamp_with_a_fraction",
            vec![ts("2026-06-01T12:00:00.000000001Z")],
        ),
        (
            "timestamp_with_a_short_fraction",
            vec![ts("2026-06-01T12:00:00.1Z")],
        ),
        ("duration", vec![dur("PT6H")]),
        ("quantity", vec![qty("25000.00", "USD")]),
        ("trailing_zeros", vec![dec_str("30.00")]),
        ("negative_zero", vec![dec_str("-0.0")]),
        ("negative", vec![dec_str("-1.50")]),
        ("smallest", vec![dec_str("0.0000000000000000000000000001")]),
        ("largest", vec![dec_str("79228162514264337593543950335")]),
        ("unicode_subject", vec![subj("caf\u{e9} \u{2615}")]),
        ("escaped_subject", vec![subj("a\"b\\c\nd\te\u{1}f/g")]),
        (
            "several",
            vec![
                subj("sealed_bid_v1"),
                subj("lot"),
                subj("alice"),
                dec_str("100"),
                subj("s1"),
            ],
        ),
        ("order_a_b", vec![subj("a"), subj("b")]),
        ("order_b_a", vec![subj("b"), subj("a")]),
        ("bool_true", vec![bool_(true)]),
        ("decimal_one", vec![dec_str("1")]),
        ("subject_true", vec![subj("true")]),
        ("subject_one", vec![subj("1")]),
    ]
}

fn computed() -> Value {
    Value::Array(
        corpus()
            .into_iter()
            .map(|(name, args)| {
                json!({
                    "name": name,
                    "args": args,
                    "preimage": preimage(&args).unwrap(),
                    "digest": digest(&args).unwrap(),
                })
            })
            .collect(),
    )
}

#[test]
fn the_preimage_bytes_are_pinned() {
    let cases = computed();
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        let text = serde_json::to_string_pretty(&cases).unwrap();
        std::fs::write(GOLDEN_PATH, format!("{text}\n")).unwrap();
        return;
    }
    let pinned: Value = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(cases, pinned, "the digest's bytes moved; see {GOLDEN_PATH}");
}
