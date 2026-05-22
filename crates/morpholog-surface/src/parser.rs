//! Parser for the v0 surface fragment.
//!
//! Grammar (BNF, P1 + P2a + P2b-lite):
//!
//! ```text
//! program        ::= program_header predicate_decl*
//! program_header ::= "program" Ident
//! predicate_decl ::= "predicate" Ident "(" arg_list? ")"
//! arg_list       ::= arg ("," arg)* ","?
//! arg            ::= Ident ":" Kind
//! Kind           ::= "Subject" | "Decimal" | "Date" | "Bool" | "Collection" | "Any"
//!
//! expression     ::= quantifier | implies
//! quantifier     ::= "exists" Ident ":" expression
//!                  | "forall" Ident "in" primary ":" expression
//! implies        ::= and ("implies" implies)?
//! and            ::= not_expr ("and" not_expr)*
//! not_expr       ::= "not" not_expr | comparison
//! comparison     ::= arith (cmp_op arith)?
//! cmp_op         ::= "=" | "!=" | "<=" | "in"
//! arith          ::= primary (("+" | "-") primary)*
//! primary        ::= "(" expression ")"
//!                  | sum_expr | value_expr
//!                  | DecimalLit | DateLit | SubjectLit
//!                  | "_"
//!                  | Ident "(" term_list ")"           -- claim call (args optional)
//!                  | Ident                             -- variable | actor
//! sum_expr       ::= "sum" "(" Ident "|" expression ")"
//! value_expr     ::= "value" Ident "(" term_list ")" ("default" expression)?
//! term_list      ::= (term ("," term)* ","?)?         -- zero or more terms
//! term           ::= Ident | "_" | DecimalLit | DateLit | SubjectLit
//! ```
//!
//! Newlines are insignificant. Trailing commas in argument lists
//! are allowed. Comments are stripped at the lexer; the parser
//! never sees them.
//!
//! Asymmetry to honour: `Expr::Neq(Term, Term)` and `Expr::In(Term,
//! Term)` operate on *terms*, not expressions. `Eq`, `Le`, `Sub`,
//! `Add` operate on full expressions. The parser must therefore
//! reject `Foo + 1 != Bar` because both `!=` operands must be
//! terms, not arithmetic expressions, and the same holds for
//! membership: `a + 1 in xs` is rejected. Similarly, claim-call
//! arguments are terms only - `Foo(x + 1, y)` is rejected. These
//! constraints follow directly from the IR shape under the
//! `docs/scope-and-ambition.md` surface doctrine.
//!
//! P2b-lite added bounded forms (`exists`, `forall`, `sum`,
//! `value`), the membership comparator (`in`), and the date /
//! subject literal sigils (`@YYYY-MM-DD`, `#NAME`). Two
//! disambiguation rules govern them:
//!
//! - The `in` keyword is structural inside `forall <ident> in
//!   <source>:` (consumed by the forall production before
//!   reaching comparator-level grammar) and a membership
//!   comparator everywhere else. Positional disambiguation; no
//!   context-sensitive parsing.
//! - `forall x in source: body` accepts the source at `primary`
//!   precedence (variables, claim calls, parenthesised
//!   expressions, primary-shaped literals). When the source is
//!   a bare Term-wrapper (`Expr::Term(_)`), the parser auto-lifts
//!   it to `Expr::In(Var(binding), source_term)` so the kernel
//!   can iterate; when it's already predicate-shaped (a claim
//!   call), it passes through unchanged.
//!
//! Error recovery: humble. On a parse failure inside a predicate
//! declaration, the parser skips forward to the next `predicate`
//! keyword (or EOF) and continues. The intent is "tell the author
//! about every malformed declaration in one parse run", not a
//! full-language error-recovery framework. The `program` keyword
//! is not a recovery sync point in P1 because the grammar permits
//! exactly one `program` header at the file start; a second one
//! would be a separate, recoverable-only-by-restart kind of error.
//! Expression parsing in P2a does not yet add recovery shapes; a
//! malformed expression surfaces as one diagnostic at the failure
//! site, sufficient for P2a's scope.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{Expr, Invariant, PredicateArgDecl, PredicateDecl, Program, Term, Value};
use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, Span};
use crate::lexer::{Token, lex, token_stream};

/// Parse a Morpholog source string into a [`Program`].
///
/// Returns `Ok(program)` when no diagnostics fire, even if the
/// program is structurally minimal (e.g. just a `program` header
/// with no predicates).
///
/// Returns `Err(diagnostics)` with one or more diagnostics on any
/// lex or parse failure. Diagnostics carry byte-offset spans; the
/// CLI renders them via `ariadne` against the original source.
pub fn parse_program(source: &str) -> Result<Program, Vec<Diagnostic>> {
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
        // Span 0..1 (or 0..0 for a zero-length source) is the
        // closest we can point at "the start"; the empty-file case
        // doesn't have a more specific location.
        let end = source.len().min(1);
        return Err(vec![Diagnostic::error(
            "expected `program` header, found empty file",
            0..end,
        )]);
    }

    let stream = token_stream(&tokens);
    let (parsed, errs) = program_parser().parse(stream).into_output_errors();

    let mut diagnostics: Vec<Diagnostic> = errs
        .into_iter()
        .map(|e| {
            let span = e.span();
            Diagnostic::error(
                format!("parse error: {}", e.reason()),
                span.start()..span.end(),
            )
        })
        .collect();

    let Some(raw) = parsed else {
        if diagnostics.is_empty() {
            diagnostics.push(Diagnostic::error("parse failed", 0..source.len()));
        }
        return Err(diagnostics);
    };

    // Build the final Program and run the duplicate-name checks
    // here on the parser side so the diagnostics carry source
    // spans for BOTH declarations. `Program::validate` also
    // detects duplicate predicate declarations but loses span
    // context; invariant-name duplication is not validated
    // kernel-side at all (no current example forces name-based
    // invariant lookup), so the parser is the only place it gets
    // caught.
    let mut pred_by_name: HashMap<&str, &Span> = HashMap::new();
    for (decl, span) in &raw.predicates {
        if let Some(first_span) = pred_by_name.get(decl.name.as_str()) {
            diagnostics.push(
                Diagnostic::error(
                    format!("duplicate predicate declaration `{}`", decl.name),
                    span.clone(),
                )
                .with_secondary((*first_span).clone(), "previously declared here"),
            );
        } else {
            pred_by_name.insert(decl.name.as_str(), span);
        }
    }
    let mut inv_by_name: HashMap<&str, &Span> = HashMap::new();
    for (inv, span) in &raw.invariants {
        if let Some(first_span) = inv_by_name.get(inv.name.as_str()) {
            diagnostics.push(
                Diagnostic::error(
                    format!("duplicate invariant declaration `{}`", inv.name),
                    span.clone(),
                )
                .with_secondary((*first_span).clone(), "previously declared here"),
            );
        } else {
            inv_by_name.insert(inv.name.as_str(), span);
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    Ok(Program {
        name: raw.name,
        predicates: raw.predicates.into_iter().map(|(d, _)| d).collect(),
        invariants: raw.invariants.into_iter().map(|(i, _)| i).collect(),
        transformations: Vec::new(),
        derived_claims: Vec::new(),
    })
}

/// Intermediate parse result. Carries spans alongside the parsed
/// values so the post-pass (duplicate detection) can produce
/// span-rich diagnostics. The final `Program` strips spans because
/// the kernel IR is source-agnostic.
#[derive(Debug)]
struct RawProgram {
    name: String,
    predicates: Vec<(PredicateDecl, Span)>,
    invariants: Vec<(Invariant, Span)>,
}

/// One top-level declaration in a programme body. Predicates and
/// invariants can be freely interleaved (e.g. `predicate Foo / invariant cap
/// over Foo / predicate Bar`); the parser sorts them into the
/// `RawProgram` vectors after collection. This shape avoids
/// committing the language to an "all predicates first" file
/// convention.
enum TopLevelDecl {
    Predicate(PredicateDecl, Span),
    Invariant(Invariant, Span),
}

fn program_parser<'a, I>() -> impl Parser<'a, I, RawProgram, extra::Err<Rich<'a, Token>>>
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };
    let kind = select! { Token::Kind(k) => k };

    // arg ::= Ident ":" Kind
    let arg = ident
        .then_ignore(just(Token::Colon))
        .then(kind)
        .map(|(name, kind)| PredicateArgDecl { name, kind });

    // arg_list ::= arg ("," arg)* ","?
    let arg_list = arg
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<PredicateArgDecl>>();

    // predicate_decl ::= "predicate" Ident "(" arg_list? ")"
    let predicate_decl = just(Token::KwPredicate)
        .ignore_then(ident)
        .then(arg_list.delimited_by(just(Token::LParen), just(Token::RParen)))
        .map_with(|(name, args), e| {
            let span: SimpleSpan = e.span();
            TopLevelDecl::Predicate(PredicateDecl { name, args }, span.start()..span.end())
        });

    // invariant_decl ::= "invariant" Ident ":" expression
    //
    // No version syntax in v0: the version field defaults to 1.
    // When versioning grows a second meaningful value, the surface
    // adds a clause (e.g. `version <N>`) and the parser starts
    // accepting it. Today, an attempted `invariant Name (v1):`
    // surfaces as an unexpected-token diagnostic on the `(`.
    let invariant_decl = just(Token::KwInvariant)
        .ignore_then(ident)
        .then_ignore(just(Token::Colon))
        .then(expression_parser())
        .map_with(|(name, body), e| {
            let span: SimpleSpan = e.span();
            TopLevelDecl::Invariant(
                Invariant {
                    name,
                    version: 1,
                    body,
                },
                span.start()..span.end(),
            )
        });

    // top_level_decl ::= predicate_decl | invariant_decl
    //
    // Free interleaving: a programme may mix predicate and
    // invariant declarations in any order. The parser collects
    // them into a single sequence and sorts on the post-pass.
    let top_level_decl = choice((predicate_decl, invariant_decl));

    // Sync at the next `predicate` or `invariant` keyword on
    // failure; skip the rest of the malformed declaration but
    // keep the declarations on either side of it.
    let top_level_recovering = top_level_decl.recover_with(skip_then_retry_until(
        any().ignored(),
        just(Token::KwPredicate)
            .ignored()
            .or(just(Token::KwInvariant).ignored())
            .or(end()),
    ));

    // program_header ::= "program" Ident
    let header = just(Token::KwProgram).ignore_then(ident);

    header
        .then(top_level_recovering.repeated().collect::<Vec<_>>())
        .then_ignore(end())
        .map(|(name, decls)| {
            let mut predicates = Vec::new();
            let mut invariants = Vec::new();
            for d in decls {
                match d {
                    TopLevelDecl::Predicate(p, s) => predicates.push((p, s)),
                    TopLevelDecl::Invariant(i, s) => invariants.push((i, s)),
                }
            }
            RawProgram {
                name,
                predicates,
                invariants,
            }
        })
}

// ============================================================
// Expressions (P2a)
// ============================================================

/// Parse a Morpholog expression into [`Expr`]. Used by tests for
/// P2a and (later) by invariant/transformation parsers when the
/// rest of the surface lands.
///
/// Same error model as [`parse_program`]: returns a list of
/// diagnostics on failure, each carrying a byte-offset span
/// suitable for rendering with `ariadne`.
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

/// Build the recursive expression parser. The grammar is laid out
/// in increasing precedence: implies (lowest) wraps and, which
/// wraps not, which wraps comparison, which wraps arith, which
/// wraps primary (highest).
///
/// `recursive` lets `primary` reference `expression` so parenthesised
/// sub-expressions can nest arbitrarily.
fn expression_parser<'a, I>() -> impl Parser<'a, I, Expr, extra::Err<Rich<'a, Token>>>
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    recursive(|expression| {
        let ident = select! { Token::Ident(s) => s };
        let decimal_lit = select! { Token::DecimalLit(s) => s };

        let date_lit = select! { Token::DateLit(s) => s };
        let subject_lit = select! { Token::SubjectLit(s) => s };

        // A `Term` is the limited atom that claim-call args and
        // `Neq` / `In` operands accept. Variables (including the
        // special `actor`), wildcards, decimal / date / subject
        // literals.
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

        // term_list inside claim calls.
        let term_list = term
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<Term>>();

        // A `primary` Expr: parens, decimal-literal-as-Term-as-Expr,
        // wildcard-as-Term-as-Expr, claim call, or bare variable.
        //
        // The ident-vs-claim-call ambiguity is resolved by peeking
        // for a following `(`. `ident.then(args.or_not())` does
        // this in chumsky.
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
        // Target is restricted to a non-reserved variable in v0.
        // Literals, wildcards, and `actor` in target position are
        // rejected with a clean diagnostic. The literal/wildcard
        // case fails earlier because `ident` only matches
        // Token::Ident; the `actor` case must be caught here
        // because the lexer treats it as a plain identifier.
        // Generalised target lands when a worked example forces it.
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

        let primary = choice((
            sum_expr,
            value_expr,
            parenthesised,
            decimal_as_expr,
            date_as_expr,
            subject_as_expr,
            wildcard_as_expr,
            ident_or_call,
        ));

        // arith ::= primary (("+" | "-") primary)*  (left-assoc)
        //
        // foldl builds the left-associative tree: a + b + c becomes
        // Add(Add(a, b), c).
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
        // Four forms (P2a + P2b-lite):
        //   - `=` -> Expr::Eq(Expr, Expr)
        //   - `<=` -> Expr::Le(Expr, Expr)
        //   - `!=` -> Expr::Neq(Term, Term)  - requires both sides to be Terms
        //   - `in` (P2b) -> Expr::In(Term, Term)  - requires both sides to be Terms
        //
        // For `!=` and `in`, we accept any Expr on either side and
        // then require it to be a bare `Expr::Term(t)`; otherwise
        // emit a clean diagnostic about the term-only restriction.
        // The `in` here is the membership comparator; the
        // structural `in` of `forall x in source:` is consumed by
        // the forall production before reaching this level.
        let comparison = arith.clone().then(
            choice((
                just(Token::Eq).to(CmpOp::Eq),
                just(Token::Neq).to(CmpOp::Neq),
                just(Token::Le).to(CmpOp::Le),
                just(Token::KwIn).to(CmpOp::In),
            ))
            .then(arith.clone())
            .or_not(),
        ).validate(|(lhs, rhs_opt), e, emitter| {
            match rhs_opt {
                None => lhs,
                Some((CmpOp::Eq, rhs)) => Expr::Eq(Box::new(lhs), Box::new(rhs)),
                Some((CmpOp::Le, rhs)) => Expr::Le(Box::new(lhs), Box::new(rhs)),
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

        // implies ::= and ("implies" implies)?  (right-assoc)
        let implies_expr = and_expr
            .clone()
            .then(
                just(Token::KwImplies)
                    .ignore_then(
                        and_expr.clone().then(
                            // Allow chained `implies` via recursion: a implies b implies c.
                            just(Token::KwImplies)
                                .ignore_then(and_expr)
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
        // Quantifiers sit at the very top of the expression
        // grammar - higher than `implies` - so their bodies
        // greedily consume the rest of the expression after the
        // colon. This matches mathematical convention: in
        // `forall x in xs: A and B`, the body is `A and B` (the
        // whole conjunction), not just `A`. Composition with
        // outer expressions is achieved by parenthesising the
        // quantifier: `(forall x in xs: body) and outer`.
        //
        // The source clause of `forall x in source: body` parses
        // the source at the `primary` level. This is restrictive
        // (a bare ident, claim call, parenthesised expression, or
        // primary-shaped literal works; arithmetic does not) but
        // matches every existing worked-example usage and avoids
        // ambiguity with the comparator-level `in`.
        //
        // When the source is a Term-shaped primary (a bare
        // variable, a literal), the parser auto-wraps it in
        // `Expr::In(Var(binding), source_term)` because the
        // kernel's Forall requires its source to be predicate-
        // shaped. A claim-call source is already predicate-shaped
        // and used as-is.
        let exists_expr = just(Token::KwExists)
            .ignore_then(ident)
            .then_ignore(just(Token::Colon))
            .then(expression.clone())
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
        // The kernel's `Expr::Forall { source, .. }` requires the
        // source to be predicate-shaped so `find_matches` can
        // produce binding extensions. The general `primary` parser
        // accepts value-shaped forms (sum, value, decimal/date/
        // subject literals, wildcards) that would let surface
        // expressions like `forall x in 5: P(x)` parse and then
        // produce ill-shaped IR. Per the surface doctrine, the
        // parser must refuse what the kernel cannot evaluate.
        //
        // Accepted unparenthesised sources:
        //   - bare variable (Ident with no `(`)       -> auto-lift to In
        //   - claim call (Ident "(" terms ")")         -> use as-is
        //   - parenthesised expression                 -> use as-is (user took responsibility)
        //
        // Value-shaped primaries (decimal/date/subject literals,
        // wildcards, `sum(...)`, `value Foo(...)`) are NOT in the
        // unparenthesised source grammar; they surface as parse
        // errors. The user can still write `(sum(...))` with parens
        // and the parser will pass it through - that's a choice
        // they explicitly signalled.
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
            .then(expression.clone())
            .validate(|((binding, source), body): ((String, Expr), Expr), e, emitter| {
                if binding == "actor" {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        "`actor` cannot be a quantifier binder: `actor` is reserved as the special term that resolves to the proposing transition's actor; references inside the body would resolve to that term, not the bound variable",
                    ));
                }
                // If the source is a bare Term wrapper (a variable
                // or `actor`), lift it to an In-expression that
                // binds the variable. If it's already predicate-
                // shaped (Claim from a call, or anything the user
                // wrapped in parens), use it as-is.
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

/// Discriminator for the three comparison operators. Internal to
/// the parser; the surface uses `=`, `!=`, `<=` directly.
#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Neq,
    Le,
    /// Membership comparator (`x in xs`). Lowered to
    /// `Expr::In(Term, Term)` with the same term-only restriction
    /// as `Neq`. Distinct from the structural `in` in
    /// `forall x in source: body`.
    In,
}

/// Convert an `Expr` to a `Term` if it's term-shaped (a bare
/// `Expr::Term(_)` wrapper). Returns `None` for any compound
/// expression. Used to enforce the IR's term-only restriction on
/// `Neq` operands.
fn expr_as_term(e: &Expr) -> Option<Term> {
    match e {
        Expr::Term(t) => Some(t.clone()),
        _ => None,
    }
}
