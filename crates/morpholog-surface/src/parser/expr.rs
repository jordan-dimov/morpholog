//! Expression-level parsing: the recursive proposition parser
//! (`expression_parser`) and value-expression parser
//! (`value_expr_parser`), plus the public entry points `parse_expression`
//! (returns a [`Prop`]) and `parse_value_expr` (returns a [`ValueExpr`]).
//!
//! The two sorts are mutually recursive - a comparison relates two value
//! expressions, a `sum` ranges over a proposition - so the productions
//! split into prop-productions and value-productions that reference each
//! other. The programme-facing default is the proposition parser:
//! invariant and `require` bodies are propositions. Value expressions
//! appear only nested (a comparator operand, a `let` value, a `sum`
//! target, a `for` collection, a derived-claim value expression), and the
//! narrower `parse_value_expr` serves those positions and their tests.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{ArithOp, CompareOp, OrderedDomain, Prop, Term, Value, ValueExpr};

/// Build a `Prop::Compare` from a factored operator and domain. The
/// parser's flat `CmpOp` (op-and-domain in one token) maps onto the IR's
/// factored shape here; the inverse mapping is `format::compare_token`.
/// The `duration(PT6H)` constructor: an ISO-8601 duration in exact
/// time units, written without quotes - the payload lexes as a plain
/// identifier, since ISO durations always start with `P`. Validated
/// here via jiff so a malformed literal is a parse diagnostic with a
/// span, not a runtime evaluation error. `duration` itself is
/// contextual (matched only when followed by `(`), so it remains
/// usable as an ordinary variable name.
pub(super) fn duration_ctor<'a, I>()
-> impl Parser<'a, I, String, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    select! { Token::Ident(s) if s == "duration" => () }
        .ignore_then(
            select! { Token::Ident(s) => s }.delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .validate(|s: String, e, emitter| {
            if s.parse::<jiff::SignedDuration>().is_err() {
                let span: SimpleSpan = e.span();
                emitter.emit(Rich::custom(
                    span,
                    format!(
                        "invalid duration literal `{s}` (expected ISO 8601 \
                         time units, e.g. PT6H or PT1H30M)"
                    ),
                ));
            }
            s
        })
}

fn compare(op: CompareOp, domain: OrderedDomain, lhs: ValueExpr, rhs: ValueExpr) -> Prop {
    Prop::Compare {
        op,
        domain,
        left: Box::new(lhs),
        right: Box::new(rhs),
    }
}

use crate::diagnostics::Diagnostic;
use crate::lexer::{Token, lex, token_stream};

/// Parse a standalone proposition. The programme-facing default:
/// invariant and `require` bodies are propositions.
pub fn parse_expression(source: &str) -> Result<Prop, Vec<Diagnostic>> {
    let tokens = lex_or_diagnostics(source)?;
    let stream = token_stream(&tokens);
    let (parsed, errs) = expression_parser()
        .then_ignore(end())
        .parse(stream)
        .into_output_errors();
    finish(parsed, errs, source)
}

/// Parse a standalone value expression. Serves the value-position tests
/// and any value-position production; a value expression is never a
/// Morpholog body on its own, only nested.
pub fn parse_value_expr(source: &str) -> Result<ValueExpr, Vec<Diagnostic>> {
    let tokens = lex_or_diagnostics(source)?;
    let stream = token_stream(&tokens);
    let (parsed, errs) = value_expr_parser()
        .then_ignore(end())
        .parse(stream)
        .into_output_errors();
    finish(parsed, errs, source)
}

/// Lex `source`, mapping a lex failure to diagnostics and an empty token
/// stream to an "expected expression" diagnostic. Shared by both entry
/// points.
fn lex_or_diagnostics(source: &str) -> Result<Vec<crate::lexer::SpannedToken>, Vec<Diagnostic>> {
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
    Ok(tokens)
}

/// Turn a chumsky parse result into `Result<T, Vec<Diagnostic>>`. Shared
/// by both entry points.
fn finish<T>(
    parsed: Option<T>,
    errs: Vec<Rich<'_, Token>>,
    source: &str,
) -> Result<T, Vec<Diagnostic>> {
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

    let Some(parsed) = parsed else {
        if diagnostics.is_empty() {
            return Err(vec![Diagnostic::error("parse failed", 0..source.len())]);
        }
        return Err(diagnostics);
    };

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    Ok(parsed)
}

/// Build the recursive proposition parser. Increasing precedence:
/// implies (lowest) -> or -> and -> not -> comparison -> prop atom. A
/// comparison relates two value expressions; the value-expression
/// grammar is built inside this closure (also `recursive`) and the two
/// reference each other - a `sum` target's body is a proposition, a
/// comparator operand is a value expression.
pub(super) fn expression_parser<'a, I>()
-> impl Parser<'a, I, Prop, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    recursive(|expression| {
        let ident = select! { Token::Ident(s) => s };
        let decimal_lit = select! { Token::DecimalLit(s) => s };
        let date_lit = select! { Token::DateLit(s) => s };
        let subject_lit = select! { Token::SubjectLit(s) => s };

        // A `Term` is the limited atom that claim-call args and `In`
        // operands accept: variables (including the special `actor`),
        // wildcards, and decimal / date / subject literals.
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
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<Term>>();

        // The value-expression grammar. Built here, inside the
        // proposition closure, so a `sum` body can reference the
        // proposition parser (`expression`) and a comparator operand
        // (below) can reference this. `arith` is the value-expression
        // entry point used everywhere a value is required.
        let arith = value_arith_parser(expression.clone(), term_list.clone());

        // A prop-only atom: a claim call (with parens) or `pre(...)`.
        // These are the propositions that are not value expressions, so
        // they appear at the comparison level without a comparator. A
        // bare `Ident` (no parens) is a value `Term::Var`, not a claim,
        // and a parenthesised proposition is handled separately.
        let claim_call = ident
            .then(
                term_list
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|(name, args)| Prop::Claim {
                predicate: name.into(),
                args,
            });

        // pre wrapper: `pre ( <prop> )`. Flips the wrapped subtree's
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
            .map(|inner: Prop| Prop::Pre(Box::new(inner)));

        let parenthesised_prop = expression
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        // comparison ::= arith (cmp_op arith)?  (non-assoc), or a
        // prop-only atom (claim / pre / parenthesised prop).
        //
        // Surface forms and their IR lowering:
        //   - `=` -> Prop::Eq(ValueExpr, ValueExpr)
        //   - `<=` -> Prop::Compare { Le, Decimal, .. }
        //   - `on_or_before` -> Prop::Compare { Le, Date, .. }
        //   - `!=` -> Prop::Neq(ValueExpr, ValueExpr) - symmetric with `=`
        //   - `in` -> Prop::In(Term, Term) - both sides must be Terms
        //
        // `<=` and `on_or_before` are distinct surface forms because the
        // comparison's domain is carried explicitly in `Prop::Compare`;
        // the surface picks it by keyword rather than overloading one
        // operator by operand kind.
        //
        // The value-comparison alternative parses an `arith` then
        // *requires* a comparator (a bare value expression is not a
        // proposition). Prop-only atoms (claim, pre, parens) cover the
        // no-comparator case. For `!=` and `in` we accept any value
        // expression on either side, then require a bare term for `in`,
        // emitting a clean diagnostic otherwise. This `in` is the
        // membership comparator; the structural `in` of `forall x in
        // source:` is consumed by the forall production before reaching
        // this level.
        let value_comparison = arith
            .clone()
            .then(
                choice((
                    just(Token::Eq).to(CmpOp::Eq),
                    just(Token::Neq).to(CmpOp::Neq),
                    just(Token::Le).to(CmpOp::Le),
                    just(Token::Lt).to(CmpOp::Lt),
                    just(Token::Ge).to(CmpOp::Ge),
                    just(Token::Gt).to(CmpOp::Gt),
                    just(Token::KwOnOrBefore).to(CmpOp::DateLe),
                    just(Token::KwOnOrAfter).to(CmpOp::DateGe),
                    // `before`/`after` are contextual: matched as comparators
                    // here, but left as ordinary identifiers everywhere else
                    // so a variable may still be named `before` or `after`
                    // (the worked examples do exactly that).
                    select! { Token::Ident(s) if s == "before" => CmpOp::DateLt },
                    select! { Token::Ident(s) if s == "after" => CmpOp::DateGt },
                    select! { Token::Ident(s) if s == "at_or_before" => CmpOp::TsLe },
                    select! { Token::Ident(s) if s == "strictly_before" => CmpOp::TsLt },
                    select! { Token::Ident(s) if s == "at_or_after" => CmpOp::TsGe },
                    select! { Token::Ident(s) if s == "strictly_after" => CmpOp::TsGt },
                    select! { Token::Ident(s) if s == "no_longer_than" => CmpOp::DurLe },
                    select! { Token::Ident(s) if s == "shorter_than" => CmpOp::DurLt },
                    select! { Token::Ident(s) if s == "no_shorter_than" => CmpOp::DurGe },
                    select! { Token::Ident(s) if s == "longer_than" => CmpOp::DurGt },
                    just(Token::KwIn).to(CmpOp::In),
                ))
                .then(arith.clone()),
            )
            .validate(|(lhs, (op, rhs)), e, emitter| match op {
                CmpOp::Eq => Prop::Eq(Box::new(lhs), Box::new(rhs)),
                CmpOp::Le => compare(CompareOp::Le, OrderedDomain::Decimal, lhs, rhs),
                CmpOp::Lt => compare(CompareOp::Lt, OrderedDomain::Decimal, lhs, rhs),
                CmpOp::Ge => compare(CompareOp::Ge, OrderedDomain::Decimal, lhs, rhs),
                CmpOp::Gt => compare(CompareOp::Gt, OrderedDomain::Decimal, lhs, rhs),
                CmpOp::DateLe => compare(CompareOp::Le, OrderedDomain::Date, lhs, rhs),
                CmpOp::DateLt => compare(CompareOp::Lt, OrderedDomain::Date, lhs, rhs),
                CmpOp::DateGe => compare(CompareOp::Ge, OrderedDomain::Date, lhs, rhs),
                CmpOp::DateGt => compare(CompareOp::Gt, OrderedDomain::Date, lhs, rhs),
                CmpOp::TsLe => compare(CompareOp::Le, OrderedDomain::Timestamp, lhs, rhs),
                CmpOp::TsLt => compare(CompareOp::Lt, OrderedDomain::Timestamp, lhs, rhs),
                CmpOp::TsGe => compare(CompareOp::Ge, OrderedDomain::Timestamp, lhs, rhs),
                CmpOp::TsGt => compare(CompareOp::Gt, OrderedDomain::Timestamp, lhs, rhs),
                CmpOp::DurLe => compare(CompareOp::Le, OrderedDomain::Duration, lhs, rhs),
                CmpOp::DurLt => compare(CompareOp::Lt, OrderedDomain::Duration, lhs, rhs),
                CmpOp::DurGe => compare(CompareOp::Ge, OrderedDomain::Duration, lhs, rhs),
                CmpOp::DurGt => compare(CompareOp::Gt, OrderedDomain::Duration, lhs, rhs),
                CmpOp::Neq => Prop::Neq(Box::new(lhs), Box::new(rhs)),
                CmpOp::In => {
                    let span: SimpleSpan = e.span();
                    let lhs_term = value_as_term(&lhs);
                    let rhs_term = value_as_term(&rhs);
                    match (lhs_term, rhs_term) {
                        (Some(l), Some(r)) => Prop::In(l, r),
                        _ => {
                            emitter.emit(Rich::custom(
                                span,
                                "`in` (membership) requires both sides to be terms (variable, wildcard, literal, or `actor`); arithmetic and other expressions are not allowed because the IR's In operates on terms only",
                            ));
                            Prop::Eq(Box::new(lhs), Box::new(rhs))
                        }
                    }
                }
            });

        // The comparison level: a value comparison, or a prop-only atom
        // standing alone. Order matters: try the value comparison first
        // so `amount <= 100` does not stall on the claim-call attempt;
        // claim / pre / parenthesised-prop cover the no-comparator case.
        let comparison = choice((value_comparison, claim_call, pre_expr, parenthesised_prop));

        // not_expr ::= "not" not_expr | comparison
        let not_expr = recursive(|not_expr| {
            choice((
                just(Token::KwNot)
                    .ignore_then(not_expr)
                    .map(|inner: Prop| Prop::Not(Box::new(inner))),
                comparison,
            ))
        });

        // and_expr ::= not_expr ("and" not_expr)*  (left-assoc,
        // flattened into a single Prop::And(Vec<Prop>))
        let and_expr = not_expr
            .clone()
            .then(
                just(Token::KwAnd)
                    .ignore_then(not_expr.clone())
                    .repeated()
                    .collect::<Vec<Prop>>(),
            )
            .map(|(first, rest)| {
                if rest.is_empty() {
                    first
                } else {
                    let mut all = Vec::with_capacity(rest.len() + 1);
                    all.push(first);
                    all.extend(rest);
                    Prop::And(all)
                }
            });

        // xor_expr ::= and_expr ("xor" and_expr)*  (left-assoc, binary)
        //
        // `xor` sits between `and` and `or`: tighter than `or`, looser
        // than `and`, so `a and b xor c and d` parses as
        // `(a and b) xor (c and d)` - the natural "exactly one of these
        // two" reading. Unlike `and`/`or` it does not flatten (xor is
        // binary); a chain `a xor b xor c` nests left-associatively into
        // `Xor(Xor(a, b), c)`.
        let xor_expr = and_expr.clone().foldl(
            just(Token::KwXor).ignore_then(and_expr.clone()).repeated(),
            |left, right| Prop::Xor(Box::new(left), Box::new(right)),
        );

        // or_expr ::= xor_expr ("or" xor_expr)*  (left-assoc,
        // flattened into a single Prop::Or(Vec<Prop>))
        //
        // Standard logical precedence: `and` tighter than `xor` tighter
        // than `or` tighter than `implies`, so `a and b or c implies d`
        // parses as `((a and b) or c) implies d`.
        let or_expr = xor_expr
            .clone()
            .then(
                just(Token::KwOr)
                    .ignore_then(xor_expr.clone())
                    .repeated()
                    .collect::<Vec<Prop>>(),
            )
            .map(|(first, rest)| {
                if rest.is_empty() {
                    first
                } else {
                    let mut all = Vec::with_capacity(rest.len() + 1);
                    all.push(first);
                    all.extend(rest);
                    Prop::Or(all)
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
                                .collect::<Vec<Prop>>(),
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
                        let Some(init) = iter.next() else {
                            unreachable!("chain has at least two elements")
                        };
                        iter.fold(init, |acc, left| Prop::Implies {
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
            .validate(|(binding, body): (String, Prop), e, emitter| {
                if binding == "actor" {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a quantifier binder: `actor` is reserved as the special term that resolves to the proposing transition's actor; references inside the body would resolve to that term, not the bound variable",
                    ));
                }
                Prop::Exists {
                    binding: binding.into(),
                    body: Box::new(body),
                }
            });

        // Restricted source parser for `forall x in <source>:`.
        //
        // The kernel's `Prop::Forall { source, .. }` requires a
        // predicate-shaped source so `find_matches` can produce binding
        // extensions. Per the surface doctrine the parser must refuse
        // what the kernel cannot evaluate, so the unparenthesised source
        // grammar admits only:
        //   - bare variable (Ident, no `(`)   -> auto-lifted to In below
        //   - claim call (Ident "(" terms ")") -> used as-is
        //   - parenthesised proposition        -> used as-is
        //
        // Value-shaped primaries (literals, wildcards, `sum(...)`,
        // `value Foo(...)`) are excluded and surface as parse errors;
        // wrapping in parens (`(sum(...))`) signals the user took
        // responsibility - but a parenthesised proposition is what is
        // accepted, so a value inside parens still fails downstream.
        //
        // A bare identifier source lifts to `In(Var(binding), source)`;
        // anything already predicate-shaped is used as-is. The
        // `ForallSource` transient distinguishes the two before the
        // lift, so the source need not be re-inspected as a Prop.
        let forall_bare_source = ident.then(
            term_list
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        );
        let forall_source = choice((
            expression
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map(ForallSource::Prop),
            forall_bare_source.map(|(name, args)| match args {
                Some(args) => ForallSource::Prop(Prop::Claim {
                    predicate: name.into(),
                    args,
                }),
                None => ForallSource::BareTerm(if name == "actor" {
                    Term::Actor
                } else {
                    Term::Var(name.into())
                }),
            }),
        ));

        let forall_expr = just(Token::KwForall)
            .ignore_then(ident)
            .then_ignore(just(Token::KwIn))
            .then(forall_source)
            .then_ignore(just(Token::Colon))
            .then(quantifier_body)
            .validate(|((binding, source), body): ((String, ForallSource), Prop), e, emitter| {
                if binding == "actor" {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a quantifier binder: `actor` is reserved as the special term that resolves to the proposing transition's actor; references inside the body would resolve to that term, not the bound variable",
                    ));
                }
                // A bare-term source (variable or `actor`) is lifted to
                // an In-proposition binding the variable; anything
                // already predicate-shaped is used as-is.
                let source_prop = match source {
                    ForallSource::BareTerm(t) => Prop::In(Term::Var(binding.clone().into()), t),
                    ForallSource::Prop(p) => p,
                };
                Prop::Forall {
                    binding: binding.into(),
                    source: Box::new(source_prop),
                    body: Box::new(body),
                }
            });

        choice((exists_expr, forall_expr, implies_expr))
    })
}

/// A parsed `forall` source before the auto-lift decision. A bare term
/// (variable or `actor`) becomes `In(Var(binding), term)`; a claim or
/// parenthesised proposition is used as the source proposition as-is.
enum ForallSource {
    BareTerm(Term),
    Prop(Prop),
}

/// Build the recursive value-expression parser used as a standalone
/// entry point (`parse_value_expr`). Mirrors the value grammar nested
/// inside [`expression_parser`], but builds its own proposition parser
/// for `sum` bodies via `expression_parser()`.
pub(super) fn value_expr_parser<'a, I>()
-> impl Parser<'a, I, ValueExpr, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };
    let decimal_lit = select! { Token::DecimalLit(s) => s };
    let date_lit = select! { Token::DateLit(s) => s };
    let subject_lit = select! { Token::SubjectLit(s) => s };

    let timestamp_lit = select! { Token::TimestampLit(s) => s };
    let term = choice((
        just(Token::Wildcard).to(Term::Wildcard),
        decimal_lit.map(|s| Term::Literal(Value::Decimal(s))),
        timestamp_lit.map(|s| Term::Literal(Value::Timestamp(s))),
        date_lit.map(|s| Term::Literal(Value::Date(s))),
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

    value_arith_parser(expression_parser(), term_list)
}

/// Build the value-expression arithmetic chain: `primary (("+" | "-")
/// primary)*`, left-associative. `primary` covers a parenthesised value
/// expression, a `sum` aggregator (whose body is a proposition, parsed
/// by `prop`), a `value` lookup (whose `default` is a value expression),
/// literals, wildcards, and bare variables / `actor`. Shared by the
/// nested value grammar inside [`expression_parser`] and the standalone
/// [`value_expr_parser`].
fn value_arith_parser<'a, I, P>(
    prop: P,
    term_list: impl Parser<'a, I, Vec<Term>, extra::Err<Rich<'a, Token>>> + Clone + 'a,
) -> impl Parser<'a, I, ValueExpr, extra::Err<Rich<'a, Token>>> + Clone + 'a
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
    P: Parser<'a, I, Prop, extra::Err<Rich<'a, Token>>> + Clone + 'a,
{
    recursive(move |value| {
        let ident = select! { Token::Ident(s) => s };
        let decimal_lit = select! { Token::DecimalLit(s) => s };
        let date_lit = select! { Token::DateLit(s) => s };
        let subject_lit = select! { Token::SubjectLit(s) => s };

        let parenthesised = value
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        let timestamp_lit = select! { Token::TimestampLit(s) => s };
        let decimal_as_value =
            decimal_lit.map(|s| ValueExpr::Term(Term::Literal(Value::Decimal(s))));
        let date_as_value = date_lit.map(|s| ValueExpr::Term(Term::Literal(Value::Date(s))));
        let timestamp_as_value =
            timestamp_lit.map(|s| ValueExpr::Term(Term::Literal(Value::Timestamp(s))));
        let duration_as_value =
            duration_ctor().map(|s| ValueExpr::Term(Term::Literal(Value::Duration(s))));
        let subject_as_value =
            subject_lit.map(|s| ValueExpr::Term(Term::Literal(Value::Subject(s.into()))));
        let wildcard_as_value = just(Token::Wildcard).to(ValueExpr::Term(Term::Wildcard));

        // A bare identifier is a value variable (or `actor`). A
        // following `(` would make it a claim, which is not a value
        // expression - so the value grammar never accepts `Foo(args)`
        // (the prop grammar does). A bare ident is `Term::Var`.
        let bare_ident = ident.map(|name| {
            if name == "actor" {
                ValueExpr::Term(Term::Actor)
            } else {
                ValueExpr::Term(Term::Var(name.into()))
            }
        });

        // sum aggregator: `sum ( <target> | <body-prop> )`
        //
        // The target is either a variable bound by the body (the usual
        // `sum(amount | ...)`) or a decimal literal, which turns the sum
        // into a count of matches (`sum(1 | ...)`). `actor` lexes as a
        // plain identifier, so it must be rejected here.
        let sum_target = choice((
            ident.map(|name| Term::Var(name.into())),
            decimal_lit.map(|s| Term::Literal(Value::Decimal(s))),
        ));
        let sum_expr = just(Token::KwSum)
            .ignore_then(
                sum_target
                    .then_ignore(just(Token::Pipe))
                    .then(prop.clone())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .validate(|(target, body): (Term, Prop), e, emitter| {
                if matches!(&target, Term::Var(n) if n.as_str() == "actor") {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a sum target: `actor` is reserved as the special term that resolves to the proposing transition's actor, not a regular variable",
                    ));
                }
                ValueExpr::Sum {
                    value: target,
                    body: Box::new(body),
                }
            });

        // value lookup: `value <Ident> ( <term-list> )` with optional
        // `default <value>` suffix. The wildcard in the args marks the
        // value position the IR extracts.
        let value_lookup = just(Token::KwValue)
            .ignore_then(ident)
            .then(
                term_list
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .then(just(Token::KwDefault).ignore_then(value.clone()).or_not())
            .map(|((predicate, args), default)| ValueExpr::ValueOf {
                predicate: predicate.into(),
                args,
                default: default.map(Box::new),
            });

        // min / max functions: `min ( <value> , <value> )` and the same
        // for `max`. Binary, both operands full value expressions. (Not
        // aggregators - `sum` is the aggregator; these take two values.)
        let min_max_expr = choice((
            just(Token::KwMin).to(ArithOp::Min),
            just(Token::KwMax).to(ArithOp::Max),
        ))
        .then(
            value
                .clone()
                .then_ignore(just(Token::Comma))
                .then(value.clone())
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|(op, (lhs, rhs))| ValueExpr::Arith {
            op,
            left: Box::new(lhs),
            right: Box::new(rhs),
        });

        let primary = choice((
            sum_expr,
            min_max_expr,
            value_lookup,
            parenthesised,
            decimal_as_value,
            timestamp_as_value,
            date_as_value,
            // Before bare idents: `duration(...)` is the constructor.
            duration_as_value,
            subject_as_value,
            wildcard_as_value,
            bare_ident,
        ));

        // factor ::= primary (("*" | "/" | "%") primary)*  (left-assoc)
        //
        // The multiplicative layer binds tighter than `+`/`-`, so
        // `a + b * c` parses as `Add(a, Mul(b, c))` and `a + b % c` as
        // `Add(a, Mod(b, c))`.
        let mul_op = choice((
            just(Token::Star).to(ArithOp::Mul),
            just(Token::Slash).to(ArithOp::Div),
            just(Token::Percent).to(ArithOp::Mod),
        ));
        let factor =
            primary
                .clone()
                .foldl(mul_op.then(primary.clone()).repeated(), |lhs, (op, rhs)| {
                    ValueExpr::Arith {
                        op,
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                    }
                });

        // arith ::= factor (("+" | "-") factor)*  (left-assoc)
        //
        // foldl builds the left-associative tree: a + b + c -> Add(Add(a, b), c).
        let arith_op = choice((
            just(Token::Plus).to(ArithOp::Add),
            just(Token::Minus).to(ArithOp::Sub),
        ));
        factor.clone().foldl(
            arith_op.then(factor.clone()).repeated(),
            |lhs, (op, rhs)| ValueExpr::Arith {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
        )
    })
}

/// Discriminator for the comparison operators. Internal to the
/// parser; the surface uses `=`, `!=`, `<=`, `in`, `on_or_before`
/// directly.
#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Neq,
    /// Decimal comparators (`<=` `<` `>=` `>`) -> `Prop::Compare` with the
    /// `Decimal` domain. Operands must be `EvalValue::Decimal` (checked at
    /// runtime).
    Le,
    Lt,
    Ge,
    Gt,
    /// Civil-date comparators (`on_or_before` `before` `on_or_after`
    /// `after`) -> `Prop::Compare` with the `Date` domain. Operands must
    /// be `EvalValue::Date` (checked at runtime). `before` and `after`
    /// are matched contextually (in comparator position only), so they
    /// remain usable as ordinary variable names elsewhere.
    /// Instant comparators (`at_or_before` `strictly_before`
    /// `at_or_after` `strictly_after`) -> `Prop::Compare` with the
    /// `Timestamp` domain. All four are contextual identifiers.
    TsLe,
    TsLt,
    TsGe,
    TsGt,
    /// Span comparators (`no_longer_than` `shorter_than`
    /// `no_shorter_than` `longer_than`) -> `Prop::Compare` with the
    /// `Duration` domain. All four are contextual identifiers; read
    /// them as length comparisons (`counted no_longer_than allowed`).
    DurLe,
    DurLt,
    DurGe,
    DurGt,
    DateLe,
    DateLt,
    DateGe,
    DateGt,
    /// Membership comparator (`x in xs`) -> `Prop::In(Term, Term)`,
    /// term-only on both sides (the IR's `In` operates on terms).
    /// Distinct from the structural `in` in `forall x in source: body`.
    In,
}

/// Unwrap a term-shaped `ValueExpr::Term(_)`, or `None` for any compound
/// value expression. Enforces the IR's term-only restriction on `In`
/// operands.
fn value_as_term(e: &ValueExpr) -> Option<Term> {
    match e {
        ValueExpr::Term(t) => Some(t.clone()),
        _ => None,
    }
}
