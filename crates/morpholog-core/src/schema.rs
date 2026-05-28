//! JSON Schema emission for a transformation's argument contract.
//!
//! The first encoding of the analysis-layer truth ([`ParamKind`] from
//! [`crate::analysis`]) into a form an external embedder can lean on:
//! a JSON Schema (Draft 2020-12) describing exactly which named
//! arguments a transformation expects and what kind each one must
//! carry.
//!
//! Module boundary: this is **adapter**, not kernel. The kernel
//! exports the inferred input contract (the [`ParamKind`] result);
//! this module renders one encoding of it. A future embedder may
//! want a different rendering (OpenAPI components, a Python
//! dataclass, a TypeScript interface, an HTML form) - each is a
//! separate adapter built from the same analysis result, never a
//! second source of truth. JSON Schema sits in `morpholog-core`
//! today because there is one adapter and it is small; a
//! `morpholog-schema` crate becomes the right home if a second
//! encoding ever lands and they need to share helpers.
//!
//! The mapping leans toward stable, embedder-friendly encodings over
//! exhaustively re-stating the kernel's contract:
//! - Subjects are opaque strings; the schema marks them
//!   `format: "uuid"` because the runtime convention is UUIDv7
//!   (per `CLAUDE.md`), but the kernel itself does not check
//!   version, so the description names the convention.
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
use crate::ir::{PredicateArgKind, Program, TransformationName};

/// Emit a JSON Schema (Draft 2020-12) for the named transformation's
/// argument object. Parameters appear in declaration order under
/// `properties`, all are `required`, and `additionalProperties` is
/// `false` so the embedder's caller cannot smuggle in extra fields.
///
/// Pure adapter over [`transformation_param_kinds`]: every error
/// from the analysis layer bubbles through unchanged.
pub fn transformation_arg_schema(
    program: &Program,
    name: &TransformationName,
) -> Result<Value, AnalysisError> {
    let kinds = transformation_param_kinds(program, name)?;

    let mut properties = serde_json::Map::with_capacity(kinds.len());
    let mut required = Vec::with_capacity(kinds.len());
    for (param, kind) in &kinds {
        properties.insert(param.as_str().to_string(), property_schema(*kind));
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

/// Map one parameter's [`ParamKind`] to its JSON Schema property
/// fragment. Centralised so the per-kind encoding is one switch the
/// reader can audit at a glance.
fn property_schema(kind: ParamKind) -> Value {
    match kind {
        ParamKind::Concrete(PredicateArgKind::Subject) => json!({
            "type": "string",
            "format": "uuid",
            "description": "opaque Morpholog subject identifier (UUIDv7 by runtime convention)"
        }),
        ParamKind::Concrete(PredicateArgKind::Decimal) => json!({
            "type": "string",
            "pattern": r"^-?(0|[1-9]\d*)(\.\d+)?$",
            "description": "arbitrary-precision decimal carried as a string for exactness"
        }),
        ParamKind::Concrete(PredicateArgKind::Date) => json!({
            "type": "string",
            "format": "date",
            "description": "ISO-8601 civil date (YYYY-MM-DD)"
        }),
        ParamKind::Concrete(PredicateArgKind::Bool) => json!({
            "type": "boolean"
        }),
        ParamKind::Concrete(PredicateArgKind::Collection) => json!({
            "type": "array",
            "description": "collection; item kind not tracked at the kernel level in v0"
        }),
        // Concrete(Any) is not produced by the analysis layer
        // (resolve() maps Known(Any) to Polymorphic), but the type
        // permits it; render the same shape as Polymorphic so the
        // schema stays honest if the analysis surface ever widens.
        ParamKind::Concrete(PredicateArgKind::Any) | ParamKind::Polymorphic => json!({
            "description": "polymorphic; the model does not narrow this parameter's kind"
        }),
        ParamKind::Unconstrained => json!({
            "description": "unconstrained; parameter is never observed at a kind-bearing position (likely a modelling smell)"
        }),
    }
}
