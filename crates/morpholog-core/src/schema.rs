//! JSON Schema emission for a transformation's argument contract.
//!
//! Renders the analysis-layer truth ([`ParamKind`] from
//! [`crate::analysis`]) into a form an external embedder can lean on:
//! a JSON Schema (Draft 2020-12) describing exactly which named
//! arguments a transformation expects and what kind each one must
//! carry.
//!
//! Module boundary: this is **adapter**, not kernel. The kernel
//! exports the inferred input contract (the [`ParamKind`] result);
//! this module renders one encoding of it. A future embedder may
//! want a different rendering (OpenAPI components, a Python
//! dataclass, a TypeScript interface, an HTML form) - each would be
//! a separate adapter built from the same analysis result, never a
//! second source of truth. JSON Schema sits in `morpholog-core`
//! today because it is small and self-contained; extraction into a
//! `morpholog-schema` crate becomes the right home when additional
//! encodings need shared helpers.
//!
//! The mapping leans toward stable, embedder-friendly encodings over
//! exhaustively re-stating the kernel's contract:
//! - Subjects render as `{"type": "string"}` with NO `format`.
//!   Morpholog's `Subject` is the only primitive noun and carries
//!   both minted entity identifiers and domain symbols (commodity
//!   codes, period names, direction enums, account codes); the IR
//!   treats `Subject` as an opaque string newtype and the schema
//!   mirrors that. Subjects minted by `Stmt::LetNewSubject` are
//!   UUIDv7 by runtime convention, but externally supplied Subjects
//!   need not be; the description names the convention without
//!   pinning it as a JSON-Schema constraint.
//! - Decimals carry as strings (not JSON numbers) because the kernel
//!   stores them as exact source strings; the pattern is strict
//!   enough to reject `00.12`, leading-`+`, and other ambiguous
//!   forms that the parser normalises.
//! - Dates carry as ISO-8601 civil dates (no time of day, no zone) -
//!   the only temporal primitive in v0.
//! - `Polymorphic` and `Unconstrained` parameters become properties
//!   with no `type` constraint and a description carrying their
//!   state - the embedder can render them but should flag that the
//!   kernel cannot help narrow them.

use serde_json::{Value, json};

use crate::analysis::{AnalysisError, ParamKind, transformation_param_kinds};
use crate::ir::{IntentName, PredicateArgKind, TransformationName};
use crate::validate::ValidatedProgram;

/// Emit a JSON Schema (Draft 2020-12) for the named transformation's
/// argument object. Parameters appear in declaration order under
/// `properties`, all are `required`, and `additionalProperties` is
/// `false` so the embedder's caller cannot smuggle in extra fields.
///
/// Pure adapter over [`transformation_param_kinds`]: every error
/// from the analysis layer bubbles through unchanged. Takes a
/// [`ValidatedProgram`] so the validation precondition is enforced
/// at the type level (and so the schema layer does not re-validate
/// after the caller already has).
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

    Ok(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": name.as_str(),
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties,
    }))
}

/// Emit a JSON Schema (Draft 2020-12) for the named intent's payload
/// object. The embedder-facing dual of [`transformation_arg_schema`]:
/// where that describes what a transformation *accepts*, this describes
/// what an emitted intent *carries*, so a deliverer reading an outbox
/// payload can decode it by name instead of by hand-coded position.
///
/// Intent arguments are *declared* with explicit kinds (unlike
/// transformation parameters, whose kinds are inferred), so this is a
/// direct render with no analysis - hence a plain `Option` (the intent
/// is declared or it is not) rather than the analysis-layer `Result`.
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
            concrete_property(arg.kind, SchemaContext::IntentPayload),
        );
        required.push(Value::String(arg.name.clone()));
    }

    Some(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": name.as_str(),
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties,
    }))
}

/// Map one parameter's [`ParamKind`] to its JSON Schema property
/// fragment. Centralised so the per-kind encoding is one switch the
/// reader can audit at a glance. `Ambiguous` renders as `anyOf` over
/// each observed kind's bare type/format/pattern fragment - the
/// per-kind description is dropped from the alternatives so the
/// embedder does not see a misleading "opaque Morpholog subject
/// identifier" as one of N options. The contract-level description
/// belongs at the property level, naming the ambiguity.
fn property_schema(kind: &ParamKind) -> Value {
    match kind {
        ParamKind::Concrete(k) => concrete_property(*k, SchemaContext::TransformationArg),
        // `Polymorphic` is the projection of "observed only at `Any`
        // slots"; the analysis layer never emits `Concrete(Any)`, but
        // the type permits it, so render it the same way.
        ParamKind::Polymorphic => json!({
            "description": "polymorphic; the model does not narrow this parameter's kind"
        }),
        ParamKind::Unconstrained => json!({
            "description": "unconstrained; parameter is never observed at a kind-bearing position (likely a modelling smell)"
        }),
        ParamKind::Ambiguous(kinds) => {
            let alternatives: Vec<Value> = kinds.iter().map(|k| bare_kind_shape(*k)).collect();
            json!({
                "description": "ambiguous; parameter is observed at different concrete kinds across branch-local positions (typically `Or` branches the static checker does not refine across)",
                "anyOf": alternatives,
            })
        }
    }
}

/// Whether a property is rendered for a transformation's *input*
/// arguments or an emitted intent's *payload*. Only the `Collection`
/// description differs: the input rendering points the caller at the
/// `--args` codec for *sending* a collection, which is meaningless for a
/// read-only payload field the embedder only ever decodes.
#[derive(Clone, Copy)]
enum SchemaContext {
    TransformationArg,
    IntentPayload,
}

/// The `Concrete`-kind property: the bare type/format/pattern shape
/// plus the per-kind, context-aware description. Shared by
/// [`property_schema`]'s `Concrete` arm and by [`intent_arg_schema`],
/// whose declared arguments are always concrete kinds.
fn concrete_property(kind: PredicateArgKind, ctx: SchemaContext) -> Value {
    let mut value = bare_kind_shape(kind);
    if let Some(obj) = value.as_object_mut()
        && let Some(desc) = concrete_kind_description(kind, ctx)
    {
        obj.insert("description".into(), Value::String(desc.into()));
    }
    value
}

/// The bare JSON-Schema type/format/pattern shape for a concrete
/// kind, without any descriptive text. Reused by [`property_schema`]
/// for both the `Concrete` rendering (which adds the per-kind
/// description on top) and the `Ambiguous` `anyOf` alternatives
/// (which deliberately omit per-alternative descriptions).
fn bare_kind_shape(kind: PredicateArgKind) -> Value {
    match kind {
        // `Subject` deliberately carries NO `format: "uuid"`.
        // Morpholog's `Subject` is the only primitive noun and
        // represents both minted entity identifiers (UUIDv7 by
        // runtime convention) and domain symbols (commodity codes,
        // direction enums, period names, etc.). The IR does not
        // pin a format; the schema mirrors that. An embedder that
        // wants UUID validation for a specific parameter layers
        // its own constraint on top in its pre-flight schema.
        PredicateArgKind::Subject => json!({"type": "string"}),
        PredicateArgKind::Decimal => {
            json!({"type": "string", "pattern": r"^-?(0|[1-9]\d*)(\.\d+)?$"})
        }
        PredicateArgKind::Date => json!({"type": "string", "format": "date"}),
        PredicateArgKind::Bool => json!({"type": "boolean"}),
        PredicateArgKind::Collection => json!({"type": "array"}),
        // `Any` carries no constraint at the JSON-Schema level; the
        // contract-level "this is polymorphic" lives in the
        // property's description, not on the bare shape.
        PredicateArgKind::Any => json!({}),
    }
}

/// The per-kind description used by the `Concrete` rendering. `None`
/// for kinds where the JSON-Schema type alone is descriptive enough
/// (booleans). Only `Collection` reads `ctx`: a transformation argument
/// points at the `--args` codec for *sending* a collection; an intent
/// payload field is read-only output, so that guidance would mislead.
fn concrete_kind_description(kind: PredicateArgKind, ctx: SchemaContext) -> Option<&'static str> {
    match kind {
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
        PredicateArgKind::Bool | PredicateArgKind::Any => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `Collection` description is input-specific: a transformation
    /// argument is pointed at the `--args` codec for sending a
    /// collection, but an intent payload is read-only output, so that
    /// guidance must not leak into the `schema --intent` contract.
    #[test]
    fn collection_description_splits_by_context() {
        let input = concrete_property(
            PredicateArgKind::Collection,
            SchemaContext::TransformationArg,
        );
        let input_desc = input["description"].as_str().expect("description string");
        assert!(
            input_desc.contains("--args"),
            "transformation-arg Collection points at the --args codec; got: {input_desc}",
        );

        let payload = concrete_property(PredicateArgKind::Collection, SchemaContext::IntentPayload);
        let payload_desc = payload["description"].as_str().expect("description string");
        assert!(
            !payload_desc.contains("--args"),
            "intent-payload Collection is read-only output; must not mention a send codec; got: {payload_desc}",
        );

        // The bare type shape is the same in both contexts.
        assert_eq!(input["type"], "array");
        assert_eq!(payload["type"], "array");
    }
}
