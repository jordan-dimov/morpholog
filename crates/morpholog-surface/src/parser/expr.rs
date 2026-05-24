//! Expression-level parsing: the recursive `expression_parser`
//! combinator plus the public `parse_expression` entry point.
//!
//! Used directly by callers and tests for standalone expressions, and
//! indirectly by `program::program_parser` for invariant bodies,
//! transformation statement conditions, and derived-claim expressions.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{Expr, Term, Value};

use crate::diagnostics::Diagnostic;
use crate::lexer::{Token, lex, token_stream};

pub fn parse_expression(source: &str) -> Result<Expr, Vec<Diagnostic>> {
    let tokens = match lex(source) {
        Ok(t) => t,
        Err(errs) => {
            return Err(errs
                .into_iter()
                .map(|e| {
                    Diagnostic::error(format!("lex error: {}", e.reason()), e.span().into_range())
                })
                .collect());
        }
    };

    if tokens.is_empty() {
        let end = source.len().min(1);
        return Err(vec![Diagnostic::error(
            "expected expression, found empty input",
            0..end,
        )]);
    }

    let stream = token_stream(&tokens);
    let (parsed, errs) = expression_parser()
        .then_ignore(end())
        .parse(stream)
        .into_output_errors();

    let diagnostics: Vec<Diagnostic> = errs
        .into_iter()
        .map(|e| {
            let span = e.span();
            Diagnostic::error(
                format!("parse error: {}", e.reason()),
                span.start()..span.end(),
            )
        })
        .collect();

    let Some(expr) = parsed else {
        if diagnostics.is_empty() {
            return Err(vec![Diagnostic::error("parse failed", 0..source.len())]);
        }
        return Err(diagnostics);
    };

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    Ok(expr)
}

/// Build the recursive expression parser. Increasing precedence:
/// implies (lowest) -> or -> and -> not -> comparison -> arith ->
/// primary (highest). `recursive` lets `primary` reference
/// `expression` so parenthesised sub-expressions nest arbitrarily.
pub(super) fn expression_parser<'a, I>()
-> impl Parser<'a, I, Expr, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    recursive(|expression| {
        let ident = select! { Token::Ident(s) => s };
        let decimal_lit = select! { Token::DecimalLit(s) => s };

        let date_lit = select! { Token::DateLit(s) => s };
        let subject_lit = select! { Token::SubjectLit(s) => s };

        // A `Term` is the limited atom that claim-call args and `Neq` /
        // `In` operands accept: variables (including the special
        // `actor`), wildcards, and decimal / date / subject literals.
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
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<Term>>();

        // A `primary` Expr: parens, a literal/wildcard, a claim call, or
        // a bare variable. The ident-vs-claim-call ambiguity is resolved
        // by peeking for a following `(` via `ident.then(args.or_not())`.
        let parenthesised = expression
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        let decimal_as_expr = decimal_lit.map(|s| Expr::Term(Term::Literal(Value::Decimal(s))));
        let date_as_expr = date_lit.map(|s| Expr::Term(Term::Literal(Value::Date(s))));
        let subject_as_expr = subject_lit.map(|s| Expr::Term(Term::Literal(Value::Subject(s))));
        let wildcard_as_expr = just(Token::Wildcard).to(Expr::Term(Term::Wildcard));

        let ident_or_call = ident
            .then(
                term_list
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen))
                    .or_not(),
            )
            .map(|(name, args)| match args {
                Some(args) => Expr::Claim {
                    predicate: name,
                    args,
                },
                None => {
                    if name == "actor" {
                        Expr::Term(Term::Actor)
                    } else {
                        Expr::Term(Term::Var(name))
                    }
                }
            });

        // sum aggregator: `sum ( <var-name> | <body-expr> )`
        //
        // Target is restricted to a non-reserved variable. Literals and
        // wildcards fail earlier because `ident` only matches
        // Token::Ident; `actor` must be caught here, since the lexer
        // treats it as a plain identifier.
        let sum_expr = just(Token::KwSum)
            .ignore_then(
                ident
                    .then_ignore(just(Token::Pipe))
                    .then(expression.clone())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .validate(|(name, body): (String, Expr), e, emitter| {
                if name == "actor" {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a sum target: `actor` is reserved as the special term that resolves to the proposing transition's actor, not a regular variable",
                    ));
                }
                Expr::Sum {
                    value: Term::Var(name.clone()),
                    binding: name,
                    body: Box::new(body),
                }
            });

        // value lookup: `value <Ident> ( <term-list> )` with
        // optional `default <expr>` suffix. The wildcard in the
        // args marks the value position the IR extracts.
        let value_expr = just(Token::KwValue)
            .ignore_then(ident)
            .then(
                term_list
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .then(
                just(Token::KwDefault)
                    .ignore_then(expression.clone())
                    .or_not(),
            )
            .map(|((predicate, args), default)| Expr::ValueOf {
                predicate,
                args,
                default: default.map(Box::new),
            });

        // pre wrapper: `pre ( <expr> )`. Flips the wrapped subtree's
        // state lookup from the default (post / candidate) to
        // pre-transition. Parens are mandatory; the lexer reserves `pre`
        // everywhere so a bare `pre` surfaces as an unexpected-token
        // diagnostic rather than a silent Var("pre").
        let pre_expr = just(Token::KwPre)
            .ignore_then(
                expression
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|inner: Expr| Expr::Pre(Box::new(inner)));

        let primary = choice((
            sum_expr,
            value_expr,
            pre_expr,
            parenthesised,
            decimal_as_expr,
            date_as_expr,
            subject_as_expr,
            wildcard_as_expr,
            ident_or_call,
        ));

        // arith ::= primary (("+" | "-") primary)*  (left-assoc)
        //
        // foldl builds the left-associative tree: a + b + c -> Add(Add(a, b), c).
        let arith_op = choice((just(Token::Plus).to(true), just(Token::Minus).to(false)));
        let arith = primary.clone().foldl(
            arith_op.then(primary.clone()).repeated(),
            |lhs, (is_plus, rhs)| {
                if is_plus {
                    Expr::Add(Box::new(lhs), Box::new(rhs))
                } else {
                    Expr::Sub(Box::new(lhs), Box::new(rhs))
                }
            },
        );

        // comparison ::= arith (cmp_op arith)?  (non-assoc)
        //
        // Surface forms and their IR lowering:
        //   - `=` -> Expr::Eq(Expr, Expr)
        //   - `<=` -> Expr::Le(Expr, Expr)   (decimal)
        //   - `on_or_before` -> Expr::DateLe(Expr, Expr)   (civil date)
        //   - `!=` -> Expr::Neq(Term, Term)  - both sides must be Terms
        //   - `in` -> Expr::In(Term, Term)  - both sides must be Terms
        //
        // `<=` and `on_or_before` are distinct surface forms because the
        // kernel keeps `Expr::Le` and `Expr::DateLe` as separate IR
        // primitives; the surface refuses to overload `<=` by operand
        // kind.
        //
        // For `!=` and `in` we accept any Expr on either side, then
        // require a bare `Expr::Term(t)`, emitting a clean diagnostic
        // otherwise. This `in` is the membership comparator; the
        // structural `in` of `forall x in source:` is consumed by the
        // forall production before reaching this level.
        let comparison = arith.clone().then(
            choice((
                just(Token::Eq).to(CmpOp::Eq),
                just(Token::Neq).to(CmpOp::Neq),
                just(Token::Le).to(CmpOp::Le),
                just(Token::KwOnOrBefore).to(CmpOp::DateLe),
                just(Token::KwIn).to(CmpOp::In),
            ))
            .then(arith.clone())
            .or_not(),
        ).validate(|(lhs, rhs_opt), e, emitter| {
            match rhs_opt {
                None => lhs,
                Some((CmpOp::Eq, rhs)) => Expr::Eq(Box::new(lhs), Box::new(rhs)),
                Some((CmpOp::Le, rhs)) => Expr::Le(Box::new(lhs), Box::new(rhs)),
                Some((CmpOp::DateLe, rhs)) => Expr::DateLe(Box::new(lhs), Box::new(rhs)),
                Some((CmpOp::Neq, rhs)) => {
                    let span: SimpleSpan = e.span();
                    let lhs_term = expr_as_term(&lhs);
                    let rhs_term = expr_as_term(&rhs);
                    match (lhs_term, rhs_term) {
                        (Some(l), Some(r)) => Expr::Neq(l, r),
                        _ => {
                            emitter.emit(Rich::custom(
                                span,
                                "`!=` requires both sides to be terms (variable, wildcard, literal, or `actor`); arithmetic and other expressions are not allowed because the IR's Neq operates on terms only",
                            ));
                            Expr::Eq(Box::new(lhs), Box::new(rhs))
                        }
                    }
                }
                Some((CmpOp::In, rhs)) => {
                    let span: SimpleSpan = e.span();
                    let lhs_term = expr_as_term(&lhs);
                    let rhs_term = expr_as_term(&rhs);
                    match (lhs_term, rhs_term) {
                        (Some(l), Some(r)) => Expr::In(l, r),
                        _ => {
                            emitter.emit(Rich::custom(
                                span,
                                "`in` (membership) requires both sides to be terms (variable, wildcard, literal, or `actor`); arithmetic and other expressions are not allowed because the IR's In operates on terms only",
                            ));
                            Expr::Eq(Box::new(lhs), Box::new(rhs))
                        }
                    }
                }
            }
        });

        // not_expr ::= "not" not_expr | comparison
        let not_expr = recursive(|not_expr| {
            choice((
                just(Token::KwNot)
                    .ignore_then(not_expr)
                    .map(|inner: Expr| Expr::Not(Box::new(inner))),
                comparison,
            ))
        });

        // and_expr ::= not_expr ("and" not_expr)*  (left-assoc,
        // flattened into a single Expr::And(Vec<Expr>))
        let and_expr = not_expr
            .clone()
            .then(
                just(Token::KwAnd)
                    .ignore_then(not_expr.clone())
                    .repeated()
                    .collect::<Vec<Expr>>(),
            )
            .map(|(first, rest)| {
                if rest.is_empty() {
                    first
                } else {
                    let mut all = Vec::with_capacity(rest.len() + 1);
                    all.push(first);
                    all.extend(rest);
                    Expr::And(all)
                }
            });

        // or_expr ::= and_expr ("or" and_expr)*  (left-assoc,
        // flattened into a single Expr::Or(Vec<Expr>))
        //
        // Standard logical precedence: `and` tighter than `or` tighter
        // than `implies`, so `a and b or c implies d` parses as
        // `((a and b) or c) implies d`.
        let or_expr = and_expr
            .clone()
            .then(
                just(Token::KwOr)
                    .ignore_then(and_expr.clone())
                    .repeated()
                    .collect::<Vec<Expr>>(),
            )
            .map(|(first, rest)| {
                if rest.is_empty() {
                    first
                } else {
                    let mut all = Vec::with_capacity(rest.len() + 1);
                    all.push(first);
                    all.extend(rest);
                    Expr::Or(all)
                }
            });

        // implies ::= or ("implies" implies)?  (right-assoc)
        let implies_expr = or_expr
            .clone()
            .then(
                just(Token::KwImplies)
                    .ignore_then(
                        or_expr.clone().then(
                            // Allow chained `implies` via recursion: a implies b implies c.
                            just(Token::KwImplies)
                                .ignore_then(or_expr)
                                .repeated()
                                .collect::<Vec<Expr>>(),
                        ),
                    )
                    .or_not(),
            )
            .map(|(first, rest_opt)| {
                match rest_opt {
                    None => first,
                    Some((second, more)) => {
                        // Right-associate: build chain from the right.
                        let mut chain = vec![first, second];
                        chain.extend(more);
                        let mut iter = chain.into_iter().rev();
                        let init = match iter.next() {
                            Some(e) => e,
                            None => unreachable!("chain has at least two elements"),
                        };
                        iter.fold(init, |acc, left| Expr::Implies {
                            left: Box::new(left),
                            right: Box::new(acc),
                        })
                    }
                }
            });

        // expression ::= quantifier | implies
        //
        // Quantifiers sit at the very top of the grammar - higher than
        // `implies` - so their bodies greedily consume the rest of the
        // expression after the colon. In `forall x in xs: A and B` the
        // body is the whole conjunction, not just `A`, matching
        // mathematical convention. Compose with outer expressions by
        // parenthesising: `(forall x in xs: body) and outer`.
        //
        // The body accepts both inline and indented forms. The layout
        // pass emits `Indent` / `Dedent` around an indented body; the
        // inline form has no layout tokens.
        let quantifier_body = choice((
            just(Token::Indent)
                .ignore_then(expression.clone())
                .then_ignore(just(Token::Dedent)),
            expression.clone(),
        ));

        let exists_expr = just(Token::KwExists)
            .ignore_then(ident)
            .then_ignore(just(Token::Colon))
            .then(quantifier_body.clone())
            .validate(|(binding, body): (String, Expr), e, emitter| {
                if binding == "actor" {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a quantifier binder: `actor` is reserved as the special term that resolves to the proposing transition's actor; references inside the body would resolve to that term, not the bound variable",
                    ));
                }
                Expr::Exists {
                    binding,
                    body: Box::new(body),
                }
            });

        // Restricted source parser for `forall x in <source>:`.
        //
        // The kernel's `Expr::Forall { source, .. }` requires a
        // predicate-shaped source so `find_matches` can produce binding
        // extensions. Per the surface doctrine the parser must refuse
        // what the kernel cannot evaluate, so the unparenthesised source
        // grammar admits only:
        //   - bare variable (Ident, no `(`)   -> auto-lifted to In below
        //   - claim call (Ident "(" terms ")") -> used as-is
        //   - parenthesised expression         -> used as-is
        //
        // Value-shaped primaries (literals, wildcards, `sum(...)`,
        // `value Foo(...)`) are excluded and surface as parse errors;
        // wrapping in parens (`(sum(...))`) signals the user took
        // responsibility and passes through.
        let bare_ident_or_call = ident
            .then(
                term_list
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen))
                    .or_not(),
            )
            .map(|(name, args)| match args {
                Some(args) => Expr::Claim {
                    predicate: name,
                    args,
                },
                None => {
                    if name == "actor" {
                        Expr::Term(Term::Actor)
                    } else {
                        Expr::Term(Term::Var(name))
                    }
                }
            });
        let parenthesised_source = expression
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));
        let forall_source = choice((parenthesised_source, bare_ident_or_call));

        let forall_expr = just(Token::KwForall)
            .ignore_then(ident)
            .then_ignore(just(Token::KwIn))
            .then(forall_source)
            .then_ignore(just(Token::Colon))
            .then(quantifier_body)
            .validate(|((binding, source), body): ((String, Expr), Expr), e, emitter| {
                if binding == "actor" {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a quantifier binder: `actor` is reserved as the special term that resolves to the proposing transition's actor; references inside the body would resolve to that term, not the bound variable",
                    ));
                }
                // A bare Term-wrapped source (variable or `actor`) is
                // lifted to an In-expression binding the variable;
                // anything already predicate-shaped is used as-is.
                let source_expr = match source {
                    Expr::Term(t) => Expr::In(Term::Var(binding.clone()), t),
                    other => other,
                };
                Expr::Forall {
                    binding,
                    source: Box::new(source_expr),
                    body: Box::new(body),
                }
            });

        choice((exists_expr, forall_expr, implies_expr))
    })
}

/// Discriminator for the comparison operators. Internal to the
/// parser; the surface uses `=`, `!=`, `<=`, `in`, `on_or_before`
/// directly.
#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Neq,
    /// Decimal `<=` -> `Expr::Le`. Operands must be
    /// `EvalValue::Decimal` (checked at runtime).
    Le,
    /// Civil-date `on_or_before` -> `Expr::DateLe`. Operands must be
    /// `EvalValue::Date` (checked at runtime).
    DateLe,
    /// Membership comparator (`x in xs`) -> `Expr::In(Term, Term)`,
    /// with the same term-only restriction as `Neq`. Distinct from the
    /// structural `in` in `forall x in source: body`.
    In,
}

/// Unwrap a term-shaped `Expr::Term(_)`, or `None` for any compound
/// expression. Enforces the IR's term-only restriction on `Neq` and
/// `In` operands.
fn expr_as_term(e: &Expr) -> Option<Term> {
    match e {
        Expr::Term(t) => Some(t.clone()),
        _ => None,
    }
}
