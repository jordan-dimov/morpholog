//! Statement parsing (P3b1).
//!
//! Recognises the gate / binding statements that can appear inside
//! a transformation body:
//!
//! ```text
//! statement ::= require_stmt | bind_stmt | let_stmt
//! require_stmt ::= "require" expression
//! bind_stmt    ::= "bind" expression       -- must be a claim shape; runtime validates
//! let_stmt     ::= "let" Ident "=" let_rhs
//! let_rhs      ::= "new" "Subject" "(" ")" | expression
//! ```
//!
//! Deferred to P3b2: `admit`, `retract`, `emit`, `for`. These add
//! state-mutating semantics and nested layout (the `for` block
//! introduces a deeper Indent). Keeping them separate lets P3b1
//! prove the layout foundation and statement sequencing first.
//!
//! Statement separators are not needed: each statement begins with
//! its own keyword, so the boundary between adjacent statements is
//! "the next statement keyword". This means the body of a
//! transformation parses as `stmt+` with no explicit punctuation.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{PredicateArgKind, Stmt};

use crate::lexer::Token;

use super::expr::expression_parser;

/// Build a parser for a single statement.
///
/// Returns a parser that consumes one `Stmt` and stops at the
/// boundary token (typically `Dedent` or another statement
/// keyword). Used inside transformation bodies; not exposed to
/// crate consumers directly.
pub(super) fn statement_parser<'a, I>() -> impl Parser<'a, I, Stmt, extra::Err<Rich<'a, Token>>>
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };

    // require <expression>
    let require_stmt = just(Token::KwRequire)
        .ignore_then(expression_parser())
        .map(Stmt::Require);

    // bind <expression>
    //
    // The IR's Stmt::BindOne takes any Expr; in practice the
    // expression must be predicate-shaped (a claim call) for the
    // runtime to extract bindings. The parser accepts any expression
    // and trusts validation / runtime to catch non-claim shapes -
    // mirrors how P2a's `Neq` term-restriction was handled.
    let bind_stmt = just(Token::KwBind)
        .ignore_then(expression_parser())
        .map(Stmt::BindOne);

    // let <name> = <rhs>
    //
    // rhs has two forms:
    //   1. `new Subject ( )`  -> Stmt::LetNewSubject { name }
    //   2. <expression>        -> Stmt::Let { name, value }
    //
    // The `new Subject()` form is recognised by the specific token
    // sequence `KwNew Kind(Subject) LParen RParen`. `Kind(Subject)`
    // is what the lexer produces for the bare identifier `Subject`
    // (kind keywords are lexer-reserved). The parser checks the
    // discriminant via `select!`.
    let new_subject_rhs = just(Token::KwNew)
        .ignore_then(select! { Token::Kind(PredicateArgKind::Subject) => () })
        .then_ignore(just(Token::LParen))
        .then_ignore(just(Token::RParen));

    let let_rhs = choice((
        new_subject_rhs.map(|()| LetRhs::NewSubject),
        expression_parser().map(LetRhs::Expr),
    ));

    let let_stmt = just(Token::KwLet)
        .ignore_then(ident)
        .then_ignore(just(Token::Eq))
        .then(let_rhs)
        .map(|(name, rhs)| match rhs {
            LetRhs::NewSubject => Stmt::LetNewSubject { name },
            LetRhs::Expr(value) => Stmt::Let { name, value },
        });

    choice((require_stmt, bind_stmt, let_stmt))
}

/// Discriminator for the two `let` RHS forms. Internal to the
/// parser; the surface keywords (`new Subject ( )` vs an
/// expression) drive the choice.
enum LetRhs {
    NewSubject,
    Expr(morpholog_core::Expr),
}
