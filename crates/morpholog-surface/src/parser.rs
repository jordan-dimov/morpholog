//! Parser for the v0 surface fragment.
//!
//! Grammar (BNF):
//!
//! ```text
//! program        ::= program_header predicate_decl*
//! program_header ::= "program" Ident
//! predicate_decl ::= "predicate" Ident "(" arg_list? ")"
//! arg_list       ::= arg ("," arg)* ","?
//! arg            ::= Ident ":" Kind
//! Kind           ::= "Subject" | "Decimal" | "Date" | "Bool" | "Collection" | "Any"
//! ```
//!
//! Newlines are insignificant. Trailing commas in argument lists
//! are allowed. Comments are stripped at the lexer; the parser
//! never sees them.
//!
//! Error recovery: humble. On a parse failure inside a predicate
//! declaration, the parser skips forward to the next `predicate` or
//! `program` keyword and continues. The intent is "tell the author
//! about every malformed declaration in one parse run", not a
//! full-language error-recovery framework.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{PredicateArgDecl, PredicateDecl, Program};
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
