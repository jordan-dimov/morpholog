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

#[cfg(test)]
mod tests {
    use super::{quote_ident, quote_literal};

    // These two functions are the injection guard for every runtime-built
    // SQL string in this crate - including reads over a schema name the
    // caller supplies (`--views-schema`). sqlx 0.9 asks each such site to
    // be audited by hand; an audit that rests on an untested helper is
    // not worth much, so the escaping is pinned here.

    #[test]
    fn ordinary_identifiers_are_quoted_not_folded() {
        assert_eq!(quote_ident("morpholog_views"), "\"morpholog_views\"");
        // Quoted even when safe, so no caller depends on PostgreSQL's
        // case-folding.
        assert_eq!(quote_ident("Order"), "\"Order\"");
    }

    #[test]
    fn an_embedded_quote_is_doubled_not_dropped() {
        // The whole guard: a name carrying a double quote must not be
        // able to end the identifier early.
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn an_injection_attempt_stays_one_identifier() {
        // The shape that would matter if a schema name reached SQL
        // unquoted: the statement terminator and the trailing DDL must
        // end up inside the quoted name, not beside it.
        let hostile = "public\"; DROP TABLE morpholog.claims; --";
        let quoted = quote_ident(hostile);
        assert_eq!(quoted, "\"public\"\"; DROP TABLE morpholog.claims; --\"");
        // One opening and one closing quote delimit the whole thing:
        // every interior quote is doubled, so none of them can close it.
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert_eq!(quoted[1..quoted.len() - 1].matches('"').count() % 2, 0);
    }

    #[test]
    fn literals_double_the_apostrophe() {
        assert_eq!(quote_literal("plain"), "'plain'");
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(
            quote_literal("x'; DROP TABLE t; --"),
            "'x''; DROP TABLE t; --'"
        );
    }

    #[test]
    fn an_empty_name_is_still_delimited() {
        // Postgres rejects "" as an identifier, which is the right
        // outcome - but the quoting must not silently produce bare SQL.
        assert_eq!(quote_ident(""), "\"\"");
        assert_eq!(quote_literal(""), "''");
    }
}
