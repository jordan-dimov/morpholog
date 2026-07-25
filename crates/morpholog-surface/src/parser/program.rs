//! Programme-level parsing: the `program` header plus every top-level
//! declaration - predicates (with their discipline clauses), intents,
//! definitions, invariants, transformations, and derived claims, freely
//! interleaved. After collection: duplicate-name diagnostics with spans
//! for both sites, definition-call resolution, and discipline lowering,
//! so the returned [`Program`] is the complete, enforceable IR.

use chumsky::input::ValueInput;
use chumsky::prelude::*;
use morpholog_core::{
    ArgDecl, Definition, DerivedClaim, DerivedValue, Discipline, IntentDecl, Invariant,
    InvariantOrigin, PredicateArgKind, PredicateDecl, Program, Transformation, Unit, Var,
};
use std::collections::{HashMap, HashSet};

use crate::diagnostics::{Diagnostic, Span};
use crate::lexer::{Token, lex, token_stream};
use crate::source_map::{DeclKind, SourceMap};

use super::expr::{expression_parser, value_expr_parser};
use super::stmt::statement_parser;

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
    parse_program_with_sources(source).map(|(program, _)| program)
}

/// [`parse_program`], keeping the source locations the parser already
/// knows. The returned [`SourceMap`] places every declaration (and
/// every top-level transformation-body statement) in the source, so
/// findings produced over the IR can be rendered with carets.
pub fn parse_program_with_sources(source: &str) -> Result<(Program, SourceMap), Vec<Diagnostic>> {
    let raw_tokens = lex(source).map_err(super::lex_error_diagnostics)?;

    if raw_tokens.is_empty() {
        // Span 0..1 (or 0..0 for a zero-length source) is the
        // closest we can point at "the start"; the empty-file case
        // doesn't have a more specific location.
        let end = source.len().min(1);
        return Err(vec![Diagnostic::error(
            "expected `program` header, found empty file",
            0..end,
        )]);
    }

    // Layout pass: enriches the token stream with virtual
    // Indent/Dedent at block boundaries. Transformation
    // bodies need this; predicate and invariant productions work
    // either way (the layout pass only inserts tokens where
    // indentation actually changes).
    let tokens = crate::layout::apply_layout(source, raw_tokens)?;

    let stream = token_stream(&tokens);
    let (parsed, errs) = program_parser().parse(stream).into_output_errors();

    let mut diagnostics = super::parse_error_diagnostics(errs);

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
    // context; invariant- and transformation-name duplication
    // are not validated kernel-side at all, so the parser is the
    // only place they get caught.
    report_duplicates(
        &mut diagnostics,
        "predicate",
        raw.predicates.iter().map(|(d, s)| (d.name.as_str(), s)),
    );
    report_duplicates(
        &mut diagnostics,
        "intent",
        raw.intents.iter().map(|(d, s)| (d.name.as_str(), s)),
    );
    report_duplicates(
        &mut diagnostics,
        "definition",
        raw.definitions.iter().map(|(d, s)| (d.name.as_str(), s)),
    );
    report_duplicates(
        &mut diagnostics,
        "invariant",
        raw.invariants.iter().map(|(i, s)| (i.name.as_str(), s)),
    );
    report_duplicates(
        &mut diagnostics,
        "transformation",
        raw.transformations
            .iter()
            .map(|(t, s, _)| (t.name.as_str(), s)),
    );
    report_duplicates(
        &mut diagnostics,
        "derived-claim",
        raw.derived_claims
            .iter()
            .map(|(d, s)| (d.predicate.as_str(), s)),
    );

    for (d, span) in &raw.derived_claims {
        // Duplicate key names inside a single derived declaration.
        // The IR's `keys` (a `Vec<Var>`) is positional; two same-named
        // keys would shadow each other in the binding context and
        // produce silently wrong enumeration.
        let mut seen_keys: HashSet<&str> = HashSet::new();
        for k in &d.keys {
            if !seen_keys.insert(k.as_str()) {
                diagnostics.push(Diagnostic::error(
                    format!("duplicate key `{}` in derived-claim `{}`", k, d.predicate),
                    span.clone(),
                ));
            }
        }

        // Duplicate value names inside a single derived declaration.
        // The IR's `values: Vec<DerivedValue>` is positional; two
        // same-named values would emit two output fields with the
        // same documentary name (the kernel doesn't enforce
        // uniqueness internally, but a derived claim with two `v`
        // outputs is a programmer error).
        let mut seen_values: HashSet<&str> = HashSet::new();
        for v in &d.values {
            if !seen_values.insert(v.name.as_str()) {
                diagnostics.push(Diagnostic::error(
                    format!(
                        "duplicate value name `{}` in derived-claim `{}`",
                        v.name, d.predicate
                    ),
                    span.clone(),
                ));
            }
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let mut map = SourceMap::new();
    for (d, s) in &raw.predicates {
        map.insert_decl(DeclKind::Predicate, d.name.as_str(), s.clone());
    }
    for (d, s) in &raw.intents {
        map.insert_decl(DeclKind::Intent, d.name.as_str(), s.clone());
    }
    for (d, s) in &raw.definitions {
        map.insert_decl(DeclKind::Definition, d.name.as_str(), s.clone());
    }
    for (i, s) in &raw.invariants {
        map.insert_decl(DeclKind::Invariant, i.name.as_str(), s.clone());
    }
    for (t, s, stmt_spans) in &raw.transformations {
        map.insert_decl(DeclKind::Transformation, t.name.as_str(), s.clone());
        map.insert_statements(t.name.as_str(), stmt_spans.clone());
    }
    for (d, s) in &raw.derived_claims {
        map.insert_decl(DeclKind::DerivedClaim, d.predicate.as_str(), s.clone());
    }

    let mut program = Program {
        name: raw.name,
        predicates: raw.predicates.into_iter().map(|(d, _)| d).collect(),
        intents: raw.intents.into_iter().map(|(d, _)| d).collect(),
        definitions: raw.definitions.into_iter().map(|(d, _)| d).collect(),
        invariants: raw.invariants.into_iter().map(|(i, _)| i).collect(),
        transformations: raw.transformations.into_iter().map(|(t, _, _)| t).collect(),
        derived_claims: raw.derived_claims.into_iter().map(|(d, _)| d).collect(),
    };
    // A call is spelled exactly like a claim reference; only the
    // declaration table can tell them apart, and a reference may
    // precede the definition it names, so resolution runs over the
    // whole collected programme. Formatting a resolved call prints
    // the same text back, so round-trip holds.
    morpholog_core::resolve_defined_calls(&mut program);
    // Declared disciplines materialise as generated invariants here,
    // for the same reason resolution runs here: the whole programme is
    // in hand, and everything downstream (propose, scoped loading,
    // audit, guarantees, explain) then sees them with no caller
    // changes. The formatter omits them; reparsing regenerates them.
    morpholog_core::lower_disciplines(&mut program);
    // Each sum's empty-case seed resolves from the summed variable's
    // declared kind, after call resolution so a variable bound inside a
    // definition call is followed to its claim position. An empty cargo
    // book is `0 t` with no seed claim needed to open it.
    morpholog_core::lower_sum_seeds(&mut program);
    Ok((program, map))
}

/// Report every name declared more than once in `items`: the
/// diagnostic points at the repeat, the secondary at the first
/// declaration. One shape serves every declaration kind; only the
/// noun differs.
fn report_duplicates<'a>(
    diagnostics: &mut Vec<Diagnostic>,
    what: &str,
    items: impl Iterator<Item = (&'a str, &'a Span)>,
) {
    let mut by_name: HashMap<&str, &Span> = HashMap::new();
    for (name, span) in items {
        if let Some(first_span) = by_name.get(name) {
            diagnostics.push(
                Diagnostic::error(
                    format!("duplicate {what} declaration `{name}`"),
                    span.clone(),
                )
                .with_secondary((*first_span).clone(), "previously declared here"),
            );
        } else {
            by_name.insert(name, span);
        }
    }
}

/// Intermediate parse result. Carries spans alongside the parsed
/// values so the post-pass (duplicate detection) can produce
/// span-rich diagnostics and the [`SourceMap`] can keep them. The
/// final `Program` strips spans because the kernel IR is
/// source-agnostic; transformations also carry one span per
/// top-level body statement.
#[derive(Debug)]
struct RawProgram {
    name: String,
    predicates: Vec<(PredicateDecl, Span)>,
    intents: Vec<(IntentDecl, Span)>,
    definitions: Vec<(Definition, Span)>,
    invariants: Vec<(Invariant, Span)>,
    transformations: Vec<(Transformation, Span, Vec<Span>)>,
    derived_claims: Vec<(DerivedClaim, Span)>,
}

/// One top-level declaration in a programme body. Predicates,
/// invariants, and transformations can be freely interleaved; the
/// parser partitions them into the `RawProgram` vectors after
/// collection, preserving source order within each category. This
/// shape avoids committing the language to an "all predicates
/// first" file convention.
enum TopLevelDecl {
    Predicate(PredicateDecl, Span),
    Intent(IntentDecl, Span),
    Definition(Definition, Span),
    Invariant(Invariant, Span),
    Transformation(Transformation, Span, Vec<Span>),
    Derived(DerivedClaim, Span),
}

fn program_parser<'a, I>() -> impl Parser<'a, I, RawProgram, extra::Err<Rich<'a, Token>>>
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };
    // A declared kind, or an identifier where one was expected - the
    // latter names the whole vocabulary rather than leaving the author
    // guessing what a kind even is (`String` is the classic reach).
    let kind = select! { Token::Kind(k) => k }.or(ident.validate(|word, e, emitter| {
        let span: SimpleSpan = e.span();
        emitter.emit(Rich::custom(
            span,
            format!(
                "`{word}` is not a kind; declared kinds are `Subject`, `Decimal`, \
                 `Decimal[UNIT]`, `Date`, `Timestamp`, `Duration`, `Bool`, and \
                 `Collection` (labels and identifiers ride `Subject`)"
            ),
        ));
        PredicateArgKind::Subject
    }));

    // arg ::= Ident ":" Kind ("[" Ident "]")?
    // The unit brackets attach only to `Decimal` - `Decimal[USD]` is a
    // unit-tagged exact decimal. A unit on any other kind has no
    // meaning the kernel could honour, so it is a parse-time error.
    let unit = just(Token::LBracket)
        .ignore_then(ident)
        .then_ignore(just(Token::RBracket));
    let arg = ident
        .then_ignore(just(Token::Colon))
        .then(kind)
        .then(unit.or_not())
        .validate(|((name, kind), unit), e, emitter| {
            let kind = match (kind, unit) {
                (k, None) => k,
                (PredicateArgKind::Decimal, Some(u)) => {
                    PredicateArgKind::Quantity(Unit::from(u.clone()))
                }
                (k, Some(_)) => {
                    let span: SimpleSpan = e.span();
                    emitter.emit(Rich::custom(
                        span,
                        format!(
                            "only `Decimal` takes a unit annotation; `{k}[...]` has no meaning"
                        ),
                    ));
                    k
                }
            };
            ArgDecl { name, kind }
        });

    // arg_list ::= arg ("," arg)* ","?
    let arg_list = arg
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<ArgDecl>>();

    // discipline_clause ::= "unique" "by" "(" ident,+ ")"
    //                      | "append" "only"
    //                      | "current" "pointer" "by" "(" ident,+ ")"
    //                      | "superseded" "via" Ident
    //
    // Every clause word is a contextual identifier (the `before` /
    // `duration` precedent), so none is reserved and all stay usable
    // as variable names. Clauses follow the arg list inline or on
    // indented continuation lines (one layout block; several clauses
    // may share it). No ambiguity with the next declaration, which
    // always opens with a reserved keyword.
    let kw_unique = select! { Token::Ident(s) if s == "unique" => () };
    let kw_by = select! { Token::Ident(s) if s == "by" => () };
    let kw_append = select! { Token::Ident(s) if s == "append" => () };
    let kw_only = select! { Token::Ident(s) if s == "only" => () };
    let kw_current = select! { Token::Ident(s) if s == "current" => () };
    let kw_pointer = select! { Token::Ident(s) if s == "pointer" => () };
    let kw_superseded = select! { Token::Ident(s) if s == "superseded" => () };
    let kw_via = select! { Token::Ident(s) if s == "via" => () };
    let field_list = ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .at_least(1)
        .collect::<Vec<String>>()
        .delimited_by(just(Token::LParen), just(Token::RParen));
    let discipline_clause = choice((
        kw_unique
            .ignore_then(kw_by)
            .ignore_then(field_list.clone())
            .map(|fields| Discipline::UniqueBy { fields }),
        kw_append.ignore_then(kw_only).to(Discipline::AppendOnly),
        kw_current
            .ignore_then(kw_pointer)
            .ignore_then(kw_by)
            .ignore_then(field_list)
            .map(|fields| Discipline::CurrentPointerBy { fields }),
        kw_superseded
            .ignore_then(kw_via)
            .ignore_then(ident)
            .map(|lineage| Discipline::SupersededVia {
                lineage: lineage.into(),
            }),
    ));
    let discipline_seq = discipline_clause
        .repeated()
        .at_least(1)
        .collect::<Vec<Discipline>>();
    let disciplines = super::indented_or_inline(discipline_seq)
        .or_not()
        .map(Option::unwrap_or_default);

    // predicate_decl ::= "predicate" Ident "(" arg_list? ")" discipline_clause*
    let predicate_decl = just(Token::KwPredicate)
        .ignore_then(ident)
        .then(
            arg_list
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then(disciplines)
        .map_with(|((name, args), disciplines), e| {
            let span: SimpleSpan = e.span();
            TopLevelDecl::Predicate(
                PredicateDecl {
                    name: name.into(),
                    args,
                    disciplines,
                },
                span.start()..span.end(),
            )
        });

    // intent_decl ::= "intent" Ident "(" arg_list? ")"
    // Mirrors predicate_decl exactly - same surface shape, different
    // vocabulary. The parser distinguishes them; the check
    // validates emits against intent decls just as it validates
    // claims against predicate decls.
    let intent_decl = just(Token::KwIntent)
        .ignore_then(ident)
        .then(arg_list.delimited_by(just(Token::LParen), just(Token::RParen)))
        .map_with(|(name, args), e| {
            let span: SimpleSpan = e.span();
            TopLevelDecl::Intent(
                IntentDecl {
                    name: name.into(),
                    args,
                },
                span.start()..span.end(),
            )
        });

    // invariant_decl ::= "invariant" Ident ":" body
    // body           ::= Indent let_line* expression Dedent | expression
    // let_line       ::= "let" Ident "=" "(" value_expression ")"
    //
    // The body alternative accepts both inline form
    // (`invariant cap: Foo(x)`) and indented multi-line form
    // (`invariant cap:\n    Foo(x)`). The layout pass produces
    // `Indent`/`Dedent` around the indented form; the inline form
    // has no layout tokens.
    //
    // A body `let` names a value expression and is substituted away
    // before the IR exists (see [`super::lets`]). Lets live in the
    // indented form only, and the value must be parenthesised:
    // parens already mean "layout off", so the value can span lines
    // freely, and a value ending in a bare decimal cannot absorb the
    // next line's leading identifier as a quantity unit.
    //
    // No version syntax in v0: the version field defaults to 1.
    // When versioning grows a second meaningful value, the surface
    // adds a clause (e.g. `version <N>`) and the parser starts
    // accepting it. Today, an attempted `invariant Name (v1):`
    // surfaces as an unexpected-token diagnostic on the `(`.
    let let_line = just(Token::KwLet)
        .ignore_then(ident)
        .then_ignore(just(Token::Eq))
        .then(value_expr_parser().delimited_by(just(Token::LParen), just(Token::RParen)))
        .map_with(|(name, value), e| {
            let span: SimpleSpan = e.span();
            super::lets::LetBinding {
                name,
                value,
                span: span.start()..span.end(),
            }
        });
    let body_with_lets = choice((
        just(Token::Indent)
            .ignore_then(let_line.repeated().collect::<Vec<_>>())
            .then(expression_parser())
            .then_ignore(just(Token::Dedent)),
        expression_parser().map(|body| (Vec::new(), body)),
    ));
    let invariant_decl = just(Token::KwInvariant)
        .ignore_then(ident)
        .then_ignore(just(Token::Colon))
        .then(body_with_lets.clone())
        .validate(|(name, (bindings, body)), e, emitter| {
            let (body, refusals) = super::lets::apply(bindings, &[], body);
            for (span, message) in refusals {
                emitter.emit(Rich::custom(span.into(), message));
            }
            let span: SimpleSpan = e.span();
            TopLevelDecl::Invariant(
                Invariant {
                    name: name.into(),
                    version: 1,
                    body,
                    origin: InvariantOrigin::Authored,
                },
                span.start()..span.end(),
            )
        });

    // definition_decl ::= "define" Ident "(" param-list ")" ":" body
    //
    // A named, parameterised proposition. Params are bare identifiers
    // like a transformation's (their kinds are inferred from the body);
    // the body is one proposition in the invariant's inline-or-indented
    // shape, `let` prefix included. Calls are claim-shaped references
    // resolved by name after the whole programme is collected (a
    // reference may precede the definition it names).
    let definition_param_list = ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<String>>();
    let definition_decl = just(Token::KwDefine)
        .ignore_then(ident)
        .then(definition_param_list.delimited_by(just(Token::LParen), just(Token::RParen)))
        .then_ignore(just(Token::Colon))
        .then(body_with_lets)
        .validate(|((name, parameters), (bindings, body)), e, emitter| {
            let (body, refusals) = super::lets::apply(bindings, &parameters, body);
            for (span, message) in refusals {
                emitter.emit(Rich::custom(span.into(), message));
            }
            let span: SimpleSpan = e.span();
            TopLevelDecl::Definition(
                Definition {
                    name: name.into(),
                    parameters: parameters.into_iter().map(Var::from).collect(),
                    body,
                },
                span.start()..span.end(),
            )
        });

    // transformation_decl ::= "transformation" Ident "(" param-list ")" ":" Indent stmt+ Dedent
    //
    // Params are bare identifiers (no kinds); the IR stores them as
    // `Var` because they initialise the transformation's binding context.
    // The body uses indented-block layout: the layout pass emits
    // Indent after the colon and Dedent at block end.
    let param_list = ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<String>>();
    // Each top-level statement keeps its span so the SourceMap can
    // place a check finding on the statement, not just the
    // transformation header. Nested statements (inside a `for`) are
    // covered by the enclosing statement's span.
    let transformation_body = just(Token::Indent)
        .ignore_then(
            statement_parser()
                .map_with(|stmt, e| {
                    let span: SimpleSpan = e.span();
                    (stmt, span.start()..span.end())
                })
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .then_ignore(just(Token::Dedent));
    let transformation_decl = just(Token::KwTransformation)
        .ignore_then(ident)
        .then(param_list.delimited_by(just(Token::LParen), just(Token::RParen)))
        .then_ignore(just(Token::Colon))
        .then(transformation_body)
        .map_with(|((name, parameters), body), e| {
            let span: SimpleSpan = e.span();
            let (body, stmt_spans): (Vec<_>, Vec<_>) = body.into_iter().unzip();
            TopLevelDecl::Transformation(
                Transformation {
                    name: name.into(),
                    parameters: parameters.into_iter().map(Var::from).collect(),
                    body,
                },
                span.start()..span.end(),
                stmt_spans,
            )
        });

    // derived_decl ::= "derived" Ident "(" key_list ")" ":" Indent over_clause value_clause+ Dedent
    //   over_clause  ::= "over" expression
    //   value_clause ::= "value" Ident "=" expression
    //   key_list     ::= Ident ("," Ident)* ","?
    //
    // Each `value` clause becomes a `DerivedValue { name, expr }`.
    // The IR evaluates each value expression against the per-key
    // bindings only; values do not see one another. Surface
    // mirrors that (no `let` for intermediates).
    let key_list = ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<String>>();
    let over_clause = just(Token::KwOver).ignore_then(expression_parser());
    let value_clause = just(Token::KwValue)
        .ignore_then(ident)
        .then_ignore(just(Token::Eq))
        .then(value_expr_parser())
        .map(|(name, expr)| DerivedValue { name, expr });
    let derived_body = just(Token::Indent)
        .ignore_then(over_clause)
        .then(value_clause.repeated().at_least(1).collect::<Vec<_>>())
        .then_ignore(just(Token::Dedent));
    let derived_decl = just(Token::KwDerived)
        .ignore_then(ident)
        .then(key_list.delimited_by(just(Token::LParen), just(Token::RParen)))
        .then_ignore(just(Token::Colon))
        .then(derived_body)
        .map_with(|((predicate, keys), (domain, values)), e| {
            let span: SimpleSpan = e.span();
            TopLevelDecl::Derived(
                DerivedClaim {
                    predicate: predicate.into(),
                    keys: keys.into_iter().map(Var::from).collect(),
                    values,
                    domain,
                },
                span.start()..span.end(),
            )
        });

    // top_level_decl ::= predicate_decl | invariant_decl | transformation_decl | derived_decl
    //
    // Free interleaving: a programme may mix any of the four in
    // any order. The parser collects them into a single sequence
    // and partitions on the post-pass (source order preserved
    // within each category).
    let top_level_decl = choice((
        predicate_decl,
        intent_decl,
        definition_decl,
        invariant_decl,
        transformation_decl,
        derived_decl,
    ));

    // Sync at the next top-level keyword on failure; skip the rest
    // of the malformed declaration but keep the declarations on
    // either side of it.
    let top_level_recovering = top_level_decl.recover_with(skip_then_retry_until(
        any().ignored(),
        just(Token::KwPredicate)
            .ignored()
            .or(just(Token::KwIntent).ignored())
            .or(just(Token::KwDefine).ignored())
            .or(just(Token::KwInvariant).ignored())
            .or(just(Token::KwTransformation).ignored())
            .or(just(Token::KwDerived).ignored())
            .or(end()),
    ));

    // program_header ::= "program" Ident
    let header = just(Token::KwProgram).ignore_then(ident);

    header
        .then(top_level_recovering.repeated().collect::<Vec<_>>())
        .then_ignore(end())
        .map(|(name, decls)| {
            let mut predicates = Vec::new();
            let mut intents = Vec::new();
            let mut definitions = Vec::new();
            let mut invariants = Vec::new();
            let mut transformations = Vec::new();
            let mut derived_claims = Vec::new();
            for d in decls {
                match d {
                    TopLevelDecl::Predicate(p, s) => predicates.push((p, s)),
                    TopLevelDecl::Intent(i, s) => intents.push((i, s)),
                    TopLevelDecl::Definition(d, s) => definitions.push((d, s)),
                    TopLevelDecl::Invariant(i, s) => invariants.push((i, s)),
                    TopLevelDecl::Transformation(t, s, ss) => transformations.push((t, s, ss)),
                    TopLevelDecl::Derived(d, s) => derived_claims.push((d, s)),
                }
            }
            RawProgram {
                name,
                predicates,
                intents,
                definitions,
                invariants,
                transformations,
                derived_claims,
            }
        })
}
