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
//! Surface verb / IR mapping (the predicate + args pair is the
//! same shape across four verbs; the verb decides the wrapper):
//!
//! - `bind Foo(args)`    -> `Stmt::BindOne(Prop::Claim { .. })`
//! - `admit Foo(args)`   -> `Stmt::Assert(Claim { predicate, args })`
//! - `retract Foo(args)` -> `Stmt::Retract { predicate, args }`
//! - `emit Foo(args)`    -> `Stmt::Emit(Intent { name, args })`
//!
//! The `claim_pattern` helper produces the raw `(String, Vec<Term>)`
//! tuple; each statement form wraps it in its own IR shape.
//!
//! Why `bind`/`admit`/`retract`/`emit` share the claim-pattern
//! restriction: each verb operates on a single claim shape; the
//! meaningful authoring form is `Verb Name(args)`. The IR's
//! `Stmt::BindOne` could carry any `Prop`, but the surface stays
//! narrower per Position A doctrine - surface less permissive
//! than IR when the meaningful authoring form is narrower.
//!
//! `let x = new Subject()` is a binding-context extension (fresh
//! subject identifier), distinct from `admit`/`retract`/`emit`
//! which actually mutate admitted state.
//!
//! `for x in coll: body` is the only statement that introduces
//! nested layout (an Indent inside the transformation's outer
//! Indent). The statement parser is therefore recursive: the
//! `for_stmt` production references the statement parser for the
//! body's `stmt+` repetition.
//!
//! Statement separators are not needed: each statement begins with
//! its own keyword, so the boundary between adjacent statements is
//! "the next statement keyword". The body of a transformation
//! parses as `stmt+` with no explicit punctuation.

use super::expr::duration_ctor;
use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{Claim, Intent, PredicateArgKind, Prop, Stmt, Term, Value, ValueExpr};

use crate::lexer::Token;

use super::expr::{expression_parser, value_expr_parser};

/// Build a parser for a single statement.
///
/// Recursive: `for_stmt`'s body references the statement parser
/// itself (so `for` blocks can nest other statements, including
/// other `for` blocks). The recursion is bounded by the layout
/// pass's matched `Indent` / `Dedent` token pairs.
pub(super) fn statement_parser<'a, I>() -> impl Parser<'a, I, Stmt, extra::Err<Rich<'a, Token>>>
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    // `require` bodies are propositions; `let` values and `for`
    // collections are value expressions. The two sorts have separate
    // parsers, used in their respective statement positions below.
    let proposition = expression_parser();
    let value_expr = value_expr_parser();

    recursive(|statement| {
        let ident = select! { Token::Ident(s) => s };
        let decimal_lit = select! { Token::DecimalLit(s) => s };
        let date_lit = select! { Token::DateLit(s) => s };
        let subject_lit = select! { Token::SubjectLit(s) => s };

        // term ::= Ident | "_" | DecimalLit | TimestampLit | DateLit
        //        | duration(ISO) | SubjectLit
        let timestamp_lit = select! { Token::TimestampLit(s) => s };
        let term = choice((
            just(Token::Wildcard).to(Term::Wildcard),
            decimal_lit.map(|s| Term::Literal(Value::Decimal(s))),
            timestamp_lit.map(|s| Term::Literal(Value::Timestamp(s))),
            date_lit.map(|s| Term::Literal(Value::Date(s))),
            // Before bare idents so `duration(...)` is the constructor,
            // not a variable followed by a stray paren.
            duration_ctor().map(|s| Term::Literal(Value::Duration(s))),
            subject_lit.map(|s| Term::Literal(Value::Subject(s.into()))),
            ident.map(|name| {
                if name == "actor" {
                    Term::Actor
                } else {
                    Term::Var(name.into())
                }
            }),
        ));
        let term_list = term
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<Term>>();

        // claim_pattern ::= Ident "(" term_list ")"
        //
        // Returns a (predicate, args) tuple. Each statement verb
        // wraps the tuple in its own IR shape - see the module
        // doc for the mapping.
        let claim_pattern =
            ident.then(term_list.delimited_by(just(Token::LParen), just(Token::RParen)));

        // require <proposition>
        let require_stmt = just(Token::KwRequire)
            .ignore_then(proposition.clone())
            .map(Stmt::Require);

        // bind <claim_pattern>
        let bind_stmt =
            just(Token::KwBind)
                .ignore_then(claim_pattern.clone())
                .map(|(predicate, args)| {
                    Stmt::BindOne(Prop::Claim {
                        predicate: predicate.into(),
                        args,
                    })
                });

        // admit <claim_pattern>
        //
        // Wildcards are rejected at parse time. The kernel emits
        // "wildcard not allowed in assert" at runtime for any
        // wildcard arg; the surface refuses to produce IR the
        // kernel will refuse to evaluate, per the doctrine of
        // "no surface form without a meaningful IR mapping". The
        // `validate` here surfaces the diagnostic with the
        // statement's span.
        let admit_stmt = just(Token::KwAdmit)
            .ignore_then(claim_pattern.clone())
            .validate(|(predicate, args), e, emitter| {
                let span: SimpleSpan = e.span();
                if args.iter().any(|t| matches!(t, Term::Wildcard)) {
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
        // Wildcards ARE meaningful in retract: they widen the
        // pattern (e.g. `retract Foo(x, _)` retracts every Foo
        // claim whose first arg matches x, regardless of the
        // second). No surface-level wildcard restriction.
        let retract_stmt = just(Token::KwRetract)
            .ignore_then(claim_pattern.clone())
            .map(|(predicate, args)| Stmt::Retract {
                predicate: predicate.into(),
                args,
            });

        // emit <claim_pattern>
        //
        // `Intent { name, args }` shares the predicate-and-args
        // shape with `Claim`; the field is named `name` rather
        // than `predicate` in the IR (intents are not claims even
        // though they look syntactically alike).
        //
        // Wildcards are rejected at parse time, same reasoning
        // as `admit`: the kernel emits "wildcard not allowed in
        // emit" for any wildcard arg in an intent.
        let emit_stmt = just(Token::KwEmit)
            .ignore_then(claim_pattern.clone())
            .validate(|(name, args), e, emitter| {
                let span: SimpleSpan = e.span();
                if args.iter().any(|t| matches!(t, Term::Wildcard)) {
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
        // The collection is parsed as a value expression. The kernel's
        // `Stmt::For.collection` is a `ValueExpr`; whatever it evaluates
        // to must be an `EvalValue::Collection` at runtime, but the
        // surface accepts any value expression.
        //
        // The body is `Indent statement+ Dedent`, identical in shape
        // to the transformation body itself. `statement` is the
        // recursive reference; chumsky resolves the cycle.
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

/// Discriminator for the two `let` RHS forms. Internal to the
/// parser; the surface keywords (`new Subject ( )` vs an
/// expression) drive the choice.
enum LetRhs {
    NewSubject,
    Value(ValueExpr),
}
