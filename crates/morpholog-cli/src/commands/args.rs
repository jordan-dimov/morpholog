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
use rust_decimal::Decimal;
use serde_json::Value;
use std::path::Path;
use std::str::FromStr;

/// The two inputs `propose` and `explain` accept. Clap has already
/// enforced that exactly one was given.
pub(crate) enum CliArgs<'a> {
    Tagged(&'a str),
    Named(&'a str),
}

/// Decode `--args` or `--args-named` into the positional `Vec<EvalValue>`
/// the kernel takes. The named path reads parameter kinds from `program`
/// and `transformation` via [`transformation_param_kinds`]; the tagged path
/// ignores them. `file` lets errors name the right `morpholog schema` call.
pub(crate) fn decode_args(
    program: &ValidatedProgram<'_>,
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
    program: &ValidatedProgram<'_>,
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
        .map(String::as_str)
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
        ParamKind::Concrete(PredicateArgKind::Timestamp) => {
            decode_timestamp(param, raw, schema_hint)
        }
        ParamKind::Concrete(PredicateArgKind::Duration) => decode_duration(param, raw, schema_hint),
        // A quantity travels as a bare decimal string. The declaration
        // already fixes the unit, so it is attached here, not sent.
        ParamKind::Concrete(PredicateArgKind::Quantity(unit)) => {
            let EvalValue::Decimal(amount) =
                decode_decimal(param, raw, schema_hint).map_err(|e| {
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
        ParamKind::Concrete(PredicateArgKind::Bool) => decode_bool(param, raw, schema_hint),
        // Expression-only: a span shifts a date inside a rule body and
        // is never a governed value, so no argument can carry one.
        ParamKind::Concrete(PredicateArgKind::CalendarSpan) => bail!(
            "parameter `{param}` was inferred as a calendar span, which is \
             expression-only and cannot be supplied as an argument; write the \
             span as a literal (e.g. span(P3M)) in the transformation body \
             instead. {schema_hint}"
        ),
        // A collection with a known element kind: a JSON array, each item
        // decoded by that kind.
        ParamKind::Collection(element) => {
            let Value::Array(items) = raw else {
                bail!("parameter `{param}` is a collection; expected a JSON array. {schema_hint}");
            };
            let decoded = items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    decode_value(param, element, item, schema_hint).map_err(|e| {
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
             kind is observed, or send it via --args with the tagged EvalValue codec. {schema_hint}"
        ),
        ParamKind::Concrete(PredicateArgKind::Any) | ParamKind::Polymorphic => bail!(
            "parameter `{param}` is polymorphic (the schema cannot narrow its kind); \
             --args-named cannot infer an EvalValue kind. Use --args with the tagged \
             EvalValue codec, or constrain the parameter in the model so its kind is \
             observed. {schema_hint}"
        ),
        ParamKind::Unconstrained => bail!(
            "parameter `{param}` is Unconstrained (never observed in a kind-bearing position). \
             --args-named cannot infer an EvalValue kind. Use --args with the tagged \
             EvalValue codec, or use the parameter in the transformation body so its \
             kind is observed. {schema_hint}"
        ),
        ParamKind::Ambiguous(observed) => {
            // `Display` keeps the unit (`Decimal[USD]`).
            let names: Vec<String> = observed.iter().map(ToString::to_string).collect();
            bail!(
                "parameter `{param}` is Ambiguous ({}); --args-named cannot choose a branch \
                 safely. Use --args with the tagged EvalValue codec, or refactor the model \
                 to expose distinct transformations or parameters. {schema_hint}",
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
    schema_hint: &str,
) -> anyhow::Result<&'v str> {
    raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is {kind} but received {}; expected {expected}. \
             {schema_hint}",
            describe_value(raw),
        )
    })
}

fn decode_subject(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = require_str(param, raw, "Subject", "a string", schema_hint)?;
    // Any string is a subject: minted ids and domain symbols (commodity
    // codes, period names) alike. The IR pins no format, so neither does
    // the codec. An embedder wanting stricter checks adds its own.
    Ok(EvalValue::Subject(Subject::from(s)))
}

fn decode_decimal(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = require_str(
        param,
        raw,
        "Decimal",
        "a decimal string (e.g. \"100.50\")",
        schema_hint,
    )?;
    // Match the schema's `^-?(0|[1-9]\d*)(\.\d+)?$` exactly.
    // `Decimal::from_str` alone also accepts a leading `+`, leading zeros
    // and a trailing dot, so check the shape first.
    if !is_schema_decimal(s) {
        bail!(
            "parameter `{param}` is Decimal but `{s}` does not match the schema pattern \
             ^-?(0|[1-9]\\d*)(\\.\\d+)?$ (no leading `+`, no leading zeros except \"0\", \
             no trailing dot, no empty string). {schema_hint}"
        );
    }
    let d = Decimal::from_str(s).map_err(|e| {
        anyhow!(
            "parameter `{param}` is Decimal but `{s}` failed to parse: {e}. \
             Expected a numeric string. {schema_hint}"
        )
    })?;
    Ok(EvalValue::Decimal(d))
}

/// Decimal shape check matching the JSON Schema pattern from
/// `morpholog-core::schema`. Hand-rolled: the grammar is too small to be
/// worth a `regex` dependency.
fn is_schema_decimal(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    let (int_part, frac_part) = match body.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (body, None),
    };

    // Integer part: "0" or a non-zero digit followed by digits.
    let int_ok = if int_part == "0" {
        true
    } else {
        let mut chars = int_part.chars();
        match chars.next() {
            Some(c) if ('1'..='9').contains(&c) => chars.all(|c| c.is_ascii_digit()),
            _ => false,
        }
    };

    // Fractional part: present iff `.` was present, then at least
    // one digit and all digits.
    let frac_ok = match frac_part {
        None => true,
        Some(f) => !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()),
    };

    int_ok && frac_ok
}

fn decode_date(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = require_str(
        param,
        raw,
        "Date",
        "an ISO-8601 civil date string (e.g. \"2026-05-29\")",
        schema_hint,
    )?;
    let d = s.parse::<Date>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Date but `{s}` failed to parse: {e}. \
             Expected YYYY-MM-DD. {schema_hint}"
        )
    })?;
    Ok(EvalValue::Date(d))
}

fn decode_timestamp(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = require_str(
        param,
        raw,
        "Timestamp",
        "an RFC 3339 instant string (e.g. \"2026-10-24T14:00:00Z\")",
        schema_hint,
    )?;
    let t = s.parse::<jiff::Timestamp>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Timestamp but `{s}` failed to parse: {e}. \
             Expected RFC 3339 (e.g. 2026-10-24T14:00:00Z). {schema_hint}"
        )
    })?;
    Ok(EvalValue::Timestamp(t))
}

fn decode_duration(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = require_str(
        param,
        raw,
        "Duration",
        "an ISO-8601 duration string in exact time units (e.g. \"PT6H\")",
        schema_hint,
    )?;
    let d = s.parse::<jiff::SignedDuration>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Duration but `{s}` failed to parse: {e}. \
             Expected ISO 8601 time units (e.g. PT6H, PT1H30M); calendar units \
             (months, years) are not accepted. {schema_hint}"
        )
    })?;
    Ok(EvalValue::Duration(d))
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

fn schema_hint(file: &Path, transformation: &TransformationName) -> String {
    format!(
        "Run `morpholog schema {} {}` to inspect the accepted shape.",
        file.display(),
        transformation,
    )
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
    decode_value(
        field,
        &ParamKind::Concrete(kind.clone()),
        &json,
        "a --where value is written bare, as in --where invoice_id=inv_1",
    )
}

#[cfg(test)]
mod tests {
    use super::is_schema_decimal;

    #[test]
    fn accepts_canonical_decimal_forms() {
        for s in ["0", "1", "100", "100.50", "-1", "-100.50", "0.5", "-0.5"] {
            assert!(is_schema_decimal(s), "{s} should be accepted");
        }
    }

    #[test]
    fn rejects_forms_the_schema_pattern_excludes() {
        for s in [
            "",      // empty
            "+1",    // leading plus
            "00.12", // leading zero
            "01",    // leading zero on integer
            "1.",    // trailing dot
            ".5",    // no leading integer
            "1.2.3", // multiple dots
            "abc",   // non-numeric
            "1e10",  // scientific (deliberately out of scope in v0)
            "-",     // bare minus
            "-.5",   // minus before missing integer
        ] {
            assert!(!is_schema_decimal(s), "{s} should be rejected");
        }
    }

    use super::decode_value;
    use morpholog_core::{EvalValue, ParamKind, PredicateArgKind};
    use serde_json::json;

    fn subject_collection() -> ParamKind {
        ParamKind::Collection(Box::new(ParamKind::Concrete(PredicateArgKind::Subject)))
    }

    #[test]
    fn a_collection_param_decodes_a_json_array_item_by_item() {
        let raw = json!(["acct_a", "acct_b"]);
        let decoded = decode_value("accounts", &subject_collection(), &raw, "").unwrap();
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
        assert!(decode_value("accounts", &subject_collection(), &raw, "").is_err());
    }

    #[test]
    fn a_collection_param_rejects_an_ill_typed_item() {
        // Each item is decoded by the element kind, so a bad one fails.
        let raw = json!(["acct_a", 42]);
        assert!(decode_value("accounts", &subject_collection(), &raw, "").is_err());
    }
}
