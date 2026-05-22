//! Lexer for the v0 surface fragment.
//!
//! Recognises: the `program` and `predicate` keywords; the kind
//! keywords (`Subject`, `Decimal`, `Date`, `Bool`, `Collection`,
//! `Any`); identifiers (`[a-zA-Z_][a-zA-Z0-9_]*`); `(`, `)`, `:`,
//! `,`; `//`-style line comments. Whitespace and comments are
//! skipped; they never reach the parser.
//!
//! Output is a vector of `(Token, Span)` pairs. Span is a byte-
//! offset range into the source string, compatible with
//! [`crate::diagnostics::Span`] and `ariadne`.
//!
//! Kind keywords are recognised at the lexer level (rather than as
//! identifiers later) so that the parser can match against
//! [`Token::Kind`] directly. This also lets the parser produce
//! "unknown kind" diagnostics by knowing a token was an ident, not
//! a kind keyword - reserved-word recognition is a lexer concern.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::PredicateArgKind;
use std::fmt;

use crate::diagnostics::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// `program` keyword.
    KwProgram,
    /// `predicate` keyword.
    KwPredicate,
    /// One of the recognised kind keywords. Lexed as a distinct
    /// token so the parser can match without re-checking the string.
    Kind(PredicateArgKind),
    /// Any other word that matches identifier syntax. The parser
    /// decides whether it's a programme name, a predicate name, or
    /// an argument name based on position.
    Ident(String),
    LParen,
    RParen,
    Colon,
    Comma,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::KwProgram => write!(f, "`program`"),
            Token::KwPredicate => write!(f, "`predicate`"),
            Token::Kind(k) => write!(f, "kind `{:?}`", k),
            Token::Ident(s) => write!(f, "identifier `{s}`"),
            Token::LParen => write!(f, "`(`"),
            Token::RParen => write!(f, "`)`"),
            Token::Colon => write!(f, "`:`"),
            Token::Comma => write!(f, "`,`"),
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
    let ident_or_keyword = text::ascii::ident().map(|s: &str| match s {
        "program" => Token::KwProgram,
        "predicate" => Token::KwPredicate,
        "Subject" => Token::Kind(PredicateArgKind::Subject),
        "Decimal" => Token::Kind(PredicateArgKind::Decimal),
        "Date" => Token::Kind(PredicateArgKind::Date),
        "Bool" => Token::Kind(PredicateArgKind::Bool),
        "Collection" => Token::Kind(PredicateArgKind::Collection),
        "Any" => Token::Kind(PredicateArgKind::Any),
        other => Token::Ident(other.to_string()),
    });

    let punct = choice((
        just('(').to(Token::LParen),
        just(')').to(Token::RParen),
        just(':').to(Token::Colon),
        just(',').to(Token::Comma),
    ));

    let token = choice((ident_or_keyword, punct)).map_with(|t, e| (t, e.span()));

    // Line comments: `//` to newline (or EOF). Skipped entirely.
    let line_comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .padded()
        .ignored();

    // Whitespace and comments interleaved; padded() handles either.
    let padding = choice((text::whitespace().at_least(1).ignored(), line_comment)).repeated();

    token.padded_by(padding).repeated().collect()
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
