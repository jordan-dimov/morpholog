//! SQL identifier and literal quoting, shared by every place the crate
//! builds SQL text at runtime: the views renderer, the verify legs that
//! read runtime-named schemas, and provisioning DDL over role names.

/// Double-quote a SQL identifier, doubling any embedded quote. Every
/// identifier in generated SQL is quoted, even safe ones, so no caller
/// depends on PostgreSQL's case-folding.
pub(crate) fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Single-quote a SQL string literal, doubling any embedded apostrophe.
pub(crate) fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
