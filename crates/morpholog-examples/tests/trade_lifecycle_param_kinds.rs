//! The integration test for the embedder kernel surface, run against
//! the trade_lifecycle worked example - the model that will be the
//! first external embedder target. It pins:
//!
//! - The "total inference" promise: every parameter of every
//!   transformation in trade_lifecycle resolves to a concrete kind,
//!   so the embedder needs no IR change to derive typed JSON-Schema
//!   input contracts (the model already supplies the kinds via
//!   claim positions).
//! - End-to-end JSON Schema composition on the load-bearing
//!   transformations (`capture_trade`, `amend_trade_terms`,
//!   `settle_trade`): each schema carries every parameter in
//!   declaration order with the expected JSON-Schema `type`. The
//!   exact property fragments are pinned by the schema unit tests
//!   in morpholog-core; this test verifies they compose into the
//!   right end-to-end shape on the real model.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    ParamKind, PredicateArgKind, TransformationName, transformation_arg_schema,
    transformation_param_kinds,
};
use morpholog_examples::trade_lifecycle;
use serde_json::Value;

#[test]
fn every_transformation_param_resolves_to_concrete() {
    let program = trade_lifecycle::program();
    // Validate once outside the loop; `ValidatedProgram` is `Copy`,
    // and re-validating per iteration would walk every invariant,
    // transformation, and derived claim on every pass for no gain.
    let validated = program.validated().expect("trade_lifecycle validates");
    let mut failures: Vec<String> = Vec::new();
    for transformation in &program.transformations {
        let kinds =
            transformation_param_kinds(&validated, &transformation.name).unwrap_or_else(|e| {
                panic!(
                    "param-kind analysis failed for `{}`: {e}",
                    transformation.name
                )
            });
        assert_eq!(
            kinds.len(),
            transformation.parameters.len(),
            "param count mismatch for `{}`",
            transformation.name,
        );
        for ((expected_var, _), declared_var) in kinds.iter().zip(transformation.parameters.iter())
        {
            assert_eq!(
                expected_var, declared_var,
                "param order drift in `{}`",
                transformation.name,
            );
        }
        for (param, kind) in &kinds {
            if !matches!(kind, ParamKind::Concrete(_)) {
                failures.push(format!(
                    "transformation `{}` parameter `{}` resolved to {:?} (expected Concrete)",
                    transformation.name, param, kind,
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "trade_lifecycle inference is not total:\n  {}",
        failures.join("\n  "),
    );
}

/// `capture_trade(trade, commodity, direction, version_id, quantity,
/// delivery_period, captured_on, price)` - the entry-point
/// transformation. Order and per-position JSON-Schema `type` pinned.
#[test]
fn capture_trade_schema_pins_typed_inputs() {
    let program = trade_lifecycle::program();
    let schema = transformation_arg_schema(
        &program.validated().expect("trade_lifecycle validates"),
        &TransformationName::from("capture_trade"),
    )
    .unwrap();
    assert_param_types(
        &schema,
        &[
            ("trade", PredicateArgKind::Subject),
            ("commodity", PredicateArgKind::Subject),
            ("direction", PredicateArgKind::Subject),
            ("version_id", PredicateArgKind::Subject),
            ("quantity", PredicateArgKind::Decimal),
            ("delivery_period", PredicateArgKind::Subject),
            ("captured_on", PredicateArgKind::Date),
            ("price", PredicateArgKind::Decimal),
        ],
    );
}

/// `amend_trade_terms(trade, prior_version_id, new_version_id,
/// quantity, delivery_period, effective_from)` - the backdatable
/// amendment. The critical bit is `effective_from` resolving to
/// Date (Date is one of the kinds the ETRM book's correction story
/// turns on).
#[test]
fn amend_trade_terms_schema_pins_typed_inputs() {
    let program = trade_lifecycle::program();
    let schema = transformation_arg_schema(
        &program.validated().expect("trade_lifecycle validates"),
        &TransformationName::from("amend_trade_terms"),
    )
    .unwrap();
    assert_param_types(
        &schema,
        &[
            ("trade", PredicateArgKind::Subject),
            ("prior_version_id", PredicateArgKind::Subject),
            ("new_version_id", PredicateArgKind::Subject),
            ("quantity", PredicateArgKind::Decimal),
            ("delivery_period", PredicateArgKind::Subject),
            ("effective_from", PredicateArgKind::Date),
        ],
    );
}

/// `settle_trade(trade, settled_qty, settlement_id, official_price_id,
/// effective_on)` - the per-slice settlement that the running-total
/// cap is enforced over. Pins that the settle-only-inside-`require`
/// gate parameters (settlement_id, official_price_id, effective_on)
/// still resolve to concrete kinds - the regression test for the
/// `Require`-clone trap, applied on the real model.
#[test]
fn settle_trade_schema_pins_typed_inputs() {
    let program = trade_lifecycle::program();
    let schema = transformation_arg_schema(
        &program.validated().expect("trade_lifecycle validates"),
        &TransformationName::from("settle_trade"),
    )
    .unwrap();
    assert_param_types(
        &schema,
        &[
            ("trade", PredicateArgKind::Subject),
            ("settled_qty", PredicateArgKind::Decimal),
            ("settlement_id", PredicateArgKind::Subject),
            ("official_price_id", PredicateArgKind::Subject),
            ("effective_on", PredicateArgKind::Date),
        ],
    );
}

fn assert_param_types(schema: &Value, expected: &[(&str, PredicateArgKind)]) {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let want_names: Vec<&str> = expected.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        required, want_names,
        "required[] order must match declaration order",
    );
    for (name, kind) in expected {
        let property = &schema["properties"][name];
        let expected_type = expected_json_type(*kind);
        assert_eq!(
            property["type"], expected_type,
            "type for `{name}` should be {expected_type} (kind {kind:?})",
        );
    }
}

fn expected_json_type(kind: PredicateArgKind) -> Value {
    match kind {
        PredicateArgKind::Subject | PredicateArgKind::Decimal | PredicateArgKind::Date => {
            Value::String("string".into())
        }
        PredicateArgKind::Bool => Value::String("boolean".into()),
        PredicateArgKind::Collection => Value::String("array".into()),
        PredicateArgKind::Any => Value::Null,
    }
}
