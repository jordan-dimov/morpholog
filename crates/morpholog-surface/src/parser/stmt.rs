//! Statement parsing (P3b1).
//!
//! Recognises the gate / binding statements that can appear inside
//! a transformation body:
//!
//! ```text
//! statement ::= require_stmt | bind_stmt | let_stmt
//! require_stmt ::= "require" expression
//! bind_stmt    ::= "bind" claim_pattern    -- restricted; see below
//! let_stmt     ::= "let" Ident "=" let_rhs
//! let_rhs      ::= "new" "Subject" "(" ")" | expression
//! claim_pattern ::= Ident "(" term_list ")"
//! ```
//!
//! The `bind` statement accepts only a claim pattern, not an
//! arbitrary expression. The IR's `Stmt::BindOne` carries an
//! `Expr` for kernel evaluation flexibility, but the meaningful
//! surface form is a single claim - the verb says "match this
//! claim and extend the binding context with the unique result".
//! `bind not Foo(x)` or `bind amount <= limit` would parse to
//! ill-shaped IR (the kernel would error at evaluation); the
//! parser rejects them at the surface, mirroring the same
//! "surface less permissive than IR" pattern used for `Neq` and
//! claim-call arg term restrictions earlier in the parser arc.
//!
//! Deferred to P3b2: `admit`, `retract`, `emit`, `for`. These add
//! state-mutating semantics and nested layout (the `for` block
//! introduces a deeper Indent). Keeping them separate lets P3b1
//! prove the layout foundation and statement sequencing first.
//! The claim-pattern helper introduced here is reused by P3b2 for
//! `admit Foo(args)` and `retract Foo(args)`.
//!
//! Statement separators are not needed: each statement begins with
//! its own keyword, so the boundary between adjacent statements is
//! "the next statement keyword". This means the body of a
//! transformation parses as `stmt+` with no explicit punctuation.
//!
//! P3b1 includes `let x = new Subject()`. It belongs here, not
//! with P3b2's state-mutating statements, because it is a
//! binding-context extension that produces a fresh subject
//! identifier - the kernel models it as `Stmt::LetNewSubject {
//! name }`, distinct from `Stmt::Assert` / `Stmt::Retract` /
//! `Stmt::Emit` which actually change admitted state. Grouping
//! it with the `let name = expr` form makes the value-binding
//! story complete in one PR.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{Expr, PredicateArgKind, Stmt, Term, Value};

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
    let decimal_lit = select! { Token::DecimalLit(s) => s };
    let date_lit = select! { Token::DateLit(s) => s };
    let subject_lit = select! { Token::SubjectLit(s) => s };

    // term ::= Ident | "_" | DecimalLit | DateLit | SubjectLit
    let term = choice((
        just(Token::Wildcard).to(Term::Wildcard),
        decimal_lit.map(|s| Term::Literal(Value::Decimal(s))),
        date_lit.map(|s| Term::Literal(Value::Date(s))),
        subject_lit.map(|s| Term::Literal(Value::Subject(s))),
        ident.map(|name| {
            if name == "actor" {
                Term::Actor
            } else {
                Term::Var(name)
            }
        }),
    ));
    let term_list = term
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<Term>>();

    // claim_pattern ::= Ident "(" term_list ")"
    //
    // Reused by P3b2 for admit / retract / emit. Parser-only:
    // does NOT consume any leading verb keyword.
    let claim_pattern = ident
        .then(term_list.delimited_by(just(Token::LParen), just(Token::RParen)))
        .map(|(predicate, args)| Expr::Claim { predicate, args });

    // require <expression>
    let require_stmt = just(Token::KwRequire)
        .ignore_then(expression_parser())
        .map(Stmt::Require);

    // bind <claim_pattern>
    //
    // Surface is restricted to a claim pattern, not an arbitrary
    // expression. See the module-level doc for rationale.
    let bind_stmt = just(Token::KwBind)
        .ignore_then(claim_pattern)
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
