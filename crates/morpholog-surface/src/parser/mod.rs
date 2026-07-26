//! Parser for the v0 surface.
//!
//! The grammar is documented where each tier is implemented: the
//! declaration grammar (programme header, predicates with discipline
//! clauses, intents, definitions, invariants, transformations,
//! derived claims) in [`program`]'s comments, the statement grammar
//! in [`stmt`]'s module doc, and the expression productions beside
//! their combinators in [`expr`]. The canonical surface-to-IR table
//! (every operator, comparator, and quantifier form, with the
//! reason it is spelled the way it is) lives in
//! `docs/runtime-semantics.md`; this header does not duplicate it -
//! an earlier copy here rotted as the grammar grew.
//!
//! Newlines are insignificant. Trailing commas in argument lists
//! are allowed. Comments are stripped at the lexer; the parser
//! never sees them.
//!
//! Asymmetry to honour: `Prop::In(Term, Term)` operates on *terms*,
//! not expressions, as do claim-call arguments. `Eq`, `Neq`, and the
//! comparators relate two value expressions; the arithmetic
//! operators compose value expressions. The
//! parser must therefore reject `a + 1 in xs` (membership is
//! term-only) and `Foo(x + 1, y)` (claim arguments are terms), while
//! `Foo + 1 != Bar` is accepted - `!=` is symmetric with `=`. These
//! constraints follow directly from the IR shape under the
//! `docs/scope-and-ambition.md` surface doctrine.
//!
//! Disambiguation rules govern the bounded forms:
//!
//! - The `in` keyword is structural inside `forall <ident> in
//!   <source>:` (consumed by the forall production before
//!   reaching comparator-level grammar) and a membership
//!   comparator everywhere else. Positional disambiguation; no
//!   context-sensitive parsing.
//! - `forall x in source: body` accepts the source in a restricted
//!   grammar (bare variable, claim call, or parenthesised
//!   expression) - per the doctrine, value-shaped primaries
//!   (literals, wildcards, `sum`, `value`) cannot be `forall`
//!   sources. When the source is a bare Term-wrapper, the parser
//!   auto-lifts it to `Prop::In(Var(binding), source_term)`.
//!
//! Module layout: this parser file is split by concern. The
//! programme-level parser (`parse_program`, `program_parser`,
//! `RawProgram`, `TopLevelDecl`) lives in [`program`]; the
//! expression-level parsers (`parse_expression`/`parse_value_expr`,
//! `expression_parser`/`value_expr_parser`, `CmpOp`, `value_as_term`)
//! live in [`expr`]. The programme
//! parser uses the expression parser for invariant bodies via
//! the `pub(super)` re-export from `expr`.
//!
//! Error recovery: humble. On a parse failure inside a top-level
//! declaration, the parser skips forward to the next top-level
//! declaration keyword (or EOF) and continues. The intent is
//! "tell the author about every malformed declaration in one
//! parse run", not a full-language error-recovery framework.
//! Expression parsing does not yet add recovery shapes; a
//! malformed expression surfaces as one diagnostic at the failure
//! site.

mod consts;
mod expr;
mod lets;
mod program;
mod stmt;

pub use expr::{parse_expression, parse_value_expr};
pub use program::{parse_program, parse_program_with_sources};

use crate::diagnostics::Diagnostic;
use crate::lexer::Token;
use chumsky::input::ValueInput;
use chumsky::prelude::*;

/// A body in the inline-or-indented shape shared by invariants,
/// definitions, quantifiers, and discipline clauses: the layout pass
/// emits `Indent`/`Dedent` around an indented body; the inline form
/// has no layout tokens.
fn indented_or_inline<'a, I, O>(
    body: impl Parser<'a, I, O, extra::Err<Rich<'a, Token>>> + Clone,
) -> impl Parser<'a, I, O, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    choice((
        just(Token::Indent)
            .ignore_then(body.clone())
            .then_ignore(just(Token::Dedent)),
        body,
    ))
}

/// Map lexer failures to diagnostics. Shared by every parse entry
/// point, so the "lex error:" prefix has one home.
fn lex_error_diagnostics(errs: Vec<Rich<'_, char>>) -> Vec<Diagnostic> {
    errs.into_iter()
        .map(|e| Diagnostic::error(format!("lex error: {}", e.reason()), e.span().into_range()))
        .collect()
}

/// Map parser failures to diagnostics - the "parse error:" twin of
/// [`lex_error_diagnostics`]. A stray `Indent` gets the layout rule
/// spelled out: "found 'indent'" names the mechanism, not the fix, and
/// the fix (same column, or parens) is not guessable from the message.
fn parse_error_diagnostics(errs: Vec<Rich<'_, Token>>) -> Vec<Diagnostic> {
    errs.into_iter()
        .map(|e| {
            let span = e.span();
            let mut message = format!("parse error: {}", e.reason());
            if matches!(e.found(), Some(Token::Indent)) {
                message.push_str(
                    "; continuation lines of one expression stay at the same column - \
                     indent no deeper, or wrap the expression in parentheses, which \
                     make layout stop mattering until they close",
                );
            }
            Diagnostic::error(message, span.start()..span.end())
        })
        .collect()
}
