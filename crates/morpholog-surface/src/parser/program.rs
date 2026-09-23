//! Programme-level parsing: the `program` header and every top-level declaration, in any order.
//!
//! After parsing it reports duplicate names, substitutes consts, resolves definition calls, and
//! lowers disciplines, so the returned [`Program`] is complete and ready to enforce.

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
/// A bare `program` header is a valid programme. Returns `Err` with at least one diagnostic on
/// any lex or parse failure.
pub fn parse_program(source: &str) -> Result<Program, Vec<Diagnostic>> {
    parse_program_with_sources(source).map(|(program, _)| program)
}

/// [`parse_program`], also returning a [`SourceMap`] that places every declaration and top-level
/// transformation statement in the source.
pub fn parse_program_with_sources(source: &str) -> Result<(Program, SourceMap), Vec<Diagnostic>> {
    let raw_tokens = lex(source).map_err(super::lex_error_diagnostics)?;

    // Named claim patterns resolve against this. Scanned up front because a declaration may
    // come after its uses.
    let field_table = super::field_table::scan(&raw_tokens);

    if raw_tokens.is_empty() {
        let end = source.len().min(1);
        return Err(vec![Diagnostic::error(
            "expected `program` header, found empty file",
            0..end,
        )]);
    }

    let tokens = crate::layout::apply_layout(source, raw_tokens)?;

    let stream = token_stream(&tokens);
    let (parsed, errs) = program_parser(&field_table)
        .parse(stream)
        .into_output_errors();

    let mut diagnostics = super::parse_error_diagnostics(errs);

    let Some(raw) = parsed else {
        if diagnostics.is_empty() {
            diagnostics.push(Diagnostic::error("parse failed", 0..source.len()));
        }
        return Err(diagnostics);
    };

    // Duplicate names are caught here, where both declarations' spans are known. The kernel
    // does not check invariant or transformation names at all.
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
    for (decl, span) in &raw.predicates {
        report_duplicate_fields(
            &mut diagnostics,
            "predicate",
            decl.name.as_str(),
            decl.args.iter().map(|a| a.name.as_str()),
            span,
        );
    }
    for (decl, span) in &raw.intents {
        report_duplicate_fields(
            &mut diagnostics,
            "intent",
            decl.name.as_str(),
            decl.args.iter().map(|a| a.name.as_str()),
            span,
        );
    }
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
        // Two keys with one name would shadow each other and silently enumerate wrongly.
        let mut seen_keys: HashSet<&str> = HashSet::new();
        for k in &d.keys {
            if !seen_keys.insert(k.as_str()) {
                diagnostics.push(Diagnostic::error(
                    format!("duplicate key `{}` in derived-claim `{}`", k, d.predicate),
                    span.clone(),
                ));
            }
        }

        // Two outputs with one name are a mistake the kernel does not catch.
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

    // Substitute consts first, so the passes below see const-free bodies.
    let mut raw = raw;
    {
        let const_errors = super::consts::apply(
            std::mem::take(&mut raw.consts),
            super::consts::ConstTargets {
                definitions: &mut raw.definitions,
                invariants: &mut raw.invariants,
                transformations: &mut raw.transformations,
                derived_claims: &mut raw.derived_claims,
                body_let_names: &raw.body_let_names,
            },
        );
        for (span, message) in const_errors {
            diagnostics.push(Diagnostic::error(message, span));
        }
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
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
    // Lowering is about to add generated definitions, after which authored and generated ones
    // look the same. Catch a clash now, or the author's definition would silently replace the
    // generated one.
    let mut collisions: Vec<Diagnostic> = Vec::new();
    for decl in &program.predicates {
        for discipline in &decl.disciplines {
            if !matches!(discipline, morpholog_core::Discipline::EffectiveBy { .. }) {
                continue;
            }
            let generated = morpholog_core::in_force_define_name(&decl.name);
            if program
                .definitions
                .iter()
                .any(|d| d.name.as_str() == generated)
            {
                collisions.push(Diagnostic::error(
                    format!(
                        "`{}` is effective-dated, which generates a definition named \
                         `{generated}` - but this programme already defines it. Rename one \
                         of them; the generated selector is not overridable.",
                        decl.name
                    ),
                    map.decl_span(DeclKind::Predicate, decl.name.as_str())
                        .unwrap_or_default(),
                ));
            }
        }
    }
    if !collisions.is_empty() {
        return Err(collisions);
    }
    // Generated definitions first, so calls to them resolve in the next pass instead of looking
    // like undeclared predicates.
    morpholog_core::lower_discipline_definitions(&mut program);
    // A call looks exactly like a claim reference, and may come before its definition, so it is
    // resolved only once the whole programme is in hand.
    morpholog_core::resolve_defined_calls(&mut program);
    // Disciplines become generated invariants, so everything downstream sees them as ordinary
    // rules. The formatter leaves them out; reparsing regenerates them.
    morpholog_core::lower_disciplines(&mut program);
    // An empty sum takes the zero of the summed variable's declared kind (`0 t`, not `0`). Runs
    // after call resolution so variables bound through definition calls can be traced.
    morpholog_core::lower_sum_seeds(&mut program);
    Ok((program, map))
}

/// Report every name declared more than once in `items`, pointing at the repeat and, as a
/// secondary span, at the first declaration.
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

/// Report a declaration that repeats an argument name, which would make every lookup by field
/// name ambiguous. `Program::validate` refuses it too, but without a span.
fn report_duplicate_fields<'a>(
    diagnostics: &mut Vec<Diagnostic>,
    what: &str,
    name: &str,
    fields: impl Iterator<Item = &'a str>,
    span: &Span,
) {
    let mut seen = std::collections::HashSet::new();
    let mut dups: Vec<&str> = Vec::new();
    for field in fields {
        if !seen.insert(field) && !dups.contains(&field) {
            dups.push(field);
        }
    }
    for field in dups {
        diagnostics.push(Diagnostic::error(
            format!(
                "{what} `{name}` declares argument `{field}` more than once; \
                 each field names one position"
            ),
            span.clone(),
        ));
    }
}

/// Parsed declarations with their spans, for duplicate checks and the [`SourceMap`].
/// Transformations also carry one span per top-level statement.
#[derive(Debug)]
struct RawProgram {
    name: String,
    predicates: Vec<(PredicateDecl, Span)>,
    intents: Vec<(IntentDecl, Span)>,
    definitions: Vec<(Definition, Span)>,
    invariants: Vec<(Invariant, Span)>,
    transformations: Vec<(Transformation, Span, Vec<Span>)>,
    derived_claims: Vec<(DerivedClaim, Span)>,
    consts: Vec<super::lets::LetBinding>,
    /// Body `let` names, kept after the lets are substituted away so a const that a body shadows
    /// can be refused.
    body_let_names: Vec<(String, Span)>,
}

/// One top-level declaration. Declarations may come in any order; they are sorted by kind
/// afterwards, keeping source order within each kind.
enum TopLevelDecl {
    Predicate(PredicateDecl, Span),
    Intent(IntentDecl, Span),
    Definition(Definition, Span, Vec<(String, Span)>),
    Invariant(Invariant, Span, Vec<(String, Span)>),
    Transformation(Transformation, Span, Vec<Span>),
    Derived(DerivedClaim, Span),
    Const(super::lets::LetBinding),
}

fn program_parser<'a, I>(
    table: &'a super::field_table::FieldTable,
) -> impl Parser<'a, I, RawProgram, extra::Err<Rich<'a, Token>>>
where
    I: ValueInput<'a, Token = Token, Span = SimpleSpan>,
{
    let ident = select! { Token::Ident(s) => s };
    // An unknown kind (`String`, say) gets a diagnostic listing the real ones.
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
    // Only `Decimal` takes a unit, as in `Decimal[USD]`.
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
    //                      | "effective" "by" "(" ident,+ ")" "on" "(" ident ")" "partial"?
    //
    // Clause words are not reserved, so they stay usable as variable names. Clauses follow the
    // argument list inline or in one indented block.
    let kw_unique = select! { Token::Ident(s) if s == "unique" => () };
    let kw_by = select! { Token::Ident(s) if s == "by" => () };
    let kw_append = select! { Token::Ident(s) if s == "append" => () };
    let kw_only = select! { Token::Ident(s) if s == "only" => () };
    let kw_current = select! { Token::Ident(s) if s == "current" => () };
    let kw_pointer = select! { Token::Ident(s) if s == "pointer" => () };
    let kw_effective = select! { Token::Ident(s) if s == "effective" => () };
    let kw_partial = select! { Token::Ident(s) if s == "partial" => () };
    let kw_on = select! { Token::Ident(s) if s == "on" => () };
    let kw_superseded = select! { Token::Ident(s) if s == "superseded" => () };
    let kw_via = select! { Token::Ident(s) if s == "via" => () };
    let field_list = ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .at_least(1)
        .collect::<Vec<String>>()
        .delimited_by(just(Token::LParen), just(Token::RParen));
    let single_field = ident.delimited_by(just(Token::LParen), just(Token::RParen));
    let discipline_clause = choice((
        kw_effective
            .ignore_then(kw_by)
            .ignore_then(field_list.clone())
            .then_ignore(kw_on)
            .then(single_field)
            .then(kw_partial.or_not())
            .map(|((keys, on), partial)| Discipline::EffectiveBy {
                keys,
                on,
                partial: partial.is_some(),
            }),
        kw_unique
            .ignore_then(kw_by)
            .ignore_then(field_list.clone())
            .map(|fields| Discipline::UniqueBy { fields }),
        kw_append.ignore_then(kw_only).to(Discipline::AppendOnly),
        kw_current
            .ignore_then(kw_pointer)
            .ignore_then(kw_by)
            .ignore_then(field_list.clone())
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

    // body     ::= Indent let_line* expression Dedent | expression
    // let_line ::= "let" Ident "=" "(" value_expression ")"
    //
    // A body `let` is substituted away before the IR exists (see [`super::lets`]). Its value
    // needs parentheses: it can then span lines, and a trailing number cannot swallow the next
    // line's first word as a unit.
    let let_line = just(Token::KwLet)
        .ignore_then(ident)
        .then_ignore(just(Token::Eq))
        .then(value_expr_parser(table).delimited_by(just(Token::LParen), just(Token::RParen)))
        .map_with(|(name, value), e| {
            let span: SimpleSpan = e.span();
            super::lets::LetBinding {
                name,
                value,
                span: span.start()..span.end(),
                noun: "let",
            }
        });
    let body_with_lets = choice((
        just(Token::Indent)
            .ignore_then(let_line.repeated().collect::<Vec<_>>())
            .then(expression_parser(table))
            .then_ignore(just(Token::Dedent)),
        expression_parser(table).map(|body| (Vec::new(), body)),
    ));
    // invariant_decl ::= "invariant" Ident ("total" "over" Ident)? ":" body
    //
    // `total` is not reserved; `over` is, because `derived` uses it. There is no version
    // syntax: every parsed invariant is version 1.
    let kw_total = select! { Token::Ident(s) if s == "total" => () };
    let totality_clause = kw_total.ignore_then(just(Token::KwOver)).ignore_then(ident);
    let invariant_decl = just(Token::KwInvariant)
        .ignore_then(ident)
        .then(totality_clause.or_not())
        .then_ignore(just(Token::Colon))
        .then(body_with_lets.clone())
        .validate(|((name, totality_for), (bindings, body)), e, emitter| {
            let let_names: Vec<(String, Span)> = bindings
                .iter()
                .map(|b| (b.name.clone(), b.span.clone()))
                .collect();
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
                    totality_for: totality_for.map(Into::into),
                },
                span.start()..span.end(),
                let_names,
            )
        });

    // definition_decl ::= "define" Ident "(" param-list ")" ":" body
    //
    // A named proposition with parameters, whose kinds are inferred from the body. The body is
    // shaped like an invariant's.
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
            let let_names: Vec<(String, Span)> = bindings
                .iter()
                .map(|b| (b.name.clone(), b.span.clone()))
                .collect();
            let (body, refusals) = super::lets::apply(bindings, &parameters, body);
            for (span, message) in refusals {
                emitter.emit(Rich::custom(span.into(), message));
            }
            let span: SimpleSpan = e.span();
            TopLevelDecl::Definition(
                Definition {
                    origin: morpholog_core::DefinitionOrigin::Authored,
                    name: name.into(),
                    parameters: parameters.into_iter().map(Var::from).collect(),
                    body,
                },
                span.start()..span.end(),
                let_names,
            )
        });

    // const_decl ::= "const" Ident "=" "(" value_expression ")"
    //
    // Substituted away at parse time (see [`super::consts`]). Parenthesised for the same
    // reasons as a body `let`.
    let const_decl = just(Token::KwConst)
        .ignore_then(ident)
        .then_ignore(just(Token::Eq))
        .then(value_expr_parser(table).delimited_by(just(Token::LParen), just(Token::RParen)))
        .map_with(|(name, value), e| {
            let span: SimpleSpan = e.span();
            TopLevelDecl::Const(super::lets::LetBinding {
                name,
                value,
                span: span.start()..span.end(),
                noun: "const",
            })
        });

    // transformation_decl ::= "transformation" Ident "(" param-list ")" ":" Indent stmt+ Dedent
    //
    // Parameters are bare identifiers; their kinds are inferred.
    let param_list = ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<String>>();
    // Keep each top-level statement's span, so a finding can point at the statement.
    let transformation_body = just(Token::Indent)
        .ignore_then(
            statement_parser(table)
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
    // Each value sees only the per-key bindings, never another value.
    let key_list = ident
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<String>>();
    let over_clause = just(Token::KwOver).ignore_then(expression_parser(table));
    let value_clause = just(Token::KwValue)
        .ignore_then(ident)
        .then_ignore(just(Token::Eq))
        .then(value_expr_parser(table))
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

    // Declarations may come in any order.
    let top_level_decl = choice((
        predicate_decl,
        intent_decl,
        definition_decl,
        invariant_decl,
        transformation_decl,
        derived_decl,
        const_decl,
    ));

    // On failure, skip to the next top-level keyword, keeping the declarations around it.
    let top_level_recovering = top_level_decl.recover_with(skip_then_retry_until(
        any().ignored(),
        just(Token::KwPredicate)
            .ignored()
            .or(just(Token::KwIntent).ignored())
            .or(just(Token::KwDefine).ignored())
            .or(just(Token::KwInvariant).ignored())
            .or(just(Token::KwTransformation).ignored())
            .or(just(Token::KwDerived).ignored())
            .or(just(Token::KwConst).ignored())
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
            let mut consts = Vec::new();
            let mut body_let_names = Vec::new();
            for d in decls {
                match d {
                    TopLevelDecl::Predicate(p, s) => predicates.push((p, s)),
                    TopLevelDecl::Intent(i, s) => intents.push((i, s)),
                    TopLevelDecl::Definition(d, s, lets) => {
                        body_let_names.extend(lets);
                        definitions.push((d, s));
                    }
                    TopLevelDecl::Invariant(i, s, lets) => {
                        body_let_names.extend(lets);
                        invariants.push((i, s));
                    }
                    TopLevelDecl::Transformation(t, s, ss) => transformations.push((t, s, ss)),
                    TopLevelDecl::Derived(d, s) => derived_claims.push((d, s)),
                    TopLevelDecl::Const(c) => consts.push(c),
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
                consts,
                body_let_names,
            }
        })
}
