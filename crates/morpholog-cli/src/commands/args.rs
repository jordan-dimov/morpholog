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
    EvalValue, ParamKind, PredicateArgKind, Subject, Transformation, TransformationName,
    ValidatedProgram, transformation_param_kinds,
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
/// the right `morpholog schema` invocation. Takes a
/// [`ValidatedProgram`] so the named path does not re-validate after
/// the caller already has.
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
        ParamKind::Concrete(PredicateArgKind::Bool) => decode_bool(param, raw, schema_hint),
        ParamKind::Concrete(PredicateArgKind::Collection) => bail!(
            "parameter `{param}` is Collection; --args-named cannot decode bare arrays \
             without per-item kind information (deferred until a worked example forces \
             collection item-kind tracking). Use --args with the tagged EvalValue codec \
             to send a collection. {schema_hint}"
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
            let names: Vec<&'static str> = observed.iter().map(kind_label).collect();
            bail!(
                "parameter `{param}` is Ambiguous ({}); --args-named cannot choose a branch \
                 safely. Use --args with the tagged EvalValue codec, or refactor the model \
                 to expose distinct transformations or parameters. {schema_hint}",
                names.join(", "),
            )
        }
    }
}

/// Render an [`EvalValue`] as the bare JSON the named codec accepts -
/// the read-side mirror of `--args-named`. Exactness rules match the
/// write side: decimals, dates, timestamps, and durations are strings
/// (a JSON number would round-trip through a double), booleans are
/// booleans, collections recurse.
pub(crate) fn eval_value_to_bare_json(v: &EvalValue) -> Value {
    match v {
        EvalValue::Subject(s) => Value::String(s.to_string()),
        EvalValue::Decimal(d) => Value::String(d.to_string()),
        EvalValue::Date(d) => Value::String(d.to_string()),
        EvalValue::Timestamp(t) => Value::String(t.to_string()),
        EvalValue::Duration(d) => Value::String(d.to_string()),
        EvalValue::Bool(b) => Value::Bool(*b),
        EvalValue::Collection(items) => {
            Value::Array(items.iter().map(eval_value_to_bare_json).collect())
        }
    }
}

fn decode_subject(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Subject but received {}; expected a string. \
             {schema_hint}",
            describe_value(raw),
        )
    })?;
    // `Subject` is Morpholog's only primitive noun: it carries
    // both minted entity identifiers and domain symbols (commodity
    // codes, period names, direction enums, account codes). The IR
    // does not pin a format; the codec mirrors that and accepts any
    // string. Subjects minted by `Stmt::LetNewSubject` are UUIDv7
    // by runtime convention; externally supplied Subjects are
    // opaque. An embedder that wants stricter validation layers
    // its own constraint on top in its pre-flight schema.
    Ok(EvalValue::Subject(Subject::from(s)))
}

fn decode_decimal(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Decimal but received {}; expected a decimal string \
             (e.g. \"100.50\"). {schema_hint}",
            describe_value(raw),
        )
    })?;
    // The schema commits to `^-?(0|[1-9]\d*)(\.\d+)?$`; the codec
    // must match or the embedder validates against a stricter
    // contract than the CLI actually enforces. `Decimal::from_str`
    // alone is too lenient (accepts leading `+`, leading zeros,
    // trailing dot). Validate the shape first, then parse.
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

/// Decimal shape check matching the JSON Schema pattern emitted
/// by `morpholog-core::schema`. Kept as a hand-rolled scan rather
/// than pulling `regex` in just for this; the grammar is small,
/// monomorphic, and unlikely to change (a worked example forcing
/// scientific notation or different number conventions would be
/// the natural moment to revisit).
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
             Expected YYYY-MM-DD. {schema_hint}"
        )
    })?;
    Ok(EvalValue::Date(d))
}

fn decode_timestamp(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Timestamp but received {}; expected an RFC 3339 \
             instant string (e.g. \"2026-10-24T14:00:00Z\"). {schema_hint}",
            describe_value(raw),
        )
    })?;
    let t = s.parse::<jiff::Timestamp>().map_err(|e| {
        anyhow!(
            "parameter `{param}` is Timestamp but `{s}` failed to parse: {e}. \
             Expected RFC 3339 (e.g. 2026-10-24T14:00:00Z). {schema_hint}"
        )
    })?;
    Ok(EvalValue::Timestamp(t))
}

fn decode_duration(param: &str, raw: &Value, schema_hint: &str) -> anyhow::Result<EvalValue> {
    let s = raw.as_str().ok_or_else(|| {
        anyhow!(
            "parameter `{param}` is Duration but received {}; expected an ISO-8601 \
             duration string in exact time units (e.g. \"PT6H\"). {schema_hint}",
            describe_value(raw),
        )
    })?;
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

fn kind_label(kind: &PredicateArgKind) -> &'static str {
    match kind {
        PredicateArgKind::Subject => "Subject",
        PredicateArgKind::Decimal => "Decimal",
        PredicateArgKind::Date => "Date",
        PredicateArgKind::Timestamp => "Timestamp",
        PredicateArgKind::Duration => "Duration",
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
}
