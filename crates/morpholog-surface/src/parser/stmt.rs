//! Statement parsing.
//!
//! Recognises every statement form a transformation body can hold:
//!
//! ```text
//! statement ::= require_stmt | bind_stmt | let_stmt
//!             | admit_stmt | retract_stmt | emit_stmt
//!             | for_stmt
//! require_stmt  ::= "require" proposition
//! bind_stmt     ::= "bind" claim_pattern    -- restricted; see below
//! let_stmt      ::= "let" Ident "=" let_rhs
//! let_rhs       ::= "new" "Subject" "(" ")" | value_expression
//! admit_stmt    ::= "admit" claim_pattern
//! retract_stmt  ::= "retract" claim_pattern
//! emit_stmt     ::= "emit" claim_pattern    -- Intent shares the shape
//! for_stmt      ::= "for" Ident "in" value_expression ":" Indent statement+ Dedent
//! claim_pattern ::= Ident "(" term_list ")"
//! ```
//!
//! The claim-shaped verbs share one pattern and differ only in the IR they build:
//!
//! - `bind Foo(args)`    -> `Stmt::BindOne(Prop::Claim { .. })`
//! - `admit Foo(args)`   -> `Stmt::Assert(Claim { predicate, args })`
//! - `retract Foo(args)` -> `Stmt::Retract { predicate, args }`
//! - `emit Foo(args)`    -> `Stmt::Emit(Intent { name, args })`
//!
//! `bind` accepts only a claim pattern although the IR's `Stmt::BindOne` could hold any `Prop`:
//! the surface allows only what authors meaningfully write.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{Claim, Intent, PredicateArgKind, Prop, Stmt, Term, ValueExpr};

use crate::lexer::Token;

use super::expr::{
    PatternArgs, expression_parser, pattern_args_parser, resolve_pattern, value_expr_parser,
};
use super::field_table::{FieldTable, Vocabulary};

/// Build a parser for a single statement. Recursive, so a `for` body can hold any statement,
/// including another `for`.
pub(super) fn statement_parser<'a, I>(
    table: &'a FieldTable,
) -> impl Parser<'a, I, Stmt, extra::Err<Rich<'a, Token>>>
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let proposition = expression_parser(table);
    let value_expr = value_expr_parser(table);

    recursive(move |statement| {
        let ident = select! { Token::Ident(s) => s };

        // claim_pattern ::= Ident "(" pattern_args ")"
        //
        // Each verb resolves the pattern against its own vocabulary.
        let claim_pattern = ident
            .then(pattern_args_parser().delimited_by(just(Token::LParen), just(Token::RParen)));

        // An optional `<name>:` prefix. No proposition starts with `Ident :`, so it is unambiguous.
        let rule_name = ident.then_ignore(just(Token::Colon)).or_not();

        // require [<name>:] <proposition>
        let require_stmt = just(Token::KwRequire)
            .ignore_then(rule_name.clone())
            .then(proposition.clone())
            .map(|(name, prop)| Stmt::Require {
                prop,
                name: name.map(Into::into),
            });

        // bind [<name>:] <claim_pattern>
        let bind_stmt = just(Token::KwBind)
            .ignore_then(rule_name)
            .then(claim_pattern.clone())
            .validate(move |(name, (predicate, args)), e, emitter| {
                let args = resolve_pattern(
                    &predicate,
                    args,
                    Vocabulary::ClaimShaped,
                    table,
                    e.span(),
                    &mut |span, message| emitter.emit(Rich::custom(span, message)),
                );
                Stmt::BindOne {
                    prop: Prop::Claim {
                        predicate: predicate.into(),
                        args,
                    },
                    name: name.map(Into::into),
                }
            });

        // admit <claim_pattern>
        //
        // Wildcards are refused here, with a span, since the kernel would refuse them anyway.
        let admit_stmt = just(Token::KwAdmit)
            .ignore_then(claim_pattern.clone())
            .validate(move |(predicate, args), e, emitter| {
                let span: SimpleSpan = e.span();
                // `..` leaves fields unfilled, the same mistake as a wildcard. Refuse it by name
                // and skip the wildcard check, so one mistake gives one diagnostic.
                let rest_refused = matches!(&args, PatternArgs::Named { rest: true, .. });
                if rest_refused {
                    emitter.emit(Rich::custom(
                        span,
                        "`..` is not allowed in `admit`: admitting a claim supplies every \
                         field, so a named pattern here names them all",
                    ));
                }
                let args = resolve_pattern(
                    &predicate,
                    args,
                    Vocabulary::PredicateOnly,
                    table,
                    span,
                    &mut |span, message| emitter.emit(Rich::custom(span, message)),
                );
                if !rest_refused && args.iter().any(|t| matches!(t, Term::Wildcard)) {
                    emitter.emit(Rich::custom(
                        span,
                        "wildcard `_` is not allowed in `admit`: admitting a claim requires every argument to be concrete; the kernel rejects wildcard-admits as `wildcard not allowed in assert`",
                    ));
                }
                Stmt::Assert(Claim {
                    predicate: predicate.into(),
                    args,
                })
            });

        // retract <claim_pattern>
        //
        // Wildcards are allowed: `retract Foo(x, _)` retracts every `Foo` whose first arg is `x`.
        let retract_stmt = just(Token::KwRetract)
            .ignore_then(claim_pattern.clone())
            .validate(move |(predicate, args), e, emitter| {
                let args = resolve_pattern(
                    &predicate,
                    args,
                    Vocabulary::PredicateOnly,
                    table,
                    e.span(),
                    &mut |span, message| emitter.emit(Rich::custom(span, message)),
                );
                Stmt::Retract {
                    predicate: predicate.into(),
                    args,
                }
            });

        // emit <claim_pattern>
        //
        // Intents look like claims but are not. Wildcards and `..` are refused as in `admit`.
        let emit_stmt = just(Token::KwEmit)
            .ignore_then(claim_pattern.clone())
            .validate(move |(name, args), e, emitter| {
                let span: SimpleSpan = e.span();
                let rest_refused = matches!(&args, PatternArgs::Named { rest: true, .. });
                if rest_refused {
                    emitter.emit(Rich::custom(
                        span,
                        "`..` is not allowed in `emit`: an intent's arguments are all \
                         supplied, so a named pattern here names every field",
                    ));
                }
                let args = resolve_pattern(
                    &name,
                    args,
                    Vocabulary::Intent,
                    table,
                    span,
                    &mut |span, message| emitter.emit(Rich::custom(span, message)),
                );
                if !rest_refused && args.iter().any(|t| matches!(t, Term::Wildcard)) {
                    emitter.emit(Rich::custom(
                        span,
                        "wildcard `_` is not allowed in `emit`: an intent's arguments must all be concrete values; the kernel rejects wildcard-emits as `wildcard not allowed in emit`",
                    ));
                }
                Stmt::Emit(Intent {
                    name: name.into(),
                    args,
                })
            });

        // let <name> = <rhs>
        //
        // The right-hand side is `new Subject()` or a value expression. `Subject` lexes as a
        // kind keyword, not an identifier.
        let new_subject_rhs = just(Token::KwNew)
            .ignore_then(select! { Token::Kind(PredicateArgKind::Subject) => () })
            .then_ignore(just(Token::LParen))
            .then_ignore(just(Token::RParen));

        let let_rhs = choice((
            new_subject_rhs.map(|()| LetRhs::NewSubject),
            value_expr.clone().map(LetRhs::Value),
        ));

        let let_stmt = just(Token::KwLet)
            .ignore_then(ident)
            .then_ignore(just(Token::Eq))
            .then(let_rhs)
            .map(|(name, rhs)| match rhs {
                LetRhs::NewSubject => Stmt::LetNewSubject { name: name.into() },
                LetRhs::Value(value) => Stmt::Let {
                    name: name.into(),
                    value,
                },
            });

        // for <name> in <value-expression> : Indent statement+ Dedent
        //
        // Any value expression parses as the collection; the runtime checks it is one.
        let for_stmt = just(Token::KwFor)
            .ignore_then(ident)
            .then_ignore(just(Token::KwIn))
            .then(value_expr.clone())
            .then_ignore(just(Token::Colon))
            .then_ignore(just(Token::Indent))
            .then(
                statement
                    .clone()
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<Stmt>>(),
            )
            .then_ignore(just(Token::Dedent))
            .map(|((binding, collection), body)| Stmt::For {
                binding: binding.into(),
                collection,
                body,
            });

        choice((
            require_stmt,
            bind_stmt,
            admit_stmt,
            retract_stmt,
            emit_stmt,
            let_stmt,
            for_stmt,
        ))
    })
}

/// The two right-hand sides a `let` can have.
enum LetRhs {
    NewSubject,
    Value(ValueExpr),
}
