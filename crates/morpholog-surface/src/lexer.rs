//! Lexer for the v0 surface fragment.
//!
//! Recognised tokens:
//!
//! - Top-level keywords: `program`, `predicate`, `intent`.
//! - Kind keywords (lexer-level): `Subject`, `Decimal`, `Date`,
//!   `Bool`, `Collection`, `Any`.
//! - Boolean keywords: `not`, `and`, `or`, `implies`, `pre`.
//! - Identifiers: `[a-zA-Z][a-zA-Z0-9_]*` and `_<rest>` for
//!   `_-prefixed` names. The bare `_` is the wildcard token, not
//!   an identifier.
//! - Decimal literals: `<digits>` or `<digits>.<digits>`.
//!   String-valued because the runtime stores decimals as
//!   `rust_decimal::Decimal` parsed from strings, never as floats.
//! - Punctuation: `(`, `)`, `:`, `,`.
//! - Comparators: `=`, `!=`, `<=`. Multi-char forms must
//!   be tried before single-char.
//! - Arithmetic: `+`, `-`.
//! - Wildcard: `_`.
//!
//! `true` and `false` are lexer-reserved but parser-rejected
//! in v0. The IR's `Value` enum has variants for `Decimal`,
//! `Subject`, and `Date` only - no `Value::Bool`. The runtime
//! `EvalValue::Bool` is produced by comparators and other
//! expressions; it never appears as an IR literal. Per the
//! surface doctrine in `docs/scope-and-ambition.md`, the surface
//! cannot create capabilities the kernel lacks: a `true` literal
//! has nowhere to lower to today. Treating `true` as an ordinary
//! identifier would silently lower to `Term::Var("true")` and
//! fail at runtime as `UnboundVariable`; reserving it at the
//! lexer (as `Token::ReservedBoolLit`) lets the parser reject
//! it with an "unexpected token" diagnostic instead. It lifts
//! to a proper bool-literal token the moment a worked example
//! forces `Value::Bool` into the IR.
//!
//! Whitespace and `--` line comments are stripped at lex; they
//! never reach the parser. Output is a vector of `(Token, Span)`
//! pairs - span is a byte-offset range into the source, compatible
//! with [`crate::diagnostics::Span`] and `ariadne`.
//!
//! Reserved words include the structural keywords
//! (`program`, `predicate`, `intent`), the kind names (`Subject`,
//! `Decimal`, `Date`, `Bool`, `Collection`, `Any`), the boolean
//! operators (`not`, `and`, `or`, `implies`, `pre`), and the placeholder bool
//! literals (`true`, `false`, lexed but not parseable per the
//! note above). The lexer maps each to a specific `Token::*`
//! variant so the parser can match against them directly. An
//! identifier in kind-position that doesn't match a reserved
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
    /// `intent` keyword (intent-declaration surface).
    KwIntent,
    /// Kind keyword in a predicate-arg position.
    Kind(PredicateArgKind),

    // ---- Invariant declarations ----
    /// `invariant` keyword.
    KwInvariant,

    // ---- Transformations + gate statements ----
    /// `transformation` keyword.
    KwTransformation,
    /// `require` statement keyword.
    KwRequire,
    /// `bind` statement keyword (unique-claim lookup that extends
    /// the binding context).
    KwBind,
    /// `let` statement keyword (value binding).
    KwLet,
    /// `new` keyword (only meaningful in `let x = new Subject()`,
    /// reserved at the lexer everywhere so a variable named
    /// `new` is rejected at the parser rather than silently
    /// shadowing the keyword's future meaning).
    KwNew,

    // ---- Planned: state-mutating statements + iteration ----
    //
    // Reserved at the lexer now so a user who writes `admit
    // Foo(...)` gets a clean "unexpected token `admit`"
    // diagnostic, not a silent `Var("admit")` interpretation. The
    // statement parser rejects all four; they become productions
    // when the planned support lands.
    /// `admit` statement keyword (planned).
    KwAdmit,
    /// `retract` statement keyword (planned).
    KwRetract,
    /// `emit` statement keyword (planned).
    KwEmit,
    /// `for` block keyword (planned).
    KwFor,

    // ---- Derived claims ----
    /// `derived` declaration keyword. Heads a derived-claim block:
    /// `derived Name(keys): Indent over <expr> value <name> = <expr>+ Dedent`.
    KwDerived,
    /// `over` keyword for the derived-claim domain expression.
    KwOver,
    // Note: derived-claim bodies use `value <name> = <expr>` clauses;
    // the parser reuses the existing `KwValue` token for that form
    // (same keyword as the expression-level `value Pred(args)`; the
    // parser disambiguates by position).

    // ---- Civil-date comparison ----
    /// `on_or_before` infix operator for civil-date `<=`. Lowers
    /// to `Expr::DateLe`. Distinct from decimal `<=` because the
    /// kernel keeps `Expr::Le` (decimal) and `Expr::DateLe`
    /// (civil date) as separate IR primitives; surface refuses
    /// to overload by operand type (see `design-history.md` -
    /// the `DateLe` entry explicitly rejected dispatch-on-kind).
    KwOnOrBefore,

    // ---- Layout virtual tokens ----
    //
    // These are NOT produced by the lexer's character-level
    // recogniser; the layout normalisation pass in `layout.rs`
    // inserts them at block boundaries. The parser matches them
    // to recognise block structure. Lex outputs include physical
    // tokens only; the layout pass enriches the stream.
    //
    // No virtual `Newline` token. Each statement and top-level
    // declaration starts with its own keyword (`require`, `bind`,
    // `let`, `predicate`, `invariant`, `transformation`, etc.); the
    // keyword anchors the boundary, so no separator token is
    // needed. This also lets parenthesised expressions span lines
    // freely with no layout interaction.
    //
    /// Block-start marker. Inserted by the layout pass when a
    /// non-blank line begins at a greater indentation than the
    /// previous non-blank line.
    Indent,
    /// Block-end marker. Inserted by the layout pass when a
    /// non-blank line begins at a smaller indentation than the
    /// previous non-blank line; one `Dedent` per indentation
    /// level closed.
    Dedent,

    // ---- Boolean composition ----
    /// `not` prefix operator.
    KwNot,
    /// `and` infix operator.
    KwAnd,
    /// `or` infix operator. Lowers to `Expr::Or`. Sits at lower
    /// precedence than `and` and higher than `implies` (standard
    /// logical-operator precedence).
    KwOr,
    /// `implies` infix operator.
    KwImplies,
    /// `pre` function-call-shape primary. Lowers to `Expr::Pre`.
    /// Always followed by `(`; the parens are mandatory. Reserved
    /// at the lexer everywhere so a variable named `pre` cannot
    /// shadow the keyword.
    KwPre,

    /// `true` or `false`: lexer-reserved but not parseable in
    /// v0. Reserved at the lexer level (rather than left as a
    /// plain identifier) so that users who write `require true`
    /// expecting bool-literal semantics get an "unexpected token
    /// `true`" parse error rather than a confusing
    /// `UnboundVariable("true")` at runtime. Lifts to a proper
    /// bool-literal token when a worked example forces
    /// `Value::Bool` into the IR.
    ReservedBoolLit(bool),

    // ---- Bounded forms + membership ----
    /// `exists` quantifier keyword.
    KwExists,
    /// `forall` quantifier keyword.
    KwForall,
    /// `sum` aggregator keyword.
    KwSum,
    /// `value` claim-lookup keyword.
    KwValue,
    /// `default` keyword (only meaningful after `value Pred(args)`
    /// in v0; reserved at the lexer everywhere so users can't
    /// accidentally name a variable `default`).
    KwDefault,
    /// `in` keyword. Dual-purpose: structural binder in
    /// `forall x in source: body`, and membership comparator
    /// in `x in xs`. Positional disambiguation by the parser.
    KwIn,
    /// `|` (pipe). Set-builder separator in aggregators:
    /// `sum(target | body)`. Distinct from boolean composition
    /// in the surface grammar - the pipe never separates
    /// quantifier bindings (which use `:`).
    Pipe,

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
    /// Date literal: `@YYYY-MM-DD`. The `@` sigil avoids
    /// ambiguity with bare arithmetic on integer-looking tokens
    /// (`2026 - 05 - 22`). String form is the inner ISO-8601
    /// date without the leading `@`; the runtime parses it via
    /// `jiff::civil::Date`. Lex-level format validation: digits
    /// and dashes only; semantic validation (real calendar
    /// dates) happens at runtime.
    DateLit(String),
    /// Subject literal: `#NAME`. The `#` sigil makes opaque
    /// symbolic subjects visibly distinct from variables; the
    /// inner string is the subject's identifier (without the
    /// `#`). Maps to `Value::Subject(name)`.
    SubjectLit(String),

    // ---- Punctuation ----
    LParen,
    RParen,
    Colon,
    Comma,

    // ---- Operators ----
    /// `=` (Eq).
    Eq,
    /// `!=` (Neq).
    Neq,
    /// `<=` lowers to Expr::Le for decimal operands (decimal-only;
    /// on_or_before is the surface for Expr::DateLe on civil dates).
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
            Token::KwIntent => write!(f, "`intent`"),
            Token::Kind(k) => write!(f, "kind `{:?}`", k),
            Token::KwInvariant => write!(f, "`invariant`"),
            Token::KwTransformation => write!(f, "`transformation`"),
            Token::KwRequire => write!(f, "`require`"),
            Token::KwBind => write!(f, "`bind`"),
            Token::KwLet => write!(f, "`let`"),
            Token::KwNew => write!(f, "`new`"),
            Token::KwAdmit => write!(f, "`admit`"),
            Token::KwRetract => write!(f, "`retract`"),
            Token::KwEmit => write!(f, "`emit`"),
            Token::KwFor => write!(f, "`for`"),
            Token::KwDerived => write!(f, "`derived`"),
            Token::KwOver => write!(f, "`over`"),
            Token::KwOnOrBefore => write!(f, "`on_or_before`"),
            Token::Indent => write!(f, "indent"),
            Token::Dedent => write!(f, "dedent"),
            Token::KwNot => write!(f, "`not`"),
            Token::KwAnd => write!(f, "`and`"),
            Token::KwOr => write!(f, "`or`"),
            Token::KwImplies => write!(f, "`implies`"),
            Token::KwPre => write!(f, "`pre`"),
            Token::ReservedBoolLit(b) => write!(f, "reserved bool literal `{b}`"),
            Token::KwExists => write!(f, "`exists`"),
            Token::KwForall => write!(f, "`forall`"),
            Token::KwSum => write!(f, "`sum`"),
            Token::KwValue => write!(f, "`value`"),
            Token::KwDefault => write!(f, "`default`"),
            Token::KwIn => write!(f, "`in`"),
            Token::Pipe => write!(f, "`|`"),
            Token::DateLit(s) => write!(f, "date literal `@{s}`"),
            Token::SubjectLit(s) => write!(f, "subject literal `#{s}`"),
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
    // The bare `_` is matched separately (as Token::Wildcard);
    // `_-prefixed` identifiers (e.g. `_foo`) remain identifiers.
    // We do this via the order of `choice` below: punctuation /
    // wildcard / decimal / ident, where ident is the catch-all.
    let ident_or_keyword = text::ascii::ident().map(|s: &str| match s {
        // P1 reserved words
        "program" => Token::KwProgram,
        "predicate" => Token::KwPredicate,
        "intent" => Token::KwIntent,
        "invariant" => Token::KwInvariant,
        "transformation" => Token::KwTransformation,
        "require" => Token::KwRequire,
        "bind" => Token::KwBind,
        "let" => Token::KwLet,
        "new" => Token::KwNew,
        // Planned-statement keywords - reserved at the lexer but not yet
        // parseable. The parser rejects them with an unexpected-
        // token diagnostic, which is honest about the v0 limit.
        "admit" => Token::KwAdmit,
        "retract" => Token::KwRetract,
        "emit" => Token::KwEmit,
        "for" => Token::KwFor,
        "derived" => Token::KwDerived,
        "over" => Token::KwOver,
        // Civil-date <= comparator
        "on_or_before" => Token::KwOnOrBefore,
        "Subject" => Token::Kind(PredicateArgKind::Subject),
        "Decimal" => Token::Kind(PredicateArgKind::Decimal),
        "Date" => Token::Kind(PredicateArgKind::Date),
        "Bool" => Token::Kind(PredicateArgKind::Bool),
        "Collection" => Token::Kind(PredicateArgKind::Collection),
        "Any" => Token::Kind(PredicateArgKind::Any),
        // Operator and boolean keywords
        "not" => Token::KwNot,
        "and" => Token::KwAnd,
        "or" => Token::KwOr,
        "implies" => Token::KwImplies,
        "pre" => Token::KwPre,
        // Bounded forms and membership keywords
        "exists" => Token::KwExists,
        "forall" => Token::KwForall,
        "sum" => Token::KwSum,
        "value" => Token::KwValue,
        "default" => Token::KwDefault,
        "in" => Token::KwIn,
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

    // ---- Date literal: @YYYY-MM-DD ----
    //
    // Lex shape: `@` followed by exactly 4 digits, dash, exactly
    // 2 digits, dash, exactly 2 digits. Authors get immediate
    // lexer feedback for `@2026-5-22` (wrong digit count) rather
    // than discovering it at runtime. Semantic validation (real
    // calendar dates - e.g. `@2026-13-40` is not a valid date)
    // happens at runtime via `jiff::civil::Date`. The body of
    // the literal is captured without the leading `@`.
    let digit_run = |n: usize| {
        any()
            .filter(|c: &char| c.is_ascii_digit())
            .repeated()
            .exactly(n)
    };
    let date_lit = just('@')
        .ignore_then(
            digit_run(4)
                .then(just('-'))
                .then(digit_run(2))
                .then(just('-'))
                .then(digit_run(2))
                .to_slice(),
        )
        .map(|s: &str| Token::DateLit(s.to_string()));

    // ---- Subject literal: #IDENT ----
    //
    // Lex shape: `#` followed by an ASCII identifier. Captured
    // without the leading `#`. Maps to `Value::Subject(name)`.
    let subject_lit = just('#')
        .ignore_then(text::ascii::ident())
        .map(|s: &str| Token::SubjectLit(s.to_string()));

    // ---- Operators ----
    //
    // Multi-char forms come first in the choice so `!=` is matched
    // as one token (Neq), not `!` followed by `=`. Single `!` and
    // single `<` is not legal; if they appear, the next
    // token-attempt fails and the lex error surfaces with their
    // span.
    let operator = choice((
        just("!=").to(Token::Neq),
        just("<=").to(Token::Le),
        just('=').to(Token::Eq),
        just('+').to(Token::Plus),
        just('-').to(Token::Minus),
        just('|').to(Token::Pipe),
    ));

    let punct = choice((
        just('(').to(Token::LParen),
        just(')').to(Token::RParen),
        just(':').to(Token::Colon),
        just(',').to(Token::Comma),
    ));

    // Order matters: try the more-specific patterns (multi-char
    // operators, sigil-led literals, decimals) before the catch-all
    // ident. The sigil literals must precede `operator` because
    // operators include single-char punctuation that could clash
    // with sigil start chars in some grammars - here they don't
    // (`@` and `#` aren't operators), but the convention keeps the
    // priority order obvious.
    let token = choice((
        date_lit,
        subject_lit,
        operator,
        punct,
        decimal_lit,
        ident_or_keyword,
    ))
    .map_with(|t, e| (t, e.span()));

    // Line comments: `--` to newline (or EOF). SQL/Haskell flavour
    // chosen to match Morpholog's database-flavoured audience and
    // the historical example files. Skipped entirely; no inner
    // padding so the outer `padding` parser is the single source
    // of truth for whitespace consumption.
    let line_comment = just("--")
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
