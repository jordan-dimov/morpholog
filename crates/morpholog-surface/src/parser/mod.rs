//! Parser for the `.morph` surface.
//!
//! Each tier documents its own grammar: declarations in [`program`], statements in [`stmt`],
//! expressions beside their combinators in [`expr`]. The full surface-to-IR table lives in
//! `docs/runtime-semantics.md` and is not repeated here.
//!
//! Newlines carry no tokens (the layout pass handles indentation), trailing commas are allowed,
//! and the lexer strips comments.
//!
//! Claim-call arguments and both sides of `in` are terms, not expressions, because the IR says
//! so. So `a + 1 in xs` and `Foo(x + 1, y)` are rejected, while `Foo + 1 != Bar` is fine: `=`,
//! `!=` and the comparators relate value expressions.
//!
//! `in` is part of the `forall x in source:` syntax and a membership test everywhere else; the
//! position decides, not context. A `forall` source is a bare variable, a claim call, or a
//! parenthesised expression; a bare variable is lifted to `Prop::In(Var(binding), source)`.
//!
//! Error recovery is minimal: a malformed top-level declaration is skipped up to the next
//! declaration keyword, so one run reports every broken declaration. A malformed expression
//! gives one diagnostic at the failure site.

mod consts;
mod expr;
mod field_table;
mod lets;
mod program;
mod stmt;
mod walk;

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

/// Map parser failures to diagnostics, like [`lex_error_diagnostics`]. A stray `Indent` also
/// gets the fix spelled out, since "found 'indent'" alone does not suggest it.
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
