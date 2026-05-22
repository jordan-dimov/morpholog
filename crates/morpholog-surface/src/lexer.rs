//! Lexer for the v0 surface fragment.
//!
//! Recognised tokens (P1 + P2a):
//!
//! - Top-level keywords: `program`, `predicate`.
//! - Kind keywords (lexer-level): `Subject`, `Decimal`, `Date`,
//!   `Bool`, `Collection`, `Any`.
//! - Boolean keywords (P2a): `not`, `and`, `implies`.
//! - Identifiers: `[a-zA-Z][a-zA-Z0-9_]*` and `_<rest>` for
//!   `_-prefixed` names. The bare `_` is the wildcard token, not
//!   an identifier.
//! - Decimal literals (P2a): `<digits>` or `<digits>.<digits>`.
//!   String-valued because the runtime stores decimals as
//!   `rust_decimal::Decimal` parsed from strings, never as floats.
//! - Punctuation: `(`, `)`, `:`, `,`.
//! - Comparators (P2a): `=`, `!=`, `<=`. Multi-char forms must
//!   be tried before single-char.
//! - Arithmetic (P2a): `+`, `-`.
//! - Wildcard (P2a): `_`.
//!
//! Deliberately NOT recognised in P2a: `true` / `false` bool
//! literals. The IR's `Value` enum has variants for `Decimal`,
//! `Subject`, and `Date` only - no `Value::Bool`. The runtime
//! `EvalValue::Bool` is produced by comparators and other
//! expressions; it never appears as an IR literal. Per the
//! surface doctrine in `docs/scope-and-ambition.md`, the surface
//! cannot create capabilities the kernel lacks: a `true` literal
//! at the surface would have nowhere to lower to. It lands when
//! a worked example forces `Value::Bool` into the IR, not before.
//!
//! Whitespace and `//` line comments are stripped at lex; they
//! never reach the parser. Output is a vector of `(Token, Span)`
//! pairs - span is a byte-offset range into the source, compatible
//! with [`crate::diagnostics::Span`] and `ariadne`.
//!
//! Reserved words recognised in P2a are the structural keywords
//! (`program`, `predicate`) and the kind names (`Subject`,
//! `Decimal`, `Date`, `Bool`, `Collection`, `Any`) plus the
//! boolean operators (`not`, `and`, `implies`). The lexer maps
//! each to a specific `Token::*` variant so the parser can
//! match against them directly. `true` and `false` deliberately
//! remain ordinary identifiers until the IR has a `Value::Bool`
//! literal - see the "Deliberately NOT recognised" block above.
//! An identifier in kind-position that doesn't match a reserved
//! kind falls through as `Token::Ident` and produces a
//! parse-time diagnostic.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::PredicateArgKind;
use std::fmt;

use crate::diagnostics::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    // ---- P1: top-level + predicate-declaration surface ----
    /// `program` keyword.
    KwProgram,
    /// `predicate` keyword.
    KwPredicate,
    /// Kind keyword in a predicate-arg position.
    Kind(PredicateArgKind),

    // ---- P2a: boolean composition ----
    /// `not` prefix operator.
    KwNot,
    /// `and` infix operator.
    KwAnd,
    /// `implies` infix operator.
    KwImplies,

    /// `true` or `false`: lexer-reserved but not parseable in
    /// v0. Reserved at the lexer level (rather than left as a
    /// plain identifier) so that users who write `require true`
    /// expecting bool-literal semantics get an "unexpected token
    /// `true`" parse error rather than a confusing
    /// `UnboundVariable("true")` at runtime. Lifts to a proper
    /// bool-literal token when a worked example forces
    /// `Value::Bool` into the IR.
    ReservedBoolLit(bool),

    // ---- Atoms ----
    /// Identifier: any reserved-keyword-free word matching
    /// `[a-zA-Z_][a-zA-Z0-9_]*`. The parser decides whether it's
    /// a variable, a predicate name, an argument name, or
    /// (later) a transformation name based on position.
    Ident(String),
    /// Bare `_`. Distinguished from identifiers because it means
    /// "match anything at this position", not "a name".
    Wildcard,
    /// Decimal literal carried as a string to preserve exactness;
    /// the runtime parses to `rust_decimal::Decimal`. Never a
    /// float.
    DecimalLit(String),

    // ---- Punctuation ----
    LParen,
    RParen,
    Colon,
    Comma,

    // ---- P2a: operators ----
    /// `=` (Eq).
    Eq,
    /// `!=` (Neq).
    Neq,
    /// `<=` (Le for decimals; in P2a, decimal-only - DateLe and
    /// the date surface land in P2b).
    Le,
    /// `+` (Add).
    Plus,
    /// `-` (Sub).
    Minus,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::KwProgram => write!(f, "`program`"),
            Token::KwPredicate => write!(f, "`predicate`"),
            Token::Kind(k) => write!(f, "kind `{:?}`", k),
            Token::KwNot => write!(f, "`not`"),
            Token::KwAnd => write!(f, "`and`"),
            Token::KwImplies => write!(f, "`implies`"),
            Token::ReservedBoolLit(b) => write!(f, "reserved bool literal `{b}`"),
            Token::Ident(s) => write!(f, "identifier `{s}`"),
            Token::Wildcard => write!(f, "`_`"),
            Token::DecimalLit(s) => write!(f, "decimal literal `{s}`"),
            Token::LParen => write!(f, "`(`"),
            Token::RParen => write!(f, "`)`"),
            Token::Colon => write!(f, "`:`"),
            Token::Comma => write!(f, "`,`"),
            Token::Eq => write!(f, "`=`"),
            Token::Neq => write!(f, "`!=`"),
            Token::Le => write!(f, "`<=`"),
            Token::Plus => write!(f, "`+`"),
            Token::Minus => write!(f, "`-`"),
        }
    }
}

/// Span-flavoured token alias used in the parser's input stream.
pub type SpannedToken = (Token, Span);

/// Lex a Morpholog source string into a token stream. Returns
/// either the full token stream (whitespace and comments stripped)
/// or a `Rich` error describing what could not be lexed.
///
/// Whitespace is the standard Unicode `is_whitespace` set;
/// comments are `//` to end-of-line.
pub fn lex(source: &str) -> Result<Vec<SpannedToken>, Vec<Rich<'_, char>>> {
    lexer().parse(source).into_result().map(|tokens| {
        tokens
            .into_iter()
            .map(|(t, s)| (t, s.start()..s.end()))
            .collect()
    })
}

fn lexer<'a>() -> impl Parser<'a, &'a str, Vec<(Token, SimpleSpan)>, extra::Err<Rich<'a, char>>> {
    // ---- Identifiers and reserved words ----
    //
    // The bare `_` is matched separately (as Token::Wildcard);
    // `_-prefixed` identifiers (e.g. `_foo`) remain identifiers.
    // We do this via the order of `choice` below: punctuation /
    // wildcard / decimal / ident, where ident is the catch-all.
    let ident_or_keyword = text::ascii::ident().map(|s: &str| match s {
        // P1 reserved words
        "program" => Token::KwProgram,
        "predicate" => Token::KwPredicate,
        "Subject" => Token::Kind(PredicateArgKind::Subject),
        "Decimal" => Token::Kind(PredicateArgKind::Decimal),
        "Date" => Token::Kind(PredicateArgKind::Date),
        "Bool" => Token::Kind(PredicateArgKind::Bool),
        "Collection" => Token::Kind(PredicateArgKind::Collection),
        "Any" => Token::Kind(PredicateArgKind::Any),
        // P2a reserved words
        "not" => Token::KwNot,
        "and" => Token::KwAnd,
        "implies" => Token::KwImplies,
        // `true` / `false` are reserved at the lexer level but
        // NOT parseable in v0 (no `Value::Bool` in the IR). The
        // parser rejects the token with an "unexpected" diagnostic;
        // this is honest and stable, where treating them as plain
        // identifiers would silently lower to `Term::Var("true")`
        // and explode at runtime as `UnboundVariable`. Lifts to a
        // bool-literal token when a worked example forces
        // `Value::Bool` into the IR.
        "true" => Token::ReservedBoolLit(true),
        "false" => Token::ReservedBoolLit(false),
        // Bare `_` is the wildcard, not an ident.
        "_" => Token::Wildcard,
        other => Token::Ident(other.to_string()),
    });

    // ---- Decimal literals ----
    //
    // `<digits>` or `<digits>.<digits>`. Carried as a string so
    // the runtime parses to `rust_decimal::Decimal` without ever
    // routing through f64. No underscore separators in v0; add
    // when a worked example forces them.
    let decimal_lit = text::digits(10)
        .then(just('.').then(text::digits(10)).or_not())
        .to_slice()
        .map(|s: &str| Token::DecimalLit(s.to_string()));

    // ---- Operators ----
    //
    // Multi-char forms come first in the choice so `!=` is matched
    // as one token (Neq), not `!` followed by `=`. Single `!` and
    // single `<` are not legal in P2a; if they appear, the next
    // token-attempt fails and the lex error surfaces with their
    // span.
    let operator = choice((
        just("!=").to(Token::Neq),
        just("<=").to(Token::Le),
        just('=').to(Token::Eq),
        just('+').to(Token::Plus),
        just('-').to(Token::Minus),
    ));

    let punct = choice((
        just('(').to(Token::LParen),
        just(')').to(Token::RParen),
        just(':').to(Token::Colon),
        just(',').to(Token::Comma),
    ));

    // Order matters: try the more-specific patterns (multi-char
    // operators, decimals) before the catch-all ident.
    let token =
        choice((operator, punct, decimal_lit, ident_or_keyword)).map_with(|t, e| (t, e.span()));

    // Line comments: `//` to newline (or EOF). Skipped entirely;
    // no inner padding so the outer `padding` parser is the single
    // source of truth for whitespace consumption.
    let line_comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();

    // Padding = zero or more (whitespace-run | line-comment).
    let padding = choice((text::whitespace().at_least(1).ignored(), line_comment)).repeated();

    // Consume leading padding, then any number of (token, trailing
    // padding) pairs, then EOF.
    padding
        .ignore_then(token.then_ignore(padding).repeated().collect())
        .then_ignore(end())
}

/// Convenience input adapter for the parser: wraps a vector of
/// `SpannedToken`s into a value-input stream that chumsky's `Input`
/// trait understands. The end-span is set to the byte position
/// immediately after the last token (or 0 for an empty stream).
pub fn token_stream(
    tokens: &[SpannedToken],
) -> impl ValueInput<'_, Token = Token, Span = SimpleSpan> {
    let end = tokens.last().map(|(_, s)| s.end).unwrap_or(0);
    chumsky::input::Stream::from_iter(
        tokens
            .iter()
            .map(|(t, s)| (t.clone(), SimpleSpan::from(s.clone()))),
    )
    .map(SimpleSpan::from(end..end), |(t, s)| (t, s))
}
