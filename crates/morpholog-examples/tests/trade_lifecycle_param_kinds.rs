//! The embedder surface, run against the trade_lifecycle example:
//!
//! - Every parameter of every transformation resolves to a concrete kind
//!   from its claim positions, so an embedder gets typed JSON-Schema input
//!   contracts with no declared kinds.
//! - For the main transformations (`capture_trade`, `amend_trade_terms`,
//!   `settle_trade`), each schema lists every parameter in declaration order
//!   with the expected `type`. Core's unit tests pin the fragments; this
//!   checks they compose on a real model.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    IntentName, ParamKind, PredicateArgKind, TransformationName, intent_arg_schema,
    transformation_arg_schema, transformation_param_kinds,
};
use morpholog_examples::trade_lifecycle;
use serde_json::Value;

#[test]
fn every_transformation_param_resolves_to_concrete() {
    let program = trade_lifecycle::program();
    // Validate once: `ValidatedProgram` is `Copy`, and validating is a full walk.
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
/// amendment. What matters is that `effective_from` resolves to Date.
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
/// effective_on)` - the per-slice settlement the running-total cap governs.
/// Parameters used only inside `require` (settlement_id, official_price_id,
/// effective_on) must still resolve to concrete kinds.
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
    let want_names: Vec<&str> = expected.iter().map(|(n, _)| *n).collect();
    // `x-morpholog-arg-order` is the load-bearing positional contract;
    // `required` is the JSON Schema validation keyword and mirrors the
    // same names. Both must be in declaration order.
    for key in ["required", "x-morpholog-arg-order"] {
        let got: Vec<&str> = schema[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} array"))
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            got, want_names,
            "{key}[] order must match declaration order"
        );
    }
    for (name, kind) in expected {
        let property = &schema["properties"][name];
        let expected_type = expected_json_type(kind);
        assert_eq!(
            property["type"], expected_type,
            "type for `{name}` should be {expected_type} (kind {kind:?})",
        );
    }
}

fn expected_json_type(kind: &PredicateArgKind) -> Value {
    match kind {
        PredicateArgKind::Subject
        | PredicateArgKind::Decimal
        | PredicateArgKind::Date
        | PredicateArgKind::Timestamp
        | PredicateArgKind::Duration
        | PredicateArgKind::Quantity(_) => Value::String("string".into()),
        PredicateArgKind::Bool => Value::String("boolean".into()),
        PredicateArgKind::Collection => Value::String("array".into()),
        PredicateArgKind::CalendarSpan | PredicateArgKind::Any => Value::Null,
    }
}

/// The intent-payload counterpart: a deliverer decodes
/// `TradeSettlementRequested` by name from this contract, not by position.
/// `x-morpholog-arg-order` carries the payload order (`settlement_id`,
/// `trade`, `settled_qty`); `required` mirrors the same names.
/// `assert_param_types` checks both.
#[test]
fn trade_settlement_requested_intent_schema_pins_payload() {
    let program = trade_lifecycle::program();
    let schema = intent_arg_schema(
        &program.validated().expect("trade_lifecycle validates"),
        &IntentName::from("TradeSettlementRequested"),
    )
    .expect("TradeSettlementRequested is a declared intent");
    assert_param_types(
        &schema,
        &[
            ("settlement_id", PredicateArgKind::Subject),
            ("trade", PredicateArgKind::Subject),
            ("settled_qty", PredicateArgKind::Decimal),
        ],
    );
}

#[test]
fn unknown_intent_schema_is_none() {
    let program = trade_lifecycle::program();
    assert!(
        intent_arg_schema(
            &program.validated().expect("trade_lifecycle validates"),
            &IntentName::from("NoSuchIntent"),
        )
        .is_none(),
    );
}
