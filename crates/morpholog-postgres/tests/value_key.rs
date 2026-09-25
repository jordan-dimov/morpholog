//! `morpholog.value_key_v1` is the one equality the compiled checks
//! compare stored values by, so its equality must be the kernel's: for
//! every pair of lawfully storable values, the keys are equal exactly
//! when the kernel says the values are. The kernel is the oracle. Keys of
//! values the codec never writes (a tag this version does not know) are
//! a robustness property, tested apart: same input, same key; the
//! function never raises.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use morpholog_core::EvalValue;
use morpholog_test_support::{bool_, coll, date, dec_str, dur, qty, subj, ts};
use serde_json::json;
use sqlx::PgPool;

async fn key(pool: &PgPool, value: serde_json::Value) -> serde_json::Value {
    sqlx::query_scalar::<_, serde_json::Value>("SELECT morpholog.value_key_v1($1)")
        .bind(value)
        .fetch_one(pool)
        .await
        .expect("the key function is total")
}

fn tagged(v: &EvalValue) -> serde_json::Value {
    serde_json::to_value(v).expect("serialises")
}

/// Storable values in equality classes the kernel decides: several
/// spellings of one value beside values that differ from it minimally.
fn corpus() -> Vec<EvalValue> {
    let mut values = vec![
        dec_str("1"),
        dec_str("1.0"),
        dec_str("1.00"),
        dec_str("1.000000000000000000000000001"),
        dec_str("0"),
        dec_str("0.0"),
        dec_str("-0"),
        dec_str("-0.00"),
        dec_str("-1"),
        dec_str("79228162514264337593543950335"),
        dec_str("-79228162514264337593543950335"),
        dec_str("0.0000000000000000000000000001"),
        qty("1", "MW"),
        qty("1.0", "MW"),
        qty("1.00", "MW"),
        qty("1", "EUR"),
        qty("0", "MW"),
        qty("-0.0", "MW"),
        subj("1"),
        subj("1.0"),
        subj("a"),
        subj(&"a".repeat(4000)),
        subj(&"b".repeat(4000)),
        bool_(true),
        bool_(false),
        date("2026-01-01"),
        date("2026-01-02"),
        ts("2026-01-01T00:00:00Z"),
        ts("2026-01-01T00:00:00.5Z"),
        ts("2026-01-01T00:00:00.000000001Z"),
        dur("PT1H"),
        dur("PT60M"),
        dur("PT3600S"),
        dur("PT1H1S"),
        coll(vec![]),
        coll(vec![dec_str("1.0")]),
        coll(vec![dec_str("1.00")]),
        coll(vec![dec_str("1"), dec_str("2")]),
        coll(vec![dec_str("2"), dec_str("1")]),
        coll(vec![subj("1")]),
        coll(vec![coll(vec![qty("1.0", "MW")])]),
        coll(vec![coll(vec![qty("1.00", "MW")])]),
        coll(vec![coll(vec![qty("1", "EUR")])]),
    ];
    // A deep one.
    let mut deep = dec_str("1.0");
    for _ in 0..20 {
        deep = coll(vec![deep]);
    }
    values.push(deep.clone());
    let mut deep2 = dec_str("1.00");
    for _ in 0..20 {
        deep2 = coll(vec![deep2]);
    }
    values.push(deep2);
    values
}

#[tokio::test]
async fn the_key_is_equal_exactly_when_the_kernel_says_so() {
    let pool = test_pool().await;
    let values = corpus();
    let mut keys = Vec::with_capacity(values.len());
    for v in &values {
        keys.push(key(&pool, tagged(v)).await);
    }
    let mut equal_pairs = 0usize;
    for (i, a) in values.iter().enumerate() {
        for (j, b) in values.iter().enumerate() {
            let kernel = a == b;
            let keyed = keys[i] == keys[j];
            assert_eq!(
                keyed, kernel,
                "{a:?} vs {b:?}: keys {} / {}",
                keys[i], keys[j]
            );
            if kernel && i != j {
                equal_pairs += 1;
            }
        }
    }
    // The corpus must exercise equality across spellings, or the test
    // proves only inequality.
    assert!(
        equal_pairs >= 20,
        "{equal_pairs} equal pairs across spellings"
    );
}

#[tokio::test]
async fn a_value_of_an_unknown_kind_keys_by_its_payload_and_never_raises() {
    let pool = test_pool().await;
    let a = key(&pool, json!({"type":"future","value":{"a":1}})).await;
    let b = key(&pool, json!({"type":"future","value":{"a":1}})).await;
    let c = key(&pool, json!({"type":"future","value":{"a":2}})).await;
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a, json!(["future", {"a": 1}]));
    assert_eq!(key(&pool, json!("bare")).await, json!(["raw", "bare"]));
    assert_eq!(
        key(&pool, json!({"value": 1})).await,
        json!(["raw", {"value": 1}])
    );
    assert_eq!(key(&pool, json!(null)).await, json!(["raw", null]));
}

#[tokio::test]
async fn the_pinned_keys_hold_on_this_database() {
    let pool = test_pool().await;
    for (value, expected) in common::pinned_value_keys() {
        assert_eq!(key(&pool, value.clone()).await, expected, "{value}");
    }
}
