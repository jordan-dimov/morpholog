//! JSON Schema (Draft 2020-12) for a transformation's arguments or an
//! intent's payload: which named fields it takes and the kind of each.
//!
//! This renders the kernel's inferred [`ParamKind`]s; it is not a second
//! source of truth. Other renderings (OpenAPI, a Python class) would be
//! built from the same analysis.
//!
//! Encoding choices:
//! - Subjects are plain strings with no `format`. A subject may be a minted
//!   UUIDv7 or a domain symbol such as a commodity code.
//! - Decimals are strings, not JSON numbers, to stay exact. The pattern
//!   rejects ambiguous forms such as `00.12` or a leading `+`.
//! - Dates are ISO-8601 civil dates, timestamps RFC 3339 instants,
//!   durations ISO-8601 exact-time spans. A quantity is a decimal string
//!   with its unit in `x-morpholog-unit`.
//! - `Polymorphic` and `Unconstrained` parameters have no `type`, and a
//!   description saying the kernel cannot narrow them.

use serde_json::{Value, json};

use crate::analysis::{AnalysisError, ParamKind, transformation_param_kinds};
use crate::ir::{IntentName, PredicateArgKind, TransformationName};
use crate::validate::ValidatedProgram;

/// Emit a JSON Schema (Draft 2020-12) for the named transformation's
/// argument object. Parameters appear in declaration order under
/// `properties`, all are `required`, and `additionalProperties` is
/// `false` so no extra fields get through.
///
/// # Errors
///
/// Whatever [`transformation_param_kinds`] returns, unchanged.
pub fn transformation_arg_schema(
    program: &ValidatedProgram<'_>,
    name: &TransformationName,
) -> Result<Value, AnalysisError> {
    let kinds = transformation_param_kinds(program, name)?;

    let mut properties = serde_json::Map::with_capacity(kinds.len());
    let mut required = Vec::with_capacity(kinds.len());
    for (param, kind) in &kinds {
        properties.insert(param.as_str().to_string(), property_schema(kind));
        required.push(Value::String(param.as_str().to_string()));
    }
    let arg_order = required.clone();

    Ok(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": name.as_str(),
        "type": "object",
        "additionalProperties": false,
        "required": required,
        // The argument order, for the positional `--args` codec.
        // `required` is a set in JSON Schema, so its order means nothing.
        "x-morpholog-arg-order": arg_order,
        "properties": properties,
    }))
}

/// Emit a JSON Schema (Draft 2020-12) for the named intent's payload
/// object, so an outbox deliverer can decode a payload by name.
///
/// Intent arguments declare their kinds, so nothing is inferred. `None`
/// when no intent has that name.
pub fn intent_arg_schema(program: &ValidatedProgram<'_>, name: &IntentName) -> Option<Value> {
    let decl = program
        .as_program()
        .intents
        .iter()
        .find(|d| &d.name == name)?;

    let mut properties = serde_json::Map::with_capacity(decl.args.len());
    let mut required = Vec::with_capacity(decl.args.len());
    for arg in &decl.args {
        properties.insert(
            arg.name.clone(),
            concrete_property(&arg.kind, SchemaContext::IntentPayload),
        );
        required.push(Value::String(arg.name.clone()));
    }
    let arg_order = required.clone();

    Some(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": name.as_str(),
        "type": "object",
        "additionalProperties": false,
        "required": required,
        // The argument order, for decoding a positional payload array.
        // `required` is a set in JSON Schema, so its order means nothing.
        "x-morpholog-arg-order": arg_order,
        "properties": properties,
    }))
}

/// Map one parameter's [`ParamKind`] to its JSON Schema property.
/// `Ambiguous` becomes an `anyOf` of bare shapes, with one description at
/// the property level naming the ambiguity rather than one per option.
fn property_schema(kind: &ParamKind) -> Value {
    match kind {
        ParamKind::Concrete(k) => concrete_property(k, SchemaContext::TransformationArg),
        // Seen only at `Any` positions.
        ParamKind::Polymorphic => json!({
            "description": "polymorphic; the model does not narrow this parameter's kind"
        }),
        ParamKind::Unconstrained => json!({
            "description": "unconstrained; parameter is never observed at a kind-bearing position (likely a modelling smell)"
        }),
        ParamKind::Ambiguous(kinds) => {
            let alternatives: Vec<Value> = kinds.iter().map(bare_kind_shape).collect();
            json!({
                "description": "ambiguous; parameter is observed at different concrete kinds across branch-local positions (typically `Or` branches the static checker does not refine across)",
                "anyOf": alternatives,
            })
        }
        // A collection whose element kind was inferred from its loop. With
        // no element evidence it stays `Concrete(Collection)`, an untyped
        // array.
        ParamKind::Collection(element) => json!({
            "type": "array",
            "description": "collection; send as a JSON array, one entry per item",
            "items": property_schema(element),
        }),
    }
}

/// Whether a property describes a transformation's input or an intent's
/// payload. Only the `Collection` description differs: advice on sending
/// one makes no sense for a payload the embedder only reads.
#[derive(Clone, Copy)]
enum SchemaContext {
    TransformationArg,
    IntentPayload,
}

/// A concrete kind's property: its bare shape plus its description.
fn concrete_property(kind: &PredicateArgKind, ctx: SchemaContext) -> Value {
    let mut value = bare_kind_shape(kind);
    if let Some(obj) = value.as_object_mut()
        && let Some(desc) = concrete_kind_description(kind, ctx)
    {
        obj.insert("description".into(), Value::String(desc));
    }
    value
}

/// The JSON Schema type/format/pattern for a concrete kind, with no
/// description.
fn bare_kind_shape(kind: &PredicateArgKind) -> Value {
    match kind {
        // No `format: "uuid"`: a subject can also be a domain symbol such
        // as a commodity code.
        PredicateArgKind::Subject => json!({"type": "string"}),
        PredicateArgKind::Decimal => {
            json!({"type": "string", "pattern": r"^-?(0|[1-9]\d*)(\.\d+)?$"})
        }
        PredicateArgKind::Date => json!({"type": "string", "format": "date"}),
        PredicateArgKind::Timestamp => json!({"type": "string", "format": "date-time"}),
        PredicateArgKind::Duration => json!({"type": "string", "format": "duration"}),
        PredicateArgKind::Bool => json!({"type": "boolean"}),
        PredicateArgKind::Collection => json!({"type": "array"}),
        // Same wire shape as `Decimal`; the declaration fixes the unit.
        // The unit is also in the description, because many form
        // generators ignore custom extensions.
        PredicateArgKind::Quantity(u) => json!({
            "type": "string",
            "pattern": r"^-?(0|[1-9]\d*)(\.\d+)?$",
            "x-morpholog-unit": u.as_str(),
        }),
        // Unreachable: the validator refuses this kind in any declaration.
        PredicateArgKind::CalendarSpan => json!(false),
        // No constraint; the property's description says it is
        // polymorphic.
        PredicateArgKind::Any => json!({}),
    }
}

/// The description for a concrete kind, or `None` where the type says
/// enough. Only `Collection` depends on `ctx`.
fn concrete_kind_description(kind: &PredicateArgKind, ctx: SchemaContext) -> Option<String> {
    let owned = match kind {
        PredicateArgKind::Quantity(u) => {
            return Some(format!(
                "exact decimal amount in {u}, carried as a string for \
                 exactness. The unit is fixed by the declaration \
                 (`Decimal[{u}]`) and is not sent with the value."
            ));
        }
        _ => match kind {
            PredicateArgKind::Subject => Some(
                "opaque Morpholog subject identifier or domain symbol. \
             Subjects minted by `Stmt::LetNewSubject` are UUIDv7 by \
             runtime convention; externally supplied Subjects (commodity \
             codes, period names, direction enums, etc.) are opaque \
             strings. The schema describes the shape, not a format constraint.",
            ),
            PredicateArgKind::Decimal => {
                Some("arbitrary-precision decimal carried as a string for exactness")
            }
            PredicateArgKind::Date => Some("ISO-8601 civil date (YYYY-MM-DD)"),
            PredicateArgKind::Timestamp => Some(
                "RFC 3339 UTC instant (e.g. 2026-10-24T14:00:00Z). Zone-less \
             by design: local-time interpretation is admitted as claims, \
             never assumed by the runtime.",
            ),
            PredicateArgKind::Duration => Some(
                "ISO-8601 duration in exact time units (e.g. PT6H); calendar \
             units (months, years) are not accepted",
            ),
            PredicateArgKind::Collection => Some(match ctx {
                SchemaContext::TransformationArg => {
                    "collection; item kind not tracked at the kernel level in v0. \
                 A Collection parameter cannot be sent via `--args-named` (the \
                 named codec cannot decode bare arrays without per-item kind \
                 information); use `--args` with the tagged EvalValue codec."
                }
                SchemaContext::IntentPayload => {
                    "collection; a positional array of values, item kind not \
                 tracked at the kernel level in v0."
                }
            }),
            PredicateArgKind::Bool
            | PredicateArgKind::Any
            | PredicateArgKind::CalendarSpan
            | PredicateArgKind::Quantity(_) => None,
        },
    };
    owned.map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advice on sending a collection via `--args` appears for a
    /// transformation argument, never for a read-only intent payload.
    #[test]
    fn collection_description_splits_by_context() {
        let input = concrete_property(
            &PredicateArgKind::Collection,
            SchemaContext::TransformationArg,
        );
        let input_desc = input["description"].as_str().expect("description string");
        assert!(
            input_desc.contains("--args"),
            "transformation-arg Collection points at the --args codec; got: {input_desc}",
        );

        let payload =
            concrete_property(&PredicateArgKind::Collection, SchemaContext::IntentPayload);
        let payload_desc = payload["description"].as_str().expect("description string");
        assert!(
            !payload_desc.contains("--args"),
            "intent-payload Collection is read-only output; must not mention a send codec; got: {payload_desc}",
        );

        // The bare type shape is the same in both contexts.
        assert_eq!(input["type"], "array");
        assert_eq!(payload["type"], "array");
    }

    /// A collection with an inferred element kind emits typed `items`;
    /// the generated client's `list[str]` field relies on it.
    #[test]
    fn an_inferred_collection_emits_typed_items() {
        let kind = ParamKind::Collection(Box::new(ParamKind::Concrete(PredicateArgKind::Subject)));
        let schema = property_schema(&kind);
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"]["type"], "string");
    }
}
