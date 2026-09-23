//! Expression parsing: propositions ([`Prop`], via `parse_expression`) and value expressions
//! ([`ValueExpr`], via `parse_value_expr`).
//!
//! The two grammars refer to each other: a comparison relates two values, and a `sum` ranges
//! over a proposition. Invariant and `require` bodies are propositions; values only appear
//! nested inside something else.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{
    ArithOp, Builtin, CompareOp, ExtremumOp, OrderedDomain, Prop, SumSeed, Term, Unit, Value,
    ValueExpr,
};

use super::field_table::{FieldTable, Vocabulary, resolve_named, resolve_named_value};

/// The `duration(PT6H)` constructor: an ISO 8601 duration in exact time units. The payload lexes
/// as an identifier because it starts with `P`. A malformed one is a parse error with a span.
/// `duration` is only special before `(`, so it stays usable as a variable name.
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

/// The `span(P3M)` constructor: a calendar span in date units (Y/M/W/D), shaped like
/// `duration(...)`. Validated with the kernel's own `morpholog_core::calendar` grammar so the
/// two cannot disagree. Only valid as a value: a span shifts a date and is never a claim or intent
/// argument, so `term_parser` leaves it out.
pub(super) fn span_ctor<'a, I>() -> impl Parser<'a, I, String, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let payload = choice((
        select! { Token::Ident(s) => s },
        // Accept `span(3)` here so the diagnostic can name the fix.
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

/// Build a `Prop::Compare`. The inverse is `format::compare_token`.
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

/// Parse a standalone proposition, such as an invariant or `require` body.
pub fn parse_expression(source: &str) -> Result<Prop, Vec<Diagnostic>> {
    let tokens = lex_or_diagnostics(source)?;
    // No programme, so no declared fields: a named pattern is refused as undeclared.
    let table = FieldTable::empty();
    let stream = token_stream(&tokens);
    let (parsed, errs) = expression_parser(&table)
        .then_ignore(end())
        .parse(stream)
        .into_output_errors();
    finish(parsed, errs, source)
}

/// Parse a standalone value expression. In a programme values only appear nested.
pub fn parse_value_expr(source: &str) -> Result<ValueExpr, Vec<Diagnostic>> {
    let tokens = lex_or_diagnostics(source)?;
    let table = FieldTable::empty();
    let stream = token_stream(&tokens);
    let (parsed, errs) = value_expr_parser(&table)
        .then_ignore(end())
        .parse(stream)
        .into_output_errors();
    finish(parsed, errs, source)
}

/// Lex `source`, turning a lex failure or empty input into diagnostics.
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

/// Turn a chumsky parse result into `Result<T, Vec<Diagnostic>>`.
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

/// A number with an optional unit: `25000 USD` is a quantity, `25000` a decimal. A contextual
/// comparator after a bare number reads as a unit, but that expression was ill-typed anyway.
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

/// A term: the only thing claim-call arguments and `in` operands accept. Variables, `actor`,
/// wildcards, and literals.
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
        // Before bare idents, so `duration(...)` is the constructor.
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

/// One item of a claim-pattern argument list: a positional term, a
/// named `field: term` entry, or the `..` rest-marker.
enum PatternItem {
    Pos(Term),
    Named(SimpleSpan, String, Term),
    Rest(SimpleSpan),
}

/// The two valid pattern-argument shapes. Shape rules (no mixing, no duplicate field, `..`
/// last) are checked here; mapping names to positions needs the declarations, so the enclosing
/// statement or expression does it.
pub(super) enum PatternArgs {
    Positional(Vec<Term>),
    Named {
        entries: Vec<(SimpleSpan, String, Term)>,
        rest: bool,
    },
}

/// An argument list that is either positional terms or named
/// `field: term` entries with an optional final `..`.
pub(super) fn pattern_args_parser<'a, I>()
-> impl Parser<'a, I, PatternArgs, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };
    let named_entry = ident
        .map_with(|name, e| (e.span(), name))
        .then_ignore(just(Token::Colon))
        .then(term_parser())
        .map(|((span, name), term)| PatternItem::Named(span, name, term));
    let rest = just(Token::DotDot).map_with(|_, e| PatternItem::Rest(e.span()));
    choice((named_entry, rest, term_parser().map(PatternItem::Pos)))
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<PatternItem>>()
        .validate(|items, e, emitter| {
            classify_pattern_items(items, e.span(), &mut |span, message| {
                emitter.emit(Rich::custom(span, message));
            })
        })
}

/// Sort collected pattern items into one of the two lawful shapes,
/// refusing the shapes that are neither.
fn classify_pattern_items(
    items: Vec<PatternItem>,
    call_span: SimpleSpan,
    refuse: &mut dyn FnMut(SimpleSpan, String),
) -> PatternArgs {
    let has_named = items.iter().any(|i| matches!(i, PatternItem::Named(..)));
    let has_rest = items.iter().any(|i| matches!(i, PatternItem::Rest(_)));
    if !has_named && !has_rest {
        return PatternArgs::Positional(
            items
                .into_iter()
                .filter_map(|i| match i {
                    PatternItem::Pos(t) => Some(t),
                    _ => None,
                })
                .collect(),
        );
    }
    let last = items.len().saturating_sub(1);
    let mut entries: Vec<(SimpleSpan, String, Term)> = Vec::new();
    let mut rest = false;
    let mut mixed = false;
    for (i, item) in items.into_iter().enumerate() {
        match item {
            PatternItem::Named(span, name, term) => {
                if entries.iter().any(|(_, n, _)| *n == name) {
                    refuse(
                        span,
                        format!("field `{name}` is named twice in this pattern"),
                    );
                } else {
                    entries.push((span, name, term));
                }
            }
            PatternItem::Rest(span) => {
                if rest {
                    refuse(span, "one `..` is enough".to_string());
                } else if i != last {
                    refuse(span, "`..` closes a named pattern; put it last".to_string());
                }
                rest = true;
            }
            PatternItem::Pos(_) => mixed = true,
        }
    }
    if mixed {
        let message = if has_named {
            "a pattern is all-named or all-positional, never mixed"
        } else {
            "`..` belongs to a named pattern; name the fields you match"
        };
        refuse(call_span, message.to_string());
    }
    PatternArgs::Named { entries, rest }
}

/// Resolve pattern arguments to positional terms. Named arguments are looked up in `vocabulary`;
/// on failure the refusals are emitted and the terms are returned in written order, only to keep
/// the already-failing parse going.
pub(super) fn resolve_pattern(
    head: &str,
    args: PatternArgs,
    vocabulary: Vocabulary,
    table: &FieldTable,
    call_span: SimpleSpan,
    refuse: &mut dyn FnMut(SimpleSpan, String),
) -> Vec<Term> {
    match args {
        PatternArgs::Positional(terms) => terms,
        PatternArgs::Named { entries, rest } => {
            match resolve_named(head, &entries, rest, vocabulary, table, call_span) {
                Ok(terms) => terms,
                Err(refusals) => {
                    for (span, message) in refusals {
                        refuse(span, message);
                    }
                    entries.into_iter().map(|(_, _, term)| term).collect()
                }
            }
        }
    }
}

/// Build the recursive proposition parser. Precedence from loosest to tightest: quantifiers,
/// `implies`, `or`, `xor`, `and`, `not`, comparisons. The value grammar is built inside it,
/// since the two refer to each other.
pub(super) fn expression_parser<'a, I>(
    table: &'a FieldTable,
) -> impl Parser<'a, I, Prop, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    recursive(move |expression| {
        let ident = select! { Token::Ident(s) => s };

        let arith = value_arith_parser(expression.clone(), table);

        // A claim call needs parentheses; a bare identifier is a variable.
        let claim_call = ident
            .then(pattern_args_parser().delimited_by(just(Token::LParen), just(Token::RParen)))
            .validate(move |(name, args), e, emitter| {
                let args = resolve_pattern(
                    &name,
                    args,
                    Vocabulary::ClaimShaped,
                    table,
                    e.span(),
                    &mut |span, message| emitter.emit(Rich::custom(span, message)),
                );
                Prop::Claim {
                    predicate: name.into(),
                    args,
                }
            });

        // `pre(<prop>)`: evaluate the inner proposition against the state before the transition.
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

        // comparison ::= arith (cmp_op arith)+
        //
        // A bare value is not a proposition, so a comparator is required. `in` accepts any value
        // here, then insists both sides are terms, for a clearer error.
        //
        // `0 <= rate <= 1` chains into an `and` of pairwise comparisons. Only ordered comparators
        // chain, all pointing the same way; a mixed chain is refused rather than guessed at.
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
                    // Contextual: identifiers everywhere else, so `before` and `after` stay
                    // usable as variable names.
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

        // Value comparison first, so `amount <= 100` is not tried as a claim call.
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

        // and_expr ::= not_expr ("and" not_expr)*, flattened into one Prop::And
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
                    // Flatten nested Ands, so a chained comparison or parenthesised
                    // conjunction gives the same IR as its spelled-out form.
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

        // xor_expr ::= and_expr ("xor" and_expr)*
        //
        // `a and b xor c and d` reads as "exactly one of these two". Xor is binary, so a chain
        // nests to the left: `Xor(Xor(a, b), c)`.
        let xor_expr = and_expr.clone().foldl(
            just(Token::KwXor).ignore_then(and_expr.clone()).repeated(),
            |left, right| Prop::Xor(Box::new(left), Box::new(right)),
        );

        // or_expr ::= xor_expr ("or" xor_expr)*, flattened into one Prop::Or
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
                            just(Token::KwImplies)
                                .ignore_then(or_expr)
                                .repeated()
                                .collect::<Vec<Prop>>(),
                        ),
                    )
                    .or_not(),
            )
            .map(|(first, rest_opt)| match rest_opt {
                None => first,
                Some((second, more)) => {
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
            });

        // expression ::= quantifier | implies
        //
        // A quantifier body runs to the end of the expression: in `forall x in xs: A and B` it is
        // the whole conjunction. Parenthesise to combine: `(forall x in xs: body) and outer`.
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

        // The source of `forall x in <source>:` must be something the kernel can enumerate
        // bindings from: a bare variable (lifted to `x in var`), a claim call, or a parenthesised
        // proposition. Literals, `sum` and `value` are parse errors.
        let forall_bare_source = ident.then(
            pattern_args_parser()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        );
        let forall_source = choice((
            expression
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map(ForallSource::Prop),
            forall_bare_source.validate(move |(name, args), e, emitter| match args {
                Some(args) => {
                    let args = resolve_pattern(
                        &name,
                        args,
                        Vocabulary::ClaimShaped,
                        table,
                        e.span(),
                        &mut |span, message| emitter.emit(Rich::custom(span, message)),
                    );
                    ForallSource::Prop(Prop::Claim {
                        predicate: name.into(),
                        args,
                    })
                }
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

/// A parsed `forall` source. A bare term becomes `In(Var(binding), term)`; a proposition is
/// used as it is.
enum ForallSource {
    BareTerm(Term),
    Prop(Prop),
}

/// Build the value-expression parser on its own, with its own proposition parser for `sum`
/// bodies. [`expression_parser`] builds the same grammar inside itself.
pub(super) fn value_expr_parser<'a, I>(
    table: &'a FieldTable,
) -> impl Parser<'a, I, ValueExpr, extra::Err<Rich<'a, Token>>> + Clone
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    value_arith_parser(expression_parser(table), table)
}

/// Build the value-expression grammar, with `prop` parsing the propositions nested inside it
/// (`sum`, `min`/`max` aggregate and `if` bodies). Shared by [`expression_parser`] and
/// [`value_expr_parser`].
fn value_arith_parser<'a, I, P>(
    prop: P,
    table: &'a FieldTable,
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

        // A variable or `actor`. `Foo(args)` is a claim, which only the proposition grammar takes.
        let bare_ident = ident.map(|name| {
            if name == "actor" {
                ValueExpr::Term(Term::Actor)
            } else {
                ValueExpr::Term(Term::Var(name.into()))
            }
        });

        // `sum(<target> | <prop>)`. The target is any value over the body's bindings:
        // `sum(amount | ...)`, `sum(1 | ...)` to count, `sum(probability * loss | ...)`. The `|`
        // is unambiguous because values only use it inside an aggregate's own parentheses.
        let sum_expr = just(Token::KwSum)
            .ignore_then(
                value
                    .clone()
                    .then_ignore(just(Token::Pipe))
                    .then(prop.clone())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .validate(|(target, body): (ValueExpr, Prop), e, emitter| {
                if matches!(&target, ValueExpr::Term(Term::Actor)) {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a sum target: `actor` is reserved as the special term that resolves to the proposing transition's actor, not a regular variable",
                    ));
                }
                if matches!(&target, ValueExpr::Term(Term::Wildcard)) {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`_` cannot be a sum target: name the value the sum adds up",
                    ));
                }
                ValueExpr::Sum {
                    value: Box::new(target),
                    body: Box::new(body),
                    seed: SumSeed::default(),
                }
            });

        // `value Pred(args) [default <value>]` reads the field marked `_`. Positionally that is
        // the first wildcard; in named form exactly one `field: _`, and `..` skips the rest.
        let value_lookup = just(Token::KwValue)
            .ignore_then(ident)
            .then(pattern_args_parser().delimited_by(just(Token::LParen), just(Token::RParen)))
            .then(just(Token::KwDefault).ignore_then(value.clone()).or_not())
            .validate(move |((predicate, args), default), e, emitter| {
                let (args, extract) = match args {
                    PatternArgs::Positional(terms) => {
                        let extract = terms
                            .iter()
                            .position(|t| matches!(t, Term::Wildcard))
                            .unwrap_or(terms.len());
                        (terms, extract)
                    }
                    PatternArgs::Named { entries, rest } => {
                        match resolve_named_value(&predicate, &entries, rest, table, e.span()) {
                            Ok(resolved) => resolved,
                            Err(refusals) => {
                                for (span, message) in refusals {
                                    emitter.emit(Rich::custom(span, message));
                                }
                                // Already failing; keep the written terms for later diagnostics.
                                let terms: Vec<Term> =
                                    entries.into_iter().map(|(_, _, term)| term).collect();
                                let extract = terms
                                    .iter()
                                    .position(|t| matches!(t, Term::Wildcard))
                                    .unwrap_or(terms.len());
                                (terms, extract)
                            }
                        }
                    }
                };
                ValueExpr::ValueOf {
                    predicate: predicate.into(),
                    args,
                    extract,
                    default: default.map(Box::new),
                }
            });

        // `min(a, b)` compares two values; `min(x | body)` aggregates over the body's bindings.
        // The separator after the first operand decides. Unlike `sum`, the aggregate target is
        // only a variable or literal: nothing has needed more.
        let extremum_target = choice((
            ident.map(|name| Term::Var(name.into())),
            decimal_or_quantity_term(),
        ));
        let extremum_body = extremum_target
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
            MinMaxShape::Binary((lhs, rhs)) => ValueExpr::Call {
                builtin: match op {
                    ExtremumOp::Min => Builtin::Min,
                    ExtremumOp::Max => Builtin::Max,
                },
                args: vec![lhs, rhs],
            },
        });

        // `abs(<value>)`, as in the two-sided bound `abs(x) <= limit`.
        let abs_expr = just(Token::KwAbs)
            .ignore_then(
                value
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|operand| ValueExpr::Call {
                builtin: Builtin::Abs,
                args: vec![operand],
            });

        // `round(<value>, <quantum>)`: nearest multiple, halves away from zero, as in
        // `round(raw, 0.01)`.
        let round_expr = just(Token::KwRound)
            .ignore_then(
                value
                    .clone()
                    .then_ignore(just(Token::Comma))
                    .then(value.clone())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|(v, quantum)| ValueExpr::Call {
                builtin: Builtin::Round,
                args: vec![v, quantum],
            });

        // `if(<prop>, <then>, <else>)`. Written as a call, so it needs no precedence rules and
        // `if` stays a legal variable name.
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

        // `period_index(anchor, span, date)` and its inverse `period_start_of(anchor, span,
        // index)`. Contextual, like `if`.
        let period_builtin_expr = select! {
            Token::Ident(s) if s == "period_index" => Builtin::PeriodIndex,
            Token::Ident(s) if s == "period_start_of" => Builtin::PeriodStartOf,
        }
        .then(
            value
                .clone()
                .then_ignore(just(Token::Comma))
                .then(value.clone())
                .then_ignore(just(Token::Comma))
                .then(value.clone())
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|(builtin, ((first, second), third))| ValueExpr::Call {
            builtin,
            args: vec![first, second, third],
        });

        let primary = choice((
            sum_expr,
            min_max_expr,
            abs_expr,
            round_expr,
            if_expr,
            period_builtin_expr,
            value_lookup,
            parenthesised,
            decimal_as_value,
            timestamp_as_value,
            date_as_value,
            // Before bare idents, so these are the constructors.
            duration_as_value,
            span_as_value,
            subject_as_value,
            wildcard_as_value,
            bare_ident,
        ));

        // factor ::= primary (("*" | "/" | "%") primary)*  (left-assoc, tighter than `+`/`-`)
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

/// A parsed comparison operator.
#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Neq,
    /// An ordered comparison. The word picks the domain (`<=` decimal, `on_or_before` date,
    /// `at_or_before` timestamp, `no_longer_than` duration), never the operand kinds.
    Compare(CompareOp, OrderedDomain),
    /// Membership, `x in xs`. Both sides must be terms.
    In,
}

/// A comparator word that is an ordinary identifier outside comparator position.
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

/// The term inside a `ValueExpr::Term`, or `None` for anything compound.
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
