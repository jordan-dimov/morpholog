//! Shared decoder for `propose` and `explain` transformation arguments.
//! Both input forms decode into the positional `Vec<EvalValue>` the kernel
//! takes; one decoder keeps the commands agreeing on what is valid.
//!
//! - `--args` (tagged): a JSON array of tagged `EvalValue`s, the kernel's
//!   own wire shape. The type tags let it carry parameters whose kind the
//!   schema cannot pin down.
//!
//! - `--args-named`: a JSON object keyed by parameter name, with bare values
//!   matching `morpholog schema <file> <transformation>`. Strict: missing,
//!   unknown, mistyped and `null` keys are refused. Parameters with no single
//!   known kind (polymorphic, unconstrained, ambiguous, or a collection of
//!   unknown items) are refused too, and the error points to `--args`.

use anyhow::{Context, anyhow, bail};
use jiff::civil::Date;
use morpholog_core::{
    EvalValue, ParamKind, PredicateArgKind, Subject, Transformation, TransformationName,
    ValidatedProgram, transformation_param_kinds,
};
use serde_json::Value;
use std::path::Path;

/// The two inputs `propose` and `explain` accept. Clap has already
/// enforced that exactly one was given.
pub(crate) enum CliArgs<'a> {
    Tagged(&'a str),
    Named(&'a str),
}

/// Decode `--args` or `--args-named` into the positional `Vec<EvalValue>`
/// the kernel takes. The named path reads parameter kinds from `program`
/// and `transformation` via [`transformation_param_kinds`]; the tagged path
/// ignores them. What the named codec refuses is an [`ArgumentRefusal`].
pub(crate) fn decode_args(
    program: &ValidatedProgram<'_>,
    transformation: &Transformation,
    input: CliArgs<'_>,
) -> anyhow::Result<Vec<EvalValue>> {
    match input {
        CliArgs::Tagged(json) => decode_tagged(json),
        CliArgs::Named(json) => decode_named(program, transformation, json),
    }
}

/// What the named codec refused. `parameter` is the one request key it
/// can point at: the parameter whose value it refused (a collection
/// element names its parameter), a missing one, or a single unknown key.
/// It says where decoding failed, never why a rule refused a valid value.
#[derive(Debug)]
pub(crate) struct ArgumentRefusal {
    pub(crate) parameter: Option<String>,
    transformation: TransformationName,
    message: String,
}

impl std::fmt::Display for ArgumentRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ArgumentRefusal {}

/// The parameter an `invalid_arguments` failure names, when the named
/// codec could point at one. Read off the typed error, never the prose.
pub(crate) fn refused_parameter(reason: &anyhow::Error) -> Option<String> {
    reason
        .chain()
        .find_map(|e| e.downcast_ref::<ArgumentRefusal>())
        .and_then(|refusal| refusal.parameter.clone())
}

/// A one-shot command's argument refusal, ending with the schema command
/// for the file its caller named. A session or a service never adds
/// this: its caller cannot run the command, and the path is the server's.
pub(crate) fn with_schema_hint(reason: anyhow::Error, file: &Path) -> anyhow::Error {
    let Some(refusal) = reason
        .chain()
        .find_map(|e| e.downcast_ref::<ArgumentRefusal>())
    else {
        return reason;
    };
    ArgumentRefusal {
        parameter: refusal.parameter.clone(),
        transformation: refusal.transformation.clone(),
        message: format!(
            "{reason:#} Run `morpholog schema {} {}` to inspect the accepted shape.",
            file.display(),
            refusal.transformation,
        ),
    }
    .into()
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
    program: &ValidatedProgram<'_>,
    transformation: &Transformation,
    json: &str,
) -> anyhow::Result<Vec<EvalValue>> {
    let object: serde_json::Map<String, Value> =
        serde_json::from_str(json).context("failed to parse --args-named as a JSON object")?;

    let kinds = transformation_param_kinds(program, &transformation.name)
        .map_err(|e| anyhow!("internal: param-kind analysis failed: {e}"))?;

    let declared: Vec<&str> = kinds.iter().map(|(v, _)| v.as_str()).collect();
    let refusal = |parameter: Option<&str>, message: String| ArgumentRefusal {
        parameter: parameter.map(str::to_string),
        transformation: transformation.name.clone(),
        message,
    };

    // Reject unknown keys before reporting per-parameter problems,
    // so a typo surfaces clearly rather than as "missing required".
    let extra: Vec<&str> = object
        .keys()
        .filter(|k| !declared.contains(&k.as_str()))
        .map(String::as_str)
        .collect();
    if !extra.is_empty() {
        // One unknown key can be named; several cannot be one answer.
        let only = match extra.as_slice() {
            [only] => Some(*only),
            _ => None,
        };
        return Err(refusal(
            only,
            format!(
                "--args-named contains unknown parameter(s) `{}`; \
                 expected: {}.",
                extra.join("`, `"),
                declared.join(", "),
            ),
        )
        .into());
    }

    let mut out = Vec::with_capacity(kinds.len());
    for (param, kind) in &kinds {
        let name = Some(param.as_str());
        let raw = object.get(param.as_str()).ok_or_else(|| {
            refusal(
                name,
                format!("--args-named is missing required parameter `{param}`."),
            )
        })?;
        if raw.is_null() {
            return Err(refusal(
                name,
                format!("parameter `{param}` is `null`; --args-named does not accept null values."),
            )
            .into());
        }
        // Named here, where the parameter is known, so no decoder below
        // has to keep an error's type for the name to survive.
        let value =
            decode_value(param.as_str(), kind, raw).map_err(|e| refusal(name, format!("{e:#}")))?;
        out.push(value);
    }
    Ok(out)
}

fn decode_value(param: &str, kind: &ParamKind, raw: &Value) -> anyhow::Result<EvalValue> {
    match kind {
        ParamKind::Concrete(PredicateArgKind::Subject) => decode_subject(param, raw),
        ParamKind::Concrete(PredicateArgKind::Decimal) => decode_decimal(param, raw),
        ParamKind::Concrete(PredicateArgKind::Date) => decode_date(param, raw),
        ParamKind::Concrete(PredicateArgKind::Timestamp) => decode_timestamp(param, raw),
        ParamKind::Concrete(PredicateArgKind::Duration) => decode_duration(param, raw),
        // A quantity travels as a bare decimal string. The declaration
        // already fixes the unit, so it is attached here, not sent.
        ParamKind::Concrete(PredicateArgKind::Quantity(unit)) => {
            let EvalValue::Decimal(amount) = decode_decimal(param, raw).map_err(|e| {
                anyhow!("{e}").context(format!(
                    "parameter `{param}` is Decimal[{unit}]; the named codec takes the \
                     bare decimal amount (the unit comes from the declaration)"
                ))
            })?
            else {
                unreachable!("decode_decimal returns EvalValue::Decimal on success")
            };
            Ok(EvalValue::Quantity {
                amount,
                unit: unit.clone(),
            })
        }
        ParamKind::Concrete(PredicateArgKind::Bool) => decode_bool(param, raw),
        // Expression-only: a span shifts a date inside a rule body and
        // is never a governed value, so no argument can carry one.
        ParamKind::Concrete(PredicateArgKind::CalendarSpan) => bail!(
            "parameter `{param}` was inferred as a calendar span, which is \
             expression-only and cannot be supplied as an argument; write the \
             span as a literal (e.g. span(P3M)) in the transformation body \
             instead."
        ),
        // A collection with a known element kind: a JSON array, each item
        // decoded by that kind.
        ParamKind::Collection(element) => {
            let Value::Array(items) = raw else {
                bail!("parameter `{param}` is a collection; expected a JSON array.");
            };
            let decoded = items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    decode_value(param, element, item).map_err(|e| {
                        anyhow!("{e}").context(format!("collection `{param}`, item {i}"))
                    })
                })
                .collect::<anyhow::Result<Vec<EvalValue>>>()?;
            Ok(EvalValue::Collection(decoded))
        }
        // The item kind was never narrowed (say, a nested collection). Only
        // the tagged `--args` codec can type the items.
        ParamKind::Concrete(PredicateArgKind::Collection) => bail!(
            "parameter `{param}` is a collection whose item kind the model never observes; \
             --args-named cannot decode it. Use the parameter's items in the body so their \
             kind is observed, or send it via --args with the tagged EvalValue codec."
        ),
        ParamKind::Concrete(PredicateArgKind::Any) | ParamKind::Polymorphic => bail!(
            "parameter `{param}` is polymorphic (the schema cannot narrow its kind); \
             --args-named cannot infer an EvalValue kind. Use --args with the tagged \
             EvalValue codec, or constrain the parameter in the model so its kind is \
             observed."
        ),
        ParamKind::Unconstrained => bail!(
            "parameter `{param}` is Unconstrained (never observed in a kind-bearing position). \
             --args-named cannot infer an EvalValue kind. Use --args with the tagged \
             EvalValue codec, or use the parameter in the transformation body so its \
             kind is observed."
        ),
        ParamKind::Ambiguous(observed) => {
            // `Display` keeps the unit (`Decimal[USD]`).
            let names: Vec<String> = observed.iter().map(ToString::to_string).collect();
            bail!(
                "parameter `{param}` is Ambiguous ({}); --args-named cannot choose a branch \
                 safely. Use --args with the tagged EvalValue codec, or refactor the model \
                 to expose distinct transformations or parameters.",
                names.join(", "),
            )
        }
    }
}

/// Render an [`EvalValue`] as the bare JSON `--args-named` accepts.
/// Decimals, dates, timestamps, durations and quantity amounts are
/// strings, since a JSON number could lose precision. A quantity drops its
/// unit, which the declaration supplies. Collections recurse.
pub(crate) fn eval_value_to_bare_json(v: &EvalValue) -> Value {
    match v {
        EvalValue::Subject(s) => Value::String(s.to_string()),
        EvalValue::Decimal(d) => Value::String(d.to_string()),
        EvalValue::Date(d) => Value::String(d.to_string()),
        EvalValue::Timestamp(t) => Value::String(t.to_string()),
        EvalValue::Duration(d) => Value::String(d.to_string()),
        // The bare amount only: the declaration carries the unit.
        EvalValue::Quantity { amount, .. } => Value::String(amount.to_string()),
        EvalValue::Bool(b) => Value::Bool(*b),
        // A span never reaches storage or the wire, but render it
        // rather than panic.
        EvalValue::CalendarSpan(s) => Value::String(s.to_string()),
        EvalValue::Collection(items) => {
            Value::Array(items.iter().map(eval_value_to_bare_json).collect())
        }
    }
}

/// Every scalar decoder starts here: refuse a non-string with one message
/// shape naming the kind, what arrived, what was expected, and the schema.
fn require_str<'v>(
    param: &str,
    raw: &'v Value,
    kind: &str,
    expected: &str,
) -> anyhow::Result<&'v str> {
    raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is {kind} but received {}; expected {expected}.",
            describe_value(raw),
        )
    })
}

fn decode_subject(param: &str, raw: &Value) -> anyhow::Result<EvalValue> {
    let s = require_str(param, raw, "Subject", "a string")?;
    // Any string is a subject: minted ids and domain symbols (commodity
    // codes, period names) alike. The IR pins no format, so neither does
    // the codec. An embedder wanting stricter checks adds its own.
    Ok(EvalValue::Subject(Subject::from(s)))
}

fn decode_decimal(param: &str, raw: &Value) -> anyhow::Result<EvalValue> {
    let s = require_str(param, raw, "Decimal", "a decimal string (e.g. \"100.50\")")?;
    let d = morpholog_core::schema::decimal_from_wire(s)
        .map_err(|e| anyhow!("parameter `{param}` is Decimal but {e}."))?;
    Ok(EvalValue::Decimal(d))
}

fn decode_date(param: &str, raw: &Value) -> anyhow::Result<EvalValue> {
    let s = require_str(
        param,
        raw,
        "Date",
        "an ISO-8601 civil date string (e.g. \"2026-05-29\")",
    )?;
    let d = s.parse::<Date>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Date but `{s}` failed to parse: {e}. \
             Expected YYYY-MM-DD."
        )
    })?;
    Ok(EvalValue::Date(d))
}

fn decode_timestamp(param: &str, raw: &Value) -> anyhow::Result<EvalValue> {
    let s = require_str(
        param,
        raw,
        "Timestamp",
        "an RFC 3339 instant string (e.g. \"2026-10-24T14:00:00Z\")",
    )?;
    let t = s.parse::<jiff::Timestamp>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Timestamp but `{s}` failed to parse: {e}. \
             Expected RFC 3339 (e.g. 2026-10-24T14:00:00Z)."
        )
    })?;
    Ok(EvalValue::Timestamp(t))
}

fn decode_duration(param: &str, raw: &Value) -> anyhow::Result<EvalValue> {
    let s = require_str(
        param,
        raw,
        "Duration",
        "an ISO-8601 duration string in exact time units (e.g. \"PT6H\")",
    )?;
    let d = s.parse::<jiff::SignedDuration>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Duration but `{s}` failed to parse: {e}. \
             Expected ISO 8601 time units (e.g. PT6H, PT1H30M); calendar units \
             (months, years) are not accepted."
        )
    })?;
    Ok(EvalValue::Duration(d))
}

fn decode_bool(param: &str, raw: &Value) -> anyhow::Result<EvalValue> {
    raw.as_bool().map(EvalValue::Bool).ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Bool but received {}; expected `true` or `false`.",
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

/// Decode one bare `--where` value under its declared kind, reusing the
/// named codec so a filter and an argument agree on what "431.7" means.
/// The raw text is treated as the codec's string form; every kind the
/// named codec accepts as a string is therefore filterable.
pub(crate) fn decode_declared_value(
    field: &str,
    kind: &PredicateArgKind,
    raw: &str,
) -> anyhow::Result<EvalValue> {
    // A command line has only text, but the named codec wants JSON
    // booleans, so `--where settled=true` needs converting.
    let json = match (kind, raw) {
        (PredicateArgKind::Bool, "true") => Value::Bool(true),
        (PredicateArgKind::Bool, "false") => Value::Bool(false),
        _ => Value::String(raw.to_string()),
    };
    decode_value(field, &ParamKind::Concrete(kind.clone()), &json).map_err(|e| {
        anyhow!("{e:#} a --where value is written bare, as in --where invoice_id=inv_1")
    })
}

#[cfg(test)]
mod tests {
    /// The texts the binary's codec and the generated client's reader
    /// answer alike, shared with the client's test suite. `binary_only`
    /// is what this input takes but the binary never writes, so the
    /// client's reader refuses it.
    const WIRE_TEXTS: &str = include_str!("../../tests/golden/wire_texts.json");

    #[test]
    fn the_wire_texts_decode_as_the_corpus_says() {
        let corpus: serde_json::Value = serde_json::from_str(WIRE_TEXTS).unwrap();
        for (kind, param_kind) in [
            ("decimal", PredicateArgKind::Decimal),
            ("date", PredicateArgKind::Date),
            ("timestamp", PredicateArgKind::Timestamp),
            ("duration", PredicateArgKind::Duration),
        ] {
            let kind_cases = &corpus[kind];
            let texts = |key: &str| {
                kind_cases[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            };
            for text in texts("both_accept").into_iter().chain(texts("binary_only")) {
                let decoded =
                    decode_value("p", &ParamKind::Concrete(param_kind.clone()), &json!(text));
                assert!(decoded.is_ok(), "{kind} {text:?} is accepted: {decoded:?}");
            }
            for text in texts("both_refuse") {
                let decoded =
                    decode_value("p", &ParamKind::Concrete(param_kind.clone()), &json!(text));
                assert!(decoded.is_err(), "{kind} {text:?} is refused: {decoded:?}");
            }
            if kind != "decimal" {
                continue;
            }
            // The tagged route reads the same texts: bare, as a quantity's
            // amount, and nested in a collection.
            let tagged = |text: &str| {
                [
                    json!([{"type": "decimal", "value": text}]),
                    json!([{"type": "quantity", "value": {"amount": text, "unit": "USD"}}]),
                    json!([{"type": "collection", "value": [{"type": "decimal", "value": text}]}]),
                ]
            };
            for text in texts("both_accept") {
                for shape in tagged(&text) {
                    let decoded = decode_tagged(&shape.to_string());
                    assert!(decoded.is_ok(), "tagged {shape} is accepted: {decoded:?}");
                }
            }
            for text in texts("both_refuse") {
                for shape in tagged(&text) {
                    let decoded = decode_tagged(&shape.to_string());
                    assert!(decoded.is_err(), "tagged {shape} is refused: {decoded:?}");
                }
            }
        }
    }

    /// The generated client carries the schema's own decimal pattern.
    #[test]
    fn the_client_template_carries_the_decimal_pattern() {
        let line = format!(
            "_DECIMAL_PATTERN = re.compile(r\"{}\", re.ASCII)",
            morpholog_core::schema::DECIMAL_PATTERN
        );
        assert!(
            crate::commands::generate::VALUES_PY.contains(&line),
            "values.py must read `{line}`"
        );
    }

    use super::{decode_tagged, decode_value};
    use morpholog_core::{EvalValue, ParamKind, PredicateArgKind};
    use serde_json::json;

    fn subject_collection() -> ParamKind {
        ParamKind::Collection(Box::new(ParamKind::Concrete(PredicateArgKind::Subject)))
    }

    #[test]
    fn a_collection_param_decodes_a_json_array_item_by_item() {
        let raw = json!(["acct_a", "acct_b"]);
        let decoded = decode_value("accounts", &subject_collection(), &raw).unwrap();
        let EvalValue::Collection(items) = decoded else {
            panic!("expected a collection, got {decoded:?}");
        };
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], EvalValue::Subject(s) if s.as_str() == "acct_a"));
        assert!(matches!(&items[1], EvalValue::Subject(s) if s.as_str() == "acct_b"));
    }

    #[test]
    fn a_collection_param_rejects_a_non_array() {
        let raw = json!("not an array");
        assert!(decode_value("accounts", &subject_collection(), &raw).is_err());
    }

    #[test]
    fn a_collection_param_rejects_an_ill_typed_item() {
        // Each item is decoded by the element kind, so a bad one fails.
        let raw = json!(["acct_a", 42]);
        assert!(decode_value("accounts", &subject_collection(), &raw).is_err());
    }
}
