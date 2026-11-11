//! The bytes `digest(a, b, ...)` hashes: the stored value key of each
//! argument, in an array, printed as PostgreSQL prints JSONB. The key is
//! `morpholog.value_key_v1` from the schema, ported here so the kernel
//! needs no database, and the text form is the one `jsonb::text` gives,
//! so a compiled rendering could reuse the schema's own functions. The
//! bytes are the contract: a reveal is held to a commitment made outside
//! the kernel, so a change to one byte here would make every outstanding
//! commitment unrevealable. A new encoding is a new builtin name.

use crate::state::EvalValue;
use rust_decimal::Decimal;
use serde_json::Value as Json;
use sha2::{Digest as _, Sha256};

/// `morpholog.value_key_v1` over a stored value: the kind tag, then the
/// value with a decimal's scale trimmed, so `30.0` and `30` share a key.
pub fn value_key_v1(value: &EvalValue) -> Json {
    match value {
        EvalValue::Decimal(d) => Json::Array(vec![Json::from("decimal"), decimal_key(*d)]),
        EvalValue::Quantity { amount, unit } => Json::Array(vec![
            Json::from("quantity"),
            Json::from(unit.as_str()),
            decimal_key(*amount),
        ]),
        EvalValue::Collection(items) => Json::Array(vec![
            Json::from("collection"),
            Json::Array(items.iter().map(value_key_v1).collect()),
        ]),
        other => {
            let tagged = serde_json::to_value(other).expect("a stored value serialises");
            Json::Array(vec![tagged["type"].clone(), tagged["value"].clone()])
        }
    }
}

/// `trim_scale(numeric)::text`: no trailing zeros, no exponent, and a
/// zero is `0` whatever its sign or scale.
fn decimal_key(d: Decimal) -> Json {
    Json::from(d.normalize().to_string())
}

/// The preimage of `digest` over these arguments.
pub fn preimage(args: &[EvalValue]) -> String {
    let keys = Json::Array(args.iter().map(value_key_v1).collect());
    let mut out = String::new();
    jsonb_text(&keys, &mut out).expect("a value key holds no JSON number");
    out
}

/// `sha256:<hex>` over the preimage.
pub fn digest(args: &[EvalValue]) -> String {
    let hash = Sha256::digest(preimage(args).as_bytes());
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for byte in hash {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// JSON as PostgreSQL prints a `jsonb`: object keys sorted by length then
/// bytes, `", "` and `": "` separators, strings escaped as PostgreSQL
/// escapes them. Tagged values hold no JSON numbers, so a number is
/// refused rather than rendered.
pub fn jsonb_text(value: &Json, out: &mut String) -> Result<(), String> {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => {
            return Err(format!("a JSON number, {n}, has no fixed text"));
        }
        Json::String(s) => escape(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                jsonb_text(item, out)?;
            }
            out.push(']');
        }
        Json::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| {
                a.len()
                    .cmp(&b.len())
                    .then_with(|| a.as_bytes().cmp(b.as_bytes()))
            });
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                escape(key, out);
                out.push_str(": ");
                jsonb_text(&map[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// PostgreSQL's `escape_json`: the short escapes, `\u00xx` for the other
/// control characters, everything else as it is.
fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
