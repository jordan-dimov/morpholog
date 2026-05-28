//! Shared decoder for `run` and `explain` transformation arguments.
//!
//! The CLI accepts two flag-distinguished input forms; both decode
//! into the same positional `Vec<EvalValue>` the kernel expects.
//! Centralising the decode here keeps the two commit / dry-run
//! paths from drifting on what counts as a valid input.
//!
//! - `--args` (tagged) is the implementer-facing codec: a JSON
//!   array of adjacently-tagged `EvalValue`s, exactly the wire
//!   shape the kernel uses internally. Polymorphic / Ambiguous /
//!   Collection parameters reach the kernel through this path
//!   because the per-element type tags resolve any ambiguity.
//!
//! - `--args-named` (bare-by-name) is the embedder-facing codec:
//!   a JSON object keyed by parameter name with bare values
//!   matching the schema (`morpholog schema <file>
//!   <transformation>`). Strict: missing required keys, unknown
//!   keys, wrong types, and `null` are all rejected. Refuses
//!   `Polymorphic`, `Unconstrained`, `Ambiguous`, and `Collection`
//!   parameters because the schema cannot give an unambiguous
//!   kind for them; each error points at the tagged codec as the
//!   fallback. The schema is the contract; the codec converts
//!   already-valid input - the embedder validates against the
//!   schema before sending if they want pre-flight checking.

use anyhow::{Context, anyhow, bail};
use jiff::civil::Date;
use morpholog_core::{
    EvalValue, ParamKind, PredicateArgKind, Program, Subject, Transformation, TransformationName,
    transformation_param_kinds,
};
use rust_decimal::Decimal;
use serde_json::Value;
use std::path::Path;
use std::str::FromStr;

/// The two flag-distinguished inputs `run` and `explain` accept.
/// Constructed by the caller after Clap has enforced one-of-two;
/// the decoder does not re-check exclusivity.
pub(crate) enum CliArgs<'a> {
    Tagged(&'a str),
    Named(&'a str),
}

/// Decode the CLI's `--args` or `--args-named` payload into the
/// positional `Vec<EvalValue>` the kernel consumes. `program` and
/// `transformation` are used by the named path to project parameter
/// kinds via [`transformation_param_kinds`]; the tagged path
/// ignores them. `file` is included so error messages can point at
/// the right `morpholog schema` invocation.
pub(crate) fn decode_args(
    program: &Program,
    transformation: &Transformation,
    file: &Path,
    input: CliArgs<'_>,
) -> anyhow::Result<Vec<EvalValue>> {
    match input {
        CliArgs::Tagged(json) => decode_tagged(json),
        CliArgs::Named(json) => decode_named(program, transformation, file, json),
    }
}

fn decode_tagged(json: &str) -> anyhow::Result<Vec<EvalValue>> {
    serde_json::from_str(json).context(
        "failed to parse --args as a JSON array of EvalValues \
         (each element must be a tagged object such as \
         `{\"type\":\"subject\",\"value\":\"...\"}` or \
         `{\"type\":\"decimal\",\"value\":\"100\"}`)",
    )
}

fn decode_named(
    program: &Program,
    transformation: &Transformation,
    file: &Path,
    json: &str,
) -> anyhow::Result<Vec<EvalValue>> {
    let object: serde_json::Map<String, Value> =
        serde_json::from_str(json).context("failed to parse --args-named as a JSON object")?;

    let kinds = transformation_param_kinds(program, &transformation.name)
        .map_err(|e| anyhow!("internal: param-kind analysis failed: {e}"))?;

    let declared: Vec<&str> = kinds.iter().map(|(v, _)| v.as_str()).collect();
    let schema_hint = schema_hint(file, &transformation.name);

    // Reject unknown keys before reporting per-parameter problems,
    // so a typo surfaces clearly rather than as "missing required".
    let extra: Vec<&str> = object
        .keys()
        .filter(|k| !declared.contains(&k.as_str()))
        .map(|s| s.as_str())
        .collect();
    if !extra.is_empty() {
        bail!(
            "--args-named contains unknown parameter(s) `{}`; \
             expected: {}. {schema_hint}",
            extra.join("`, `"),
            declared.join(", "),
        );
    }

    let mut out = Vec::with_capacity(kinds.len());
    for (param, kind) in &kinds {
        let raw = object.get(param.as_str()).ok_or_else(|| {
            anyhow!("--args-named is missing required parameter `{param}`. {schema_hint}")
        })?;
        if raw.is_null() {
            bail!(
                "parameter `{param}` is `null`; --args-named does not accept null values. \
                 {schema_hint}"
            );
        }
        out.push(decode_value(param.as_str(), kind, raw, &schema_hint)?);
    }
    Ok(out)
}

fn decode_value(
    param: &str,
    kind: &ParamKind,
    raw: &Value,
    schema_hint: &str,
) -> anyhow::Result<EvalValue> {
    match kind {
        ParamKind::Concrete(PredicateArgKind::Subject) => decode_subject(param, raw, schema_hint),
        ParamKind::Concrete(PredicateArgKind::Decimal) => decode_decimal(param, raw, schema_hint),
        ParamKind::Concrete(PredicateArgKind::Date) => decode_date(param, raw, schema_hint),
        ParamKind::Concrete(PredicateArgKind::Bool) => decode_bool(param, raw, schema_hint),
        ParamKind::Concrete(PredicateArgKind::Collection) => bail!(
            "parameter `{param}` is Collection; --args-named cannot decode bare arrays \
             without per-item kind information (deferred until a worked example forces \
             collection item-kind tracking). Use --args with the tagged EvalValue codec \
             to send a collection."
        ),
        ParamKind::Concrete(PredicateArgKind::Any) | ParamKind::Polymorphic => bail!(
            "parameter `{param}` is Polymorphic; --args-named cannot infer an EvalValue kind. \
             Use --args with the tagged EvalValue codec, or constrain the parameter in \
             the model so its kind is observed."
        ),
        ParamKind::Unconstrained => bail!(
            "parameter `{param}` is Unconstrained (never observed in a kind-bearing position). \
             --args-named cannot infer an EvalValue kind. Use --args with the tagged \
             EvalValue codec, or use the parameter in the transformation body so its \
             kind is observed."
        ),
        ParamKind::Ambiguous(observed) => {
            let names: Vec<&'static str> = observed.iter().map(kind_label).collect();
            bail!(
                "parameter `{param}` is Ambiguous ({}); --args-named cannot choose a branch \
                 safely. Use --args with the tagged EvalValue codec, or refactor the model \
                 to expose distinct transformations or parameters.",
                names.join(", "),
            )
        }
    }
}

fn decode_subject(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    raw.as_str()
        .map(|s| EvalValue::Subject(Subject::from(s)))
        .ok_or_else(|| {
            anyhow!(
                "parameter `{param}` is Subject but received {}; expected a UUID string. \
                 {schema_hint}",
                describe_value(raw),
            )
        })
}

fn decode_decimal(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Decimal but received {}; expected a decimal string \
             (e.g. \"100.50\"). {schema_hint}",
            describe_value(raw),
        )
    })?;
    let d = Decimal::from_str(s).map_err(|e| {
        anyhow!(
            "parameter `{param}` is Decimal but `{s}` failed to parse: {e}. \
             Expected a numeric string."
        )
    })?;
    Ok(EvalValue::Decimal(d))
}

fn decode_date(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Date but received {}; expected an ISO-8601 civil date \
             string (e.g. \"2026-05-29\"). {schema_hint}",
            describe_value(raw),
        )
    })?;
    let d = s.parse::<Date>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Date but `{s}` failed to parse: {e}. \
             Expected YYYY-MM-DD."
        )
    })?;
    Ok(EvalValue::Date(d))
}

fn decode_bool(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    raw.as_bool().map(EvalValue::Bool).ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Bool but received {}; expected `true` or `false`. \
             {schema_hint}",
            describe_value(raw),
        )
    })
}

fn describe_value(raw: &Value) -> &'static str {
    match raw {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn kind_label(kind: &PredicateArgKind) -> &'static str {
    match kind {
        PredicateArgKind::Subject => "Subject",
        PredicateArgKind::Decimal => "Decimal",
        PredicateArgKind::Date => "Date",
        PredicateArgKind::Bool => "Bool",
        PredicateArgKind::Collection => "Collection",
        PredicateArgKind::Any => "Any",
    }
}

fn schema_hint(file: &Path, transformation: &TransformationName) -> String {
    format!(
        "Run `morpholog schema {} {}` to inspect the accepted shape.",
        file.display(),
        transformation,
    )
}
