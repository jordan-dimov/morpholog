//! Parser for the v0 surface fragment.
//!
//! Grammar (BNF, P1 + P2a):
//!
//! ```text
//! program        ::= program_header predicate_decl*
//! program_header ::= "program" Ident
//! predicate_decl ::= "predicate" Ident "(" arg_list? ")"
//! arg_list       ::= arg ("," arg)* ","?
//! arg            ::= Ident ":" Kind
//! Kind           ::= "Subject" | "Decimal" | "Date" | "Bool" | "Collection" | "Any"
//!
//! expression     ::= implies
//! implies        ::= and ("implies" implies)?
//! and            ::= not_expr ("and" not_expr)*
//! not_expr       ::= "not" not_expr | comparison
//! comparison     ::= arith (cmp_op arith)?
//! cmp_op         ::= "=" | "!=" | "<="
//! arith          ::= primary (("+" | "-") primary)*
//! primary        ::= "(" expression ")"
//!                  | DecimalLit
//!                  | "_"
//!                  | Ident "(" term_list ")"           -- claim call
//!                  | Ident                             -- variable | actor
//! term_list      ::= term ("," term)* ","?
//! term           ::= Ident | "_" | DecimalLit         -- arg in claim call
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
//! terms, not arithmetic expressions. Similarly, claim-call
//! arguments are terms only - `Foo(x + 1, y)` is rejected. These
//! constraints follow directly from the IR shape under the
//! `docs/scope-and-ambition.md` surface doctrine.
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
use morpholog_core::{Expr, PredicateArgDecl, PredicateDecl, Program, Term, Value};
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

    // Build the final Program and run the duplicate-predicate check
    // here on the parser side so the diagnostic carries source spans
    // for BOTH declarations. `Program::validate` also detects this
    // but loses the span context.
    let mut by_name: HashMap<&str, &Span> = HashMap::new();
    for (decl, span) in &raw.predicates {
        if let Some(first_span) = by_name.get(decl.name.as_str()) {
            diagnostics.push(
                Diagnostic::error(
                    format!("duplicate predicate declaration `{}`", decl.name),
                    span.clone(),
                )
                .with_secondary((*first_span).clone(), "previously declared here"),
            );
        } else {
            by_name.insert(decl.name.as_str(), span);
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    Ok(Program {
        name: raw.name,
        predicates: raw.predicates.into_iter().map(|(d, _)| d).collect(),
        invariants: Vec::new(),
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
            (PredicateDecl { name, args }, span.start()..span.end())
        });

    // Sync at the next `predicate` or `program` keyword on failure;
    // skip the rest of the malformed declaration but keep the
    // declarations on either side of it.
    let predicate_decl_recovering = predicate_decl.recover_with(skip_then_retry_until(
        any().ignored(),
        just(Token::KwPredicate).ignored().or(end()),
    ));

    // program_header ::= "program" Ident
    let header = just(Token::KwProgram).ignore_then(ident);

    header
        .then(predicate_decl_recovering.repeated().collect::<Vec<_>>())
        .then_ignore(end())
        .map(|(name, predicates)| RawProgram { name, predicates })
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

        // A `Term` is the limited atom that claim-call args and
        // `Neq` / `In` operands accept. Variables (including the
        // special `actor`), wildcards, decimal literals.
        let term = choice((
            just(Token::Wildcard).to(Term::Wildcard),
            decimal_lit.map(|s| Term::Literal(Value::Decimal(s))),
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

        let primary = choice((
            parenthesised,
            decimal_as_expr,
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
        // Three forms:
        //   - `=` -> Expr::Eq(Expr, Expr)
        //   - `<=` -> Expr::Le(Expr, Expr)
        //   - `!=` -> Expr::Neq(Term, Term)  - requires both sides to be Terms
        //
        // For `!=`, we accept any Expr on either side and then
        // require it to be a bare `Expr::Term(t)`; otherwise emit
        // a clean diagnostic about the term-only restriction.
        let comparison = arith.clone().then(
            choice((
                just(Token::Eq).to(CmpOp::Eq),
                just(Token::Neq).to(CmpOp::Neq),
                just(Token::Le).to(CmpOp::Le),
            ))
            .then(arith.clone())
            .or_not(),
        ).validate(|(lhs, rhs_opt), e, emitter| {
            match rhs_opt {
                None => lhs,
                Some((CmpOp::Eq, rhs)) => Expr::Eq(Box::new(lhs), Box::new(rhs)),
                Some((CmpOp::Le, rhs)) => Expr::Le(Box::new(lhs), Box::new(rhs)),
                Some((CmpOp::Neq, rhs)) => {
                    // Pull the Term out of each side, or emit a
                    // diagnostic if either side is not Term-shaped.
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
                            // Return something to keep parsing
                            // going; the diagnostic above marks
                            // this as failed.
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
        and_expr
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
                        // `chain` always has at least two elements
                        // (first + second), so the reverse iterator
                        // is non-empty and the fold below cannot
                        // start from `None`.
                        let mut chain = vec![first, second];
                        chain.extend(more);
                        let mut iter = chain.into_iter().rev();
                        // SAFETY: chain.len() >= 2 by construction.
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
            })
    })
}

/// Discriminator for the three comparison operators. Internal to
/// the parser; the surface uses `=`, `!=`, `<=` directly.
#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    Neq,
    Le,
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
