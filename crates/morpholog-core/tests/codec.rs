//! JSON codec round-trip tests for the runtime value types.
//!
//! PG persistence writes these JSON shapes into JSONB columns declared
//! in `crates/morpholog-core/sql/schema.sql`. The round-trip tests pin
//! both the wire shape (so future readers cannot accidentally change
//! it) and the exactness contract (decimals serialise as JSON
//! **strings**, never as JSON numbers).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ClaimInstance, EvalValue, IntentInstance};
use rust_decimal::Decimal;
use std::str::FromStr;

fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
}

#[test]
fn eval_value_decimal_round_trips_as_tagged_json_string() {
    let v = EvalValue::Decimal(Decimal::new(917, 1));
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"decimal","value":"91.7"}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_subject_round_trips_as_tagged_json_string() {
    let v = EvalValue::Subject("asset_a".to_string());
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"subject","value":"asset_a"}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_bool_round_trips_as_tagged_json() {
    let v = EvalValue::Bool(true);
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"bool","value":true}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_date_round_trips_as_tagged_json_string() {
    // Civil dates serialise as ISO-8601 strings under the
    // adjacently-tagged shape. Pinning the exact wire shape so future
    // changes to jiff's serde format cannot silently break the PG JSONB
    // contract.
    let v: EvalValue = EvalValue::Date("2026-03-12".parse().unwrap());
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"date","value":"2026-03-12"}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_collection_round_trips_through_nested_json() {
    let v = EvalValue::Collection(vec![
        EvalValue::Subject("l1".to_string()),
        EvalValue::Subject("l2".to_string()),
        EvalValue::Decimal(Decimal::new(60, 0)),
    ]);
    let json = serde_json::to_string(&v).unwrap();
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
    // Nested decimal must still be a string inside the collection.
    assert!(json.contains(r#""type":"decimal","value":"60""#));
}

#[test]
fn claim_instance_round_trips_through_json_with_mixed_args() {
    let c = ClaimInstance {
        predicate: "BankRecognisedRevenue".to_string(),
        args: vec![subj("asset_a"), subj("p_2026_04"), dec(92), subj("rec_001")],
    };
    let json = serde_json::to_string(&c).unwrap();
    let parsed: ClaimInstance = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, c);
    // Shape is { "predicate": "...", "args": [ ... ] }.
    assert!(json.starts_with(r#"{"predicate":"BankRecognisedRevenue","args":["#));
}

#[test]
fn intent_instance_round_trips_through_json() {
    let i = IntentInstance {
        name: "NetSettlementCreated".to_string(),
        args: vec![subj("net1")],
    };
    let json = serde_json::to_string(&i).unwrap();
    let parsed: IntentInstance = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, i);
    assert!(json.starts_with(r#"{"name":"NetSettlementCreated","args":["#));
}

#[test]
fn decimal_value_preserves_high_precision_through_json() {
    // 18 fractional digits — well within rust_decimal's 96-bit range.
    let exact = "1234567890.123456789";
    let v = EvalValue::Decimal(Decimal::from_str(exact).unwrap());
    let json = serde_json::to_string(&v).unwrap();
    // The decimal must appear verbatim, as a quoted string.
    assert!(json.contains(&format!(r#""value":"{exact}""#)));
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn claim_args_serialise_as_a_json_array() {
    // Pins the split contract: the `claims.arguments` column has a CHECK
    // constraint `jsonb_typeof(arguments) = 'array'`. The PG adapter
    // writes only the `args` field of a ClaimInstance into that column.
    let c = ClaimInstance {
        predicate: "Quantity".to_string(),
        args: vec![subj("trade123"), dec(10)],
    };
    let args_json = serde_json::to_string(&c.args).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&args_json).unwrap();
    assert!(
        parsed.is_array(),
        "claim.args must serialise as a JSON array"
    );
}

#[test]
fn intent_args_serialise_as_a_json_array() {
    // Pins the split contract: the `outbox.arguments` column has a CHECK
    // constraint `jsonb_typeof(arguments) = 'array'`. The PG adapter
    // writes only the `args` field of an IntentInstance into that column.
    let i = IntentInstance {
        name: "NetSettlementCreated".to_string(),
        args: vec![subj("net1"), dec(100)],
    };
    let args_json = serde_json::to_string(&i.args).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&args_json).unwrap();
    assert!(
        parsed.is_array(),
        "intent.args must serialise as a JSON array"
    );
}
