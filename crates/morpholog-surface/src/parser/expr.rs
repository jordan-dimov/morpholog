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
use morpholog_core::{
    ArithOp, CompareOp, ExtremumOp, OrderedDomain, Prop, SumSeed, Term, Unit, Value, ValueExpr,
};

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

/// The `span(P3M)` constructor: a calendar span in date units (Y/M/W/D),
/// in the `duration(PT6H)` mould - the payload lexes as a plain
/// identifier (ISO periods always start with `P`), and `span` is
/// contextual (matched only when followed by `(`), so it remains
/// usable as an ordinary variable name. Validated here through the
/// kernel's own grammar (`morpholog_core::calendar`), so the parse
/// diagnostic and the evaluator cannot drift. Value-position only:
/// a span shifts a date inside arithmetic and is never a claim or
/// intent argument, so `term_parser` deliberately does not carry it.
pub(super) fn span_ctor<'a, I>() -> impl Parser<'a, I, String, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let payload = choice((
        select! { Token::Ident(s) => s },
        // A bare number (`span(3)`) is a shape mistake worth its own
        // hint; capture it so the diagnostic names the fix.
        select! { Token::DecimalLit(s) => s },
    ));
    select! { Token::Ident(s) if s == "span" => () }
        .ignore_then(payload.delimited_by(just(Token::LParen), just(Token::RParen)))
        .validate(|s: String, e, emitter| {
            if let Err(reason) = morpholog_core::calendar::parse_calendar_span(&s) {
                let span: SimpleSpan = e.span();
                emitter.emit(Rich::custom(
                    span,
                    format!("invalid span literal `{s}` ({reason})"),
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
    let tokens = lex(source).map_err(super::lex_error_diagnostics)?;
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
    let diagnostics = super::parse_error_diagnostics(errs);

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

/// A numeric literal, optionally followed by an identifier read as a
/// unit: `25000 USD` is a quantity literal, a bare `25000` a plain
/// decimal. An ill-typed contextual keyword after a bare number (e.g.
/// a time comparator) reads as a unit and fails downstream -
/// acceptable, since that expression was already ill-typed.
fn decimal_or_quantity_term<'a, I>() -> impl Parser<'a, I, Term, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };
    let decimal_lit = select! { Token::DecimalLit(s) => s };
    decimal_lit
        .then(ident.or_not())
        .map(|(s, unit)| match unit {
            Some(u) => Term::Literal(Value::Quantity {
                amount: s,
                unit: Unit::from(u),
            }),
            None => Term::Literal(Value::Decimal(s)),
        })
}

/// A `Term` is the limited atom that claim-call args and `In` operands
/// accept: variables (including the special `actor`), wildcards, and
/// decimal / quantity / timestamp / date / duration / subject literals.
pub(super) fn term_parser<'a, I>() -> impl Parser<'a, I, Term, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };
    let date_lit = select! { Token::DateLit(s) => s };
    let subject_lit = select! { Token::SubjectLit(s) => s };
    let timestamp_lit = select! { Token::TimestampLit(s) => s };
    choice((
        just(Token::Wildcard).to(Term::Wildcard),
        decimal_or_quantity_term(),
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
    ))
}

/// Comma-separated terms with an optional trailing comma - the
/// argument list shape shared by claim calls, claim patterns, and
/// `value` lookups.
pub(super) fn term_list_parser<'a, I>()
-> impl Parser<'a, I, Vec<Term>, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    term_parser()
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<Term>>()
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
        let term_list = term_list_parser();

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

        // comparison ::= arith (cmp_op arith)+, or a prop-only atom
        // (claim / pre / parenthesised prop).
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
        //
        // A range reads as spoken: `0 <= rate <= 1` chains, lowering
        // to the same `Prop::And` of pairwise comparisons the spelled
        // out `and` form produces - no new IR. Only the ordered
        // comparators chain, and every link must point the same way;
        // a mixed-direction chain is refused rather than guessed at.
        let value_comparison = arith
            .clone()
            .then(
                choice((
                    just(Token::Eq).to(CmpOp::Eq),
                    just(Token::Neq).to(CmpOp::Neq),
                    just(Token::Le).to(CmpOp::Compare(CompareOp::Le, OrderedDomain::Decimal)),
                    just(Token::Lt).to(CmpOp::Compare(CompareOp::Lt, OrderedDomain::Decimal)),
                    just(Token::Ge).to(CmpOp::Compare(CompareOp::Ge, OrderedDomain::Decimal)),
                    just(Token::Gt).to(CmpOp::Compare(CompareOp::Gt, OrderedDomain::Decimal)),
                    just(Token::KwOnOrBefore)
                        .to(CmpOp::Compare(CompareOp::Le, OrderedDomain::Date)),
                    just(Token::KwOnOrAfter)
                        .to(CmpOp::Compare(CompareOp::Ge, OrderedDomain::Date)),
                    // The remaining comparators are contextual: matched
                    // here, but left as ordinary identifiers everywhere
                    // else so a variable may still be named `before` or
                    // `after` (the worked examples do exactly that).
                    contextual_cmp("before", CompareOp::Lt, OrderedDomain::Date),
                    contextual_cmp("after", CompareOp::Gt, OrderedDomain::Date),
                    contextual_cmp("at_or_before", CompareOp::Le, OrderedDomain::Timestamp),
                    contextual_cmp("strictly_before", CompareOp::Lt, OrderedDomain::Timestamp),
                    contextual_cmp("at_or_after", CompareOp::Ge, OrderedDomain::Timestamp),
                    contextual_cmp("strictly_after", CompareOp::Gt, OrderedDomain::Timestamp),
                    contextual_cmp("no_longer_than", CompareOp::Le, OrderedDomain::Duration),
                    contextual_cmp("shorter_than", CompareOp::Lt, OrderedDomain::Duration),
                    contextual_cmp("no_shorter_than", CompareOp::Ge, OrderedDomain::Duration),
                    contextual_cmp("longer_than", CompareOp::Gt, OrderedDomain::Duration),
                    just(Token::KwIn).to(CmpOp::In),
                ))
                .then(arith.clone())
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>(),
            )
            .validate(|(lhs, links), e, emitter| {
                let span: SimpleSpan = e.span();
                let links = match <[(CmpOp, ValueExpr); 1]>::try_from(links) {
                    Err(links) => links,
                    Ok([(op, rhs)]) => {
                        return match op {
                        CmpOp::Eq => Prop::Eq(Box::new(lhs), Box::new(rhs)),
                        CmpOp::Compare(op, domain) => compare(op, domain, lhs, rhs),
                        CmpOp::Neq => Prop::Neq(Box::new(lhs), Box::new(rhs)),
                            CmpOp::In => {
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
                        };
                    }
                };
                let mut props = Vec::with_capacity(links.len());
                let mut downward: Option<bool> = None;
                let mut left = lhs;
                for (op, rhs) in links {
                    let CmpOp::Compare(op, domain) = op else {
                        emitter.emit(Rich::custom(
                            span,
                            "only the ordered comparators chain (`<=`, `<`, `>=`, `>` and the date, time, and duration forms); `=`, `!=`, and `in` relate exactly two things - split this with `and`",
                        ));
                        return Prop::And(props);
                    };
                    let down = matches!(op, CompareOp::Le | CompareOp::Lt);
                    if *downward.get_or_insert(down) != down {
                        emitter.emit(Rich::custom(
                            span,
                            "a chained comparison must point one way (`a <= x <= b`, or `b >= x >= a`); a mixed-direction chain is not a range - split it with `and`",
                        ));
                    }
                    props.push(compare(op, domain, left, rhs.clone()));
                    left = rhs;
                }
                Prop::And(props)
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
                    // Splice direct And children so a chained comparison
                    // or a parenthesised conjunction composes into the
                    // same flat vec its spelled-out form parses to.
                    let mut all = Vec::with_capacity(rest.len() + 1);
                    for prop in std::iter::once(first).chain(rest) {
                        match prop {
                            Prop::And(inner) => all.extend(inner),
                            other => all.push(other),
                        }
                    }
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
        let quantifier_body = super::indented_or_inline(expression.clone());

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
    value_arith_parser(expression_parser(), term_list_parser())
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
        let date_lit = select! { Token::DateLit(s) => s };
        let subject_lit = select! { Token::SubjectLit(s) => s };

        let parenthesised = value
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        let timestamp_lit = select! { Token::TimestampLit(s) => s };
        let decimal_as_value = decimal_or_quantity_term().map(ValueExpr::Term);
        let date_as_value = date_lit.map(|s| ValueExpr::Term(Term::Literal(Value::Date(s))));
        let timestamp_as_value =
            timestamp_lit.map(|s| ValueExpr::Term(Term::Literal(Value::Timestamp(s))));
        let duration_as_value =
            duration_ctor().map(|s| ValueExpr::Term(Term::Literal(Value::Duration(s))));
        let span_as_value =
            span_ctor().map(|s| ValueExpr::Term(Term::Literal(Value::CalendarSpan(s))));
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
            decimal_or_quantity_term(),
        ));
        let sum_expr = just(Token::KwSum)
            .ignore_then(
                sum_target
                    .clone()
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
                    seed: SumSeed::default(),
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

        // `min` and `max` open two different things, told apart by what
        // follows the target: a `,` gives the binary form that caps one
        // value against another, a `|` gives the aggregate that ranges
        // over the bindings a body defines. The keyword is consumed once
        // and the inner shape decides, so neither form has to unwind a
        // half-parsed call to try the other.
        let extremum_body = sum_target
            .clone()
            .then_ignore(just(Token::Pipe))
            .then(prop.clone())
            .map(MinMaxShape::Aggregate);
        let binary_body = value
            .clone()
            .then_ignore(just(Token::Comma))
            .then(value.clone())
            .map(MinMaxShape::Binary);
        let min_max_expr = choice((
            just(Token::KwMin).to(ExtremumOp::Min),
            just(Token::KwMax).to(ExtremumOp::Max),
        ))
        .then(
            choice((extremum_body, binary_body))
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .validate(|(op, shape), e, emitter| match shape {
            MinMaxShape::Aggregate((target, body)) => {
                if matches!(&target, Term::Var(n) if n.as_str() == "actor") {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be an aggregate target: `actor` is reserved as the special term that resolves to the proposing transition's actor, not a regular variable",
                    ));
                }
                ValueExpr::Extremum {
                    op,
                    value: target,
                    body: Box::new(body),
                }
            }
            MinMaxShape::Binary((lhs, rhs)) => ValueExpr::Arith {
                op: match op {
                    ExtremumOp::Min => ArithOp::Min,
                    ExtremumOp::Max => ArithOp::Max,
                },
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
        });

        // abs function: `abs ( <value> )`. Unary magnitude of a signed
        // value, the natural form for a two-sided bound (`abs(x) <= limit`).
        let abs_expr = just(Token::KwAbs)
            .ignore_then(
                value
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|operand| ValueExpr::Abs(Box::new(operand)));

        // round function: `round ( <value> , <value> )`. Nearest
        // multiple of the quantum, exact halves away from zero; the
        // money form is `round(raw, 0.01)`.
        let round_expr = just(Token::KwRound)
            .ignore_then(
                value
                    .clone()
                    .then_ignore(just(Token::Comma))
                    .then(value.clone())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|(v, quantum)| ValueExpr::Round {
                value: Box::new(v),
                quantum: Box::new(quantum),
            });

        // The conditional: `if ( <prop> , <value> , <value> )`. The
        // value selected by whether the proposition holds. Contextual
        // like `duration` - a constructor only when `if` is followed
        // by `(` - so `if` stays a legal variable name. Function-
        // shaped like `round`: self-delimiting, no precedence tier,
        // no dangling else.
        let if_expr = select! { Token::Ident(s) if s == "if" => () }
            .ignore_then(
                prop.clone()
                    .then_ignore(just(Token::Comma))
                    .then(value.clone())
                    .then_ignore(just(Token::Comma))
                    .then(value.clone())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|((when, then), otherwise)| ValueExpr::Cond {
                when: Box::new(when),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
            });

        let primary = choice((
            sum_expr,
            min_max_expr,
            abs_expr,
            round_expr,
            if_expr,
            value_lookup,
            parenthesised,
            decimal_as_value,
            timestamp_as_value,
            date_as_value,
            // Before bare idents: `duration(...)` / `span(...)` are
            // the constructors.
            duration_as_value,
            span_as_value,
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
/// directly. The ordered comparators all lower to `Prop::Compare`,
/// so they carry their `(op, domain)` pair from the token choice -
/// the one place the comparator vocabulary is spelled out.
#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Neq,
    /// An ordered comparison with its domain picked by surface
    /// keyword (`<=` decimal, `on_or_before` date, `at_or_before`
    /// timestamp, `no_longer_than` duration...), never by operand
    /// kind. Operand kinds are checked at runtime against the domain.
    Compare(CompareOp, OrderedDomain),
    /// Membership comparator (`x in xs`) -> `Prop::In(Term, Term)`,
    /// term-only on both sides (the IR's `In` operates on terms).
    /// Distinct from the structural `in` in `forall x in source: body`.
    In,
}

/// A contextual comparator: `word` reads as an ordered comparison in
/// comparator position only, staying usable as an ordinary identifier
/// everywhere else.
fn contextual_cmp<'a, I>(
    word: &'static str,
    op: CompareOp,
    domain: OrderedDomain,
) -> impl Parser<'a, I, CmpOp, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    select! { Token::Ident(s) if s == word => CmpOp::Compare(op, domain) }
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

/// Which shape a `min` / `max` call turned out to be, decided by the
/// separator after its first operand.
enum MinMaxShape {
    Aggregate((Term, Prop)),
    Binary((ValueExpr, ValueExpr)),
}
