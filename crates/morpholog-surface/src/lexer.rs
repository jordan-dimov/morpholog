//! Lexer: source text to `(Token, Span)` pairs, with whitespace and `--` comments stripped. The
//! span is a byte range ([`crate::diagnostics::Span`]).
//!
//! Every reserved word gets its own `Token` variant. An unknown word in a kind position lexes as
//! `Token::Ident` and the parser reports it.
//!
//! `true` and `false` are reserved but not parseable, because the IR has no boolean value to
//! lower them to. Reserving them makes `require true` a parse error; as identifiers they would
//! become an unbound variable at runtime.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::PredicateArgKind;
use std::fmt;

use crate::diagnostics::Span;

/// Declares every token once. A keyword's spelling feeds both the reserved-word lookup and
/// diagnostics, so the two cannot drift apart. Kind names and bool literals are matched in
/// `lexer` instead, because they lex to tokens carrying a value.
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
        /// Only meaningful in `let x = new Subject()`, but reserved everywhere so no variable
        /// can be named `new`.
        KwNew = "new",
        KwAdmit = "admit",
        KwRetract = "retract",
        KwEmit = "emit",
        KwFor = "for",

        // ---- Civil-date comparison ----
        /// Infix civil-date `<=`, lowering to `Prop::Compare` over `Date`. Its own word because
        /// the IR names the comparison's domain rather than inferring it from the operands.
        /// `before` and `after` are not reserved: the parser matches them by position, so they
        /// stay usable as variable names.
        KwOnOrBefore = "on_or_before",
        /// Infix civil-date `>=`; lowers to `Prop::Compare` (`Ge`, `Date`).
        KwOnOrAfter = "on_or_after",

        // ---- Boolean composition ----
        KwNot = "not",
        KwAnd = "and",
        /// Lowers to `Prop::Or`. Binds looser than `xor` and tighter than `implies`.
        KwOr = "or",
        /// Lowers to `Prop::Xor` (exactly one). Binds looser than `and`, tighter than `or`.
        KwXor = "xor",
        KwImplies = "implies",
        /// `pre(...)`, lowering to `Prop::Pre`. The parentheses are required.
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
        /// Only meaningful after `value Pred(args)`, but reserved everywhere.
        KwDefault = "default",
        /// Part of `forall x in source: body`, and membership in `x in xs`. The parser tells
        /// them apart by position.
        KwIn = "in",
    }
    symbols {
        /// The separator in `sum(target | body)`. Quantifiers use `:`, never `|`.
        Pipe = "|",
        /// Bare `_`: "match anything here", not a name.
        Wildcard = "_",
        LParen = "(",
        RParen = ")",
        /// The unit brackets of a `Decimal[USD]` kind annotation, and nothing else.
        LBracket = "[",
        RBracket = "]",
        Colon = ":",
        Comma = ",",
        /// The rest-marker in a named-field claim pattern
        /// (`Pred(field: x, ..)`): the unmentioned fields are wildcards.
        DotDot = "..",
        Eq = "=",
        Neq = "!=",
        /// Decimal comparators (bare decimals or same-unit quantities), lowering to
        /// `Prop::Compare` over `Decimal`. Dates use `on_or_before` / `on_or_after`.
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
        /// `true` or `false`: reserved but not parseable (see the module doc).
        ReservedBoolLit(bool),

        // ---- Layout tokens, inserted by `layout.rs` rather than lexed ----
        //
        // No `Newline` token is needed: every statement and declaration starts with a keyword.
        //
        /// Block start: a non-blank line indented deeper than the previous one.
        Indent,
        /// Block end: a non-blank line indented less than the previous one. One per level closed.
        Dedent,

        // ---- Atoms ----
        /// A non-reserved word matching `[a-zA-Z_][a-zA-Z0-9_]*`. Its position decides whether
        /// it names a variable, predicate, field, or transformation.
        Ident(String),
        /// Decimal literal, kept as a string so it is never rounded through a float.
        DecimalLit(String),
        /// Date literal `@YYYY-MM-DD`, stored without the `@`. The sigil keeps it apart from
        /// arithmetic like `2026 - 05 - 22`. Only the shape is checked here; whether the date
        /// exists is checked at runtime.
        DateLit(String),
        /// Timestamp literal `@YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`, stored without the
        /// `@`. The `T` part is what makes it a timestamp rather than a date. Unlike dates, an
        /// impossible instant such as `@2026-13-40T...` is a lex error.
        TimestampLit(String),
        /// Subject literal `#NAME`, stored without the `#`. The sigil sets named subjects apart
        /// from variables.
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

/// Lex a Morpholog source string into tokens, dropping whitespace and `--` line comments, or
/// return `Rich` errors for what could not be lexed.
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
    // A bare `_` is the wildcard; `_foo` is an identifier.
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
    // `<digits>` or `<digits>.<digits>`. No underscore separators.
    let decimal_lit = text::digits(10)
        .then(just('.').then(text::digits(10)).or_not())
        .to_slice()
        .map(|s: &str| Token::DecimalLit(s.to_string()));

    // ---- Date literal: @YYYY-MM-DD ----
    //
    // Exactly 4-2-2 digits, so `@2026-5-22` fails here rather than at runtime.
    let digit_run = |n: usize| {
        any()
            .filter(|c: &char| c.is_ascii_digit())
            .repeated()
            .exactly(n)
    };
    // The optional time part that turns a date literal into a timestamp.
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
                // Reject impossible instants here, with a span, rather than at evaluation.
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
    let subject_lit = just('#')
        .ignore_then(text::ascii::ident())
        .map(|s: &str| Token::SubjectLit(s.to_string()));

    // ---- Operators ----
    //
    // Multi-char forms come before their one-char prefixes. A lone `!` is a lex error.
    let operator = choice((
        just("!=").to(Token::Neq),
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
        // A lone `.` stays a lex error; a decimal's own dot is consumed by `decimal_lit`.
        just("..").to(Token::DotDot),
    ));

    // More specific patterns first; identifiers are the catch-all.
    let token = choice((
        date_lit,
        subject_lit,
        operator,
        punct,
        decimal_lit,
        ident_or_keyword,
    ))
    .map_with(|t, e| (t, e.span()));

    // Line comments: `--` to end of line. Whitespace is left to `padding`.
    let line_comment = just("--")
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();

    let padding = choice((text::whitespace().at_least(1).ignored(), line_comment)).repeated();

    // Leading padding, then (token, trailing padding) pairs, then EOF.
    padding
        .ignore_then(token.then_ignore(padding).repeated().collect())
        .then_ignore(end())
}

/// Wraps tokens as a chumsky input stream. The end-of-input span sits just after the last token,
/// or at 0 when there are none.
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
