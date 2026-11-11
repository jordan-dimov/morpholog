//! The claims table's own key for a claim's arguments, computed without
//! the database: `arguments_hash` is `sha256` over the JSONB's text as
//! PostgreSQL prints it, so the baseline's order by
//! `(predicate_name, arguments_hash)` can be checked by a verifier that
//! never sees the table. The text is the kernel's rendering of a
//! `jsonb`, shared with the digest builtin.

use morpholog_core::EvalValue;
use morpholog_core::digest::jsonb_text;
use sha2::{Digest as _, Sha256};

use crate::merkle::Hash;

/// `morpholog.claim_digest(arguments)` for these arguments.
pub fn arguments_hash(args: &[EvalValue]) -> Result<Hash, String> {
    let value = serde_json::to_value(args).map_err(|e| e.to_string())?;
    let mut text = String::new();
    jsonb_text(&value, &mut text)?;
    Ok(Sha256::digest(text.as_bytes()).into())
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
