//! The claims table's own key for a claim's arguments, computed without
//! the database: `arguments_hash` is `sha256` over the JSONB's text as
//! PostgreSQL prints it, so the baseline's order by
//! `(predicate_name, arguments_hash)` can be checked by a verifier that
//! never sees the table. The rendering follows jsonb output: object keys
//! sorted by length then bytes, `", "` and `": "` separators, strings
//! escaped as PostgreSQL escapes them. Tagged values hold no JSON
//! numbers, so a number is refused rather than rendered.

use morpholog_core::EvalValue;
use sha2::{Digest as _, Sha256};

use crate::merkle::Hash;

/// `morpholog.claim_digest(arguments)` for these arguments.
pub fn arguments_hash(args: &[EvalValue]) -> Result<Hash, String> {
    let value = serde_json::to_value(args).map_err(|e| e.to_string())?;
    let mut text = String::new();
    jsonb_text(&value, &mut text)?;
    Ok(Sha256::digest(text.as_bytes()).into())
}

fn jsonb_text(value: &serde_json::Value, out: &mut String) -> Result<(), String> {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => {
            return Err(format!("a JSON number, {n}, has no fixed text"));
        }
        serde_json::Value::String(s) => escape(s, out),
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                jsonb_text(item, out)?;
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: serde_json::Value) -> String {
        let mut out = String::new();
        jsonb_text(&value, &mut out).unwrap();
        out
    }

    #[test]
    fn renders_as_postgresql_prints_jsonb() {
        assert_eq!(
            text(serde_json::json!([{"value": "a", "type": "subject"}])),
            r#"[{"type": "subject", "value": "a"}]"#
        );
        assert_eq!(
            text(serde_json::json!({"type": "quantity", "value": {"unit": "USD", "amount": "1"}})),
            r#"{"type": "quantity", "value": {"unit": "USD", "amount": "1"}}"#
        );
        assert_eq!(
            text(serde_json::json!({"bb": true, "a": [], "c": {}})),
            r#"{"a": [], "c": {}, "bb": true}"#
        );
        assert_eq!(
            text(serde_json::json!("q\"\\\n\t\u{1}é")),
            "\"q\\\"\\\\\\n\\t\\u0001é\""
        );
        assert!(jsonb_text(&serde_json::json!(1), &mut String::new()).is_err());
    }
}
