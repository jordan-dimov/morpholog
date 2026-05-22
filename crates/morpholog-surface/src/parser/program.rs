//! Programme-level parsing: top-level header + predicate and
//! invariant declarations.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{Invariant, PredicateArgDecl, PredicateDecl, Program};
use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, Span};
use crate::lexer::{Token, lex, token_stream};

use super::expr::expression_parser;

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
/// invariants can be freely interleaved (e.g. `predicate Foo /
/// invariant cap over Foo / predicate Bar`); the parser partitions
/// them into the `RawProgram` vectors after collection, preserving
/// source order within each category. This shape avoids committing
/// the language to an "all predicates first" file convention.
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
    // them into a single sequence and partitions on the post-pass
    // (source order preserved within each category).
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
