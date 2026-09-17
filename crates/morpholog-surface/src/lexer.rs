//! Lexer for the v0 surface fragment.
//!
//! Recognised tokens:
//!
//! - Declaration keywords: `program`, `predicate`, `intent`,
//!   `invariant`, `transformation`, `derived`.
//! - Kind keywords: `Subject`, `Decimal`, `Date`, `Timestamp`,
//!   `Duration`, `Bool`, `Collection`, `Any`.
//! - Boolean keywords: `not`, `and`, `or`, `implies`, `pre`.
//! - Identifiers: `[a-zA-Z][a-zA-Z0-9_]*` and `_<rest>` for
//!   `_-prefixed` names. The bare `_` is the wildcard token, not
//!   an identifier.
//! - Decimal literals: `<digits>` or `<digits>.<digits>`,
//!   string-valued because the runtime parses to
//!   `rust_decimal::Decimal`, never to a float.
//! - Punctuation: `(`, `)`, `[`, `]`, `:`, `,`.
//! - Comparators: `=`, `!=`, `<=`, `<`, `>=`, `>`.
//! - Arithmetic: `+`, `-`, `*`, `/` (infix); `min` / `max` / `abs` (functions).
//! - Wildcard: `_`.
//!
//! Each reserved word maps to a specific `Token::*` variant so the
//! parser can match it directly; an identifier in kind-position that
//! matches no reserved kind falls through as `Token::Ident` and
//! produces a parse-time diagnostic.
//!
//! `true` and `false` are reserved at the lexer (as
//! `Token::ReservedBoolLit`) but not parseable: the IR has no
//! `Value::Bool`, so a bool literal has nowhere to lower to. Reserving
//! them lets the parser reject `require true` with an "unexpected
//! token" diagnostic, where treating them as identifiers would lower
//! to `Term::Var("true")` and explode at runtime as `UnboundVariable`.
//!
//! Whitespace and `--` line comments are stripped at lex. Output is a
//! vector of `(Token, Span)` pairs; the span is a byte-offset range
//! compatible with [`crate::diagnostics::Span`] and `ariadne`.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::PredicateArgKind;
use std::fmt;

use crate::diagnostics::Span;

/// The keyword vocabulary in one list. A keyword's spelling feeds both
/// the reserved-word map and the diagnostic rendering, so a word cannot
/// be reserved in one place and forgotten in the other; every other
/// token states how a diagnostic names it. The kind names and the
/// reserved bool literals lex to constructed tokens and stay in the
/// map beside it.
macro_rules! tokens {
    (
        keywords { $( $(#[$kd:meta])* $kw:ident = $kt:literal, )* }
        symbols { $( $(#[$sd:meta])* $sy:ident = $st:literal, )* }
        others { $( $(#[$od:meta])* $ov:ident $( ( $ot:ty ) )?, )* }
        display |$f:ident| { $( $pat:pat => $render:expr, )* }
    ) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Token {
            $( $(#[$kd])* $kw, )*
            $( $(#[$sd])* $sy, )*
            $( $(#[$od])* $ov $( ( $ot ) )?, )*
        }

        impl Token {
            /// The token a reserved word lexes to, if the word is one.
            fn keyword(word: &str) -> Option<Token> {
                match word {
                    $( $kt => Some(Token::$kw), )*
                    _ => None,
                }
            }
        }

        impl fmt::Display for Token {
            fn fmt(&self, $f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $( Token::$kw => write!($f, "`{}`", $kt), )*
                    $( Token::$sy => write!($f, "`{}`", $st), )*
                    $( $pat => $render, )*
                }
            }
        }
    };
}

tokens! {
    keywords {
        // ---- Declarations ----
        KwProgram = "program",
        KwPredicate = "predicate",
        KwIntent = "intent",
        KwInvariant = "invariant",
        KwTransformation = "transformation",
        /// Heads a derived-claim block:
        /// `derived Name(keys): Indent over <expr> value <name> = <expr>+ Dedent`.
        KwDerived = "derived",
        KwDefine = "define",
        /// The derived-claim domain expression.
        KwOver = "over",
        /// A programme-level named value, substituted away at parse time.
        KwConst = "const",

        // ---- Statements ----
        KwRequire = "require",
        /// Unique-claim lookup that extends the binding context.
        KwBind = "bind",
        KwLet = "let",
        /// Only meaningful in `let x = new Subject()`; reserved everywhere
        /// so a variable named `new` is rejected at the parser rather than
        /// silently shadowing the keyword.
        KwNew = "new",
        KwAdmit = "admit",
        KwRetract = "retract",
        KwEmit = "emit",
        KwFor = "for",

        // ---- Civil-date comparison ----
        /// Infix civil-date `<=`. Lowers to `Prop::Compare` with the
        /// `Date` domain. A distinct keyword from decimal `<=` because
        /// the comparison's domain is carried explicitly in the IR,
        /// type-checking each operand kind separately rather than
        /// overloading one operator by operand type. `before` and
        /// `after` complete the set but are matched contextually by the
        /// parser (not reserved), so they stay usable as variable names.
        KwOnOrBefore = "on_or_before",
        /// Infix civil-date `>=`; lowers to `Prop::Compare` (`Ge`, `Date`).
        KwOnOrAfter = "on_or_after",

        // ---- Boolean composition ----
        KwNot = "not",
        KwAnd = "and",
        /// Lowers to `Prop::Or`. Sits at lower precedence than `and`,
        /// higher than `implies`.
        KwOr = "or",
        /// Lowers to `Prop::Xor` (exactly-one). Sits between `and` and
        /// `or` in precedence: tighter than `or`, looser than `and`.
        KwXor = "xor",
        KwImplies = "implies",
        /// Function-call-shape primary lowering to `Prop::Pre`. The parens
        /// are mandatory. Reserved everywhere so a variable named `pre`
        /// cannot shadow the keyword.
        KwPre = "pre",

        // ---- Bounded forms, functions, and membership ----
        KwExists = "exists",
        KwForall = "forall",
        KwSum = "sum",
        /// Binary decimal minimum: `min(a, b)`.
        KwMin = "min",
        /// Binary decimal maximum: `max(a, b)`.
        KwMax = "max",
        /// Unary magnitude: `abs(x)`.
        KwAbs = "abs",
        /// `round(x, quantum)`: nearest multiple, halves away from zero.
        KwRound = "round",
        /// Claim lookup; derived-claim bodies reuse it for their
        /// `value <name> = <expr>` clauses, disambiguated by position.
        KwValue = "value",
        /// Only meaningful after `value Pred(args)`; reserved everywhere
        /// so users can't accidentally name a variable `default`.
        KwDefault = "default",
        /// Dual-purpose: structural binder in `forall x in source: body`,
        /// and membership comparator in `x in xs`. Positional
        /// disambiguation by the parser.
        KwIn = "in",
    }
    symbols {
        /// Set-builder separator in aggregators: `sum(target | body)`.
        /// Distinct from boolean composition; the pipe never separates
        /// quantifier bindings (which use `:`).
        Pipe = "|",
        /// Bare `_`. Distinct from identifiers because it means "match
        /// anything at this position", not "a name".
        Wildcard = "_",
        LParen = "(",
        RParen = ")",
        /// The unit brackets of a `Decimal[USD]` kind annotation. No
        /// other production uses them in v0.
        LBracket = "[",
        RBracket = "]",
        Colon = ":",
        Comma = ",",
        /// The rest-marker in a named-field claim pattern
        /// (`Pred(field: x, ..)`): the unmentioned fields are wildcards.
        DotDot = "..",
        Eq = "=",
        Neq = "!=",
        /// Decimal-domain comparators (bare decimals or same-unit
        /// quantities), lowering to `Prop::Compare` with the `Decimal`
        /// domain; `on_or_before` and `on_or_after` are the civil-date
        /// surface for the same operators.
        Le = "<=",
        Lt = "<",
        Ge = ">=",
        Gt = ">",
        Plus = "+",
        Minus = "-",
        Star = "*",
        Slash = "/",
        Percent = "%",
    }
    others {
        /// Kind keyword in a predicate-arg position.
        Kind(PredicateArgKind),
        /// `true` or `false`: reserved but not parseable. See the
        /// module-level note on why these are tokens rather than
        /// identifiers.
        ReservedBoolLit(bool),

        // ---- Layout virtual tokens ----
        //
        // Not produced by the character-level recogniser; the layout pass
        // in `layout.rs` inserts them at block boundaries and the parser
        // matches them to recognise block structure.
        //
        // There is no virtual `Newline` token: each statement and top-level
        // declaration starts with its own keyword, which anchors the
        // boundary, so no separator is needed. This also lets parenthesised
        // expressions span lines freely with no layout interaction.
        //
        /// Block-start marker. Inserted by the layout pass when a non-blank
        /// line begins at a greater indentation than the previous one.
        Indent,
        /// Block-end marker. Inserted by the layout pass when a non-blank
        /// line begins at a smaller indentation than the previous one; one
        /// `Dedent` per indentation level closed.
        Dedent,

        // ---- Atoms ----
        /// Identifier: any reserved-keyword-free word matching
        /// `[a-zA-Z_][a-zA-Z0-9_]*`. The parser decides by position
        /// whether it's a variable, predicate name, argument name, or
        /// transformation name.
        Ident(String),
        /// Decimal literal carried as a string to preserve exactness; the
        /// runtime parses to `rust_decimal::Decimal`, never a float.
        DecimalLit(String),
        /// Date literal: `@YYYY-MM-DD`. The `@` sigil avoids ambiguity with
        /// bare arithmetic on integer-looking tokens (`2026 - 05 - 22`).
        /// String form is the inner ISO-8601 date without the `@`; the
        /// runtime parses it via `jiff::civil::Date`. Lex validates digit
        /// and dash shape only; real-calendar validation is at runtime.
        DateLit(String),
        /// Timestamp literal: `@YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`.
        /// The same `@` sigil as dates, extended to a full RFC 3339 instant;
        /// the presence of the `T` time part is what distinguishes the two.
        /// Lex validates both the shape and, via `jiff::Timestamp`, that
        /// the instant is real - a `@2026-13-40T...` is a spanned lex
        /// diagnostic, not a runtime evaluation error. (Dates keep their
        /// validate-at-runtime precedent.) Captured without the `@`.
        TimestampLit(String),
        /// Subject literal: `#NAME`. The `#` sigil makes opaque symbolic
        /// subjects visibly distinct from variables; the inner string is
        /// the subject identifier (without the `#`). Maps to
        /// `Value::Subject(name)`.
        SubjectLit(String),
    }
    display |f| {
        Token::Kind(k) => write!(f, "kind `{k:?}`"),
        Token::ReservedBoolLit(b) => write!(f, "reserved bool literal `{b}`"),
        Token::Indent => write!(f, "indent"),
        Token::Dedent => write!(f, "dedent"),
        Token::Ident(s) => write!(f, "identifier `{s}`"),
        Token::DecimalLit(s) => write!(f, "decimal literal `{s}`"),
        Token::DateLit(s) => write!(f, "date literal `@{s}`"),
        Token::TimestampLit(s) => write!(f, "timestamp literal `@{s}`"),
        Token::SubjectLit(s) => write!(f, "subject literal `#{s}`"),
    }
}

/// Span-flavoured token alias used in the parser's input stream.
pub type SpannedToken = (Token, Span);

/// Lex a Morpholog source string into a token stream. Returns
/// either the full token stream (whitespace and comments stripped)
/// or a `Rich` error describing what could not be lexed.
///
/// Whitespace is the standard Unicode `is_whitespace` set;
/// comments are `--` to end-of-line.
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
    // The bare `_` is matched here as Token::Wildcard; `_-prefixed`
    // identifiers (e.g. `_foo`) remain identifiers.
    let ident_or_keyword = text::ascii::ident().map(|s: &str| {
        if let Some(keyword) = Token::keyword(s) {
            return keyword;
        }
        match s {
            "Subject" => Token::Kind(PredicateArgKind::Subject),
            "Decimal" => Token::Kind(PredicateArgKind::Decimal),
            "Date" => Token::Kind(PredicateArgKind::Date),
            "Timestamp" => Token::Kind(PredicateArgKind::Timestamp),
            "Duration" => Token::Kind(PredicateArgKind::Duration),
            "Bool" => Token::Kind(PredicateArgKind::Bool),
            "Collection" => Token::Kind(PredicateArgKind::Collection),
            "Any" => Token::Kind(PredicateArgKind::Any),
            // Reserved but not parseable; see the module-level note.
            "true" => Token::ReservedBoolLit(true),
            "false" => Token::ReservedBoolLit(false),
            "_" => Token::Wildcard,
            other => Token::Ident(other.to_string()),
        }
    });

    // ---- Decimal literals ----
    //
    // `<digits>` or `<digits>.<digits>`, carried as a string so the
    // runtime parses to `rust_decimal::Decimal` without routing through
    // f64. No underscore separators.
    let decimal_lit = text::digits(10)
        .then(just('.').then(text::digits(10)).or_not())
        .to_slice()
        .map(|s: &str| Token::DecimalLit(s.to_string()));

    // ---- Date literal: @YYYY-MM-DD ----
    //
    // `@` then exactly 4-2-2 digits separated by dashes, so `@2026-5-22`
    // (wrong digit count) fails at lex rather than runtime. Real-calendar
    // validation (e.g. rejecting `@2026-13-40`) happens at runtime via
    // `jiff::civil::Date`. Captured without the leading `@`.
    let digit_run = |n: usize| {
        any()
            .filter(|c: &char| c.is_ascii_digit())
            .repeated()
            .exactly(n)
    };
    // The optional RFC 3339 time part that turns a date literal into a
    // timestamp literal: `T` HH:MM:SS, optional fractional seconds,
    // then `Z` or a numeric offset. Shape-validated here; calendar and
    // range validation is `jiff::Timestamp` at parse time.
    let frac = just('.').then(
        any()
            .filter(|c: &char| c.is_ascii_digit())
            .repeated()
            .at_least(1),
    );
    let offset = choice((
        just('Z').ignored(),
        one_of("+-")
            .then(digit_run(2))
            .then(just(':'))
            .then(digit_run(2))
            .ignored(),
    ));
    let time_part = just('T')
        .then(digit_run(2))
        .then(just(':'))
        .then(digit_run(2))
        .then(just(':'))
        .then(digit_run(2))
        .then(frac.or_not())
        .then(offset);

    let date_lit = just('@')
        .ignore_then(
            digit_run(4)
                .then(just('-'))
                .then(digit_run(2))
                .then(just('-'))
                .then(digit_run(2))
                .then(time_part.or_not())
                .to_slice(),
        )
        .validate(|s: &str, e, emitter| {
            if s.contains('T') {
                // Shape is already enforced structurally; jiff confirms
                // the instant is real (no month 13, no 61st second), so
                // a bad literal is a spanned lex diagnostic rather than
                // an evaluation-time surprise - the same treatment
                // `duration(...)` gets in the parser. Dates keep their
                // validate-at-runtime precedent.
                if s.parse::<jiff::Timestamp>().is_err() {
                    emitter.emit(Rich::custom(
                        e.span(),
                        format!(
                            "invalid timestamp literal `@{s}` (expected a real RFC 3339 instant)"
                        ),
                    ));
                }
                Token::TimestampLit(s.to_string())
            } else {
                Token::DateLit(s.to_string())
            }
        });

    // ---- Subject literal: #IDENT ----
    //
    // `#` then an ASCII identifier, captured without the `#`. Maps to
    // `Value::Subject(name)`.
    let subject_lit = just('#')
        .ignore_then(text::ascii::ident())
        .map(|s: &str| Token::SubjectLit(s.to_string()));

    // ---- Operators ----
    //
    // Multi-char forms come first so `!=` matches as one token (Neq),
    // not `!` then `=`. Single `!` and single `<` are not legal; the
    // next token-attempt fails and the lex error surfaces with its span.
    let operator = choice((
        just("!=").to(Token::Neq),
        // Multi-char forms before their single-char prefixes: `<=`
        // before `<`, `>=` before `>`.
        just("<=").to(Token::Le),
        just('<').to(Token::Lt),
        just(">=").to(Token::Ge),
        just('>').to(Token::Gt),
        just('=').to(Token::Eq),
        just('+').to(Token::Plus),
        just('-').to(Token::Minus),
        just('*').to(Token::Star),
        just('/').to(Token::Slash),
        just('%').to(Token::Percent),
        just('|').to(Token::Pipe),
    ));

    let punct = choice((
        just('(').to(Token::LParen),
        just(')').to(Token::RParen),
        just('[').to(Token::LBracket),
        just(']').to(Token::RBracket),
        just(':').to(Token::Colon),
        just(',').to(Token::Comma),
        // `..` before nothing it could prefix: a lone `.` stays a lex
        // error, and a decimal's own fraction dot is consumed inside
        // `decimal_lit`, which never reaches a second consecutive dot.
        just("..").to(Token::DotDot),
    ));

    // Order matters: try the more-specific patterns (multi-char
    // operators, sigil-led literals, decimals) before the catch-all
    // ident. Sigil literals precede `operator` by convention to keep
    // the priority order obvious, though `@` and `#` are not operators.
    let token = choice((
        date_lit,
        subject_lit,
        operator,
        punct,
        decimal_lit,
        ident_or_keyword,
    ))
    .map_with(|t, e| (t, e.span()));

    // Line comments: `--` to newline or EOF (SQL/Haskell flavour). No
    // inner padding, so the outer `padding` parser is the single source
    // of truth for whitespace consumption.
    let line_comment = just("--")
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();

    let padding = choice((text::whitespace().at_least(1).ignored(), line_comment)).repeated();

    // Leading padding, then (token, trailing padding) pairs, then EOF.
    padding
        .ignore_then(token.then_ignore(padding).repeated().collect())
        .then_ignore(end())
}

/// Wraps a slice of `SpannedToken`s into a chumsky value-input stream.
/// The end-span is the byte position immediately after the last token
/// (or 0 for an empty stream).
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

#[cfg(test)]
mod tests {
    use super::{Token, lex};

    #[test]
    fn a_decimal_followed_by_the_rest_marker_lexes_as_two_tokens() {
        // The decimal's own fraction rule must rewind its consumed `.`
        // when no digits follow, leaving `..` whole for the rest-marker.
        let tokens = lex("1..").expect("lexes");
        let kinds: Vec<&Token> = tokens.iter().map(|(t, _)| t).collect();
        assert!(
            matches!(kinds.as_slice(), [Token::DecimalLit(d), Token::DotDot] if d == "1"),
            "got {kinds:?}"
        );
    }

    #[test]
    fn a_lone_dot_stays_a_lex_error() {
        assert!(lex("Foo(a, .)").is_err(), "a single `.` is not a token");
    }
}
