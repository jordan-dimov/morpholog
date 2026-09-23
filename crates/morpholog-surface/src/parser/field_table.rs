//! The declared-field table a named claim pattern resolves against.
//!
//! A named pattern (`Pred(field: x, ..)`) needs the declaration's field order, but the
//! declaration may come later in the file. So a quick scan of the raw tokens collects field lists
//! before parsing. It is not a second grammar: anything it cannot follow, or any name declared
//! twice, is left out, and the real parser reports the error. A test checks the scan against the
//! parser over every worked example.
//!
//! Predicates and intents are kept apart, since one name can be both with different fields.
//! `define` names are recorded only so a named pattern on one gets an accurate error.

use std::collections::{HashMap, HashSet};

use crate::lexer::Token;

/// One declaration's usable field list, or the refusal to have one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeclFields {
    Usable(Vec<String>),
    /// Declared more than once, so which fields apply would depend on source order.
    Ambiguous,
}

#[derive(Debug, Clone, Default)]
pub(super) struct FieldTable {
    pub(super) predicates: HashMap<String, DeclFields>,
    pub(super) intents: HashMap<String, DeclFields>,
    pub(super) authored_definitions: HashSet<String>,
}

impl FieldTable {
    /// The table for parsing a lone expression: every named pattern is refused as undeclared.
    pub(super) fn empty() -> Self {
        Self::default()
    }
}

/// Scan the raw token stream for `predicate`, `intent` and `define` declarations. A declaration
/// the scan cannot follow is left out.
pub(super) fn scan<S>(tokens: &[(Token, S)]) -> FieldTable {
    let mut table = FieldTable::default();
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i].0 {
            Token::KwPredicate | Token::KwIntent => {
                let is_predicate = matches!(tokens[i].0, Token::KwPredicate);
                if let Some((name, fields, next)) = scan_decl(tokens, i + 1) {
                    let map = if is_predicate {
                        &mut table.predicates
                    } else {
                        &mut table.intents
                    };
                    map.entry(name)
                        .and_modify(|f| *f = DeclFields::Ambiguous)
                        .or_insert(DeclFields::Usable(fields));
                    i = next;
                } else {
                    i += 1;
                }
            }
            Token::KwDefine => {
                if let Some(Token::Ident(name)) = tokens.get(i + 1).map(|t| &t.0) {
                    table.authored_definitions.insert(name.clone());
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    table
}

/// Follow one declaration head `Ident ( ... )`. At paren depth 1, each `Ident :` is a field name
/// and its kind is skipped up to the next `,`. Returns `(name, fields, index past the closing
/// paren)`, or `None` when the shape does not hold.
fn scan_decl<S>(tokens: &[(Token, S)], start: usize) -> Option<(String, Vec<String>, usize)> {
    let Token::Ident(name) = &tokens.get(start)?.0 else {
        return None;
    };
    if !matches!(tokens.get(start + 1)?.0, Token::LParen) {
        return None;
    }
    let mut fields = Vec::new();
    let mut depth = 1usize;
    let mut at_field_slot = true;
    let mut saw_content = false;
    let mut i = start + 2;
    while depth > 0 {
        let token = &tokens.get(i)?.0;
        if depth == 1 && !matches!(token, Token::RParen) {
            saw_content = true;
        }
        match token {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Comma if depth == 1 => at_field_slot = true,
            Token::Ident(field) if depth == 1 && at_field_slot => {
                if matches!(tokens.get(i + 1).map(|t| &t.0), Some(Token::Colon)) {
                    fields.push(field.clone());
                }
                at_field_slot = false;
            }
            _ if depth == 1 => at_field_slot = false,
            _ => {}
        }
        i += 1;
    }
    if saw_content && fields.is_empty() {
        // Something in the parens, but no `name:` fields: leave it to the real parser.
        return None;
    }
    Some((name.clone(), fields, i))
}

/// Which declarations a named pattern resolves against, chosen by where it appears. The two
/// predicate variants differ only in whether the error may suggest a positional definition call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Vocabulary {
    /// Propositions and `bind`, where a positional definition call is also allowed.
    ClaimShaped,
    /// `admit`/`retract`: predicates only.
    PredicateOnly,
    Intent,
}

/// Resolve a named pattern to positional arguments in declared field order, with wildcards for
/// unmentioned fields. Errors come back as spanned messages.
pub(super) fn resolve_named(
    head: &str,
    entries: &[(chumsky::span::SimpleSpan, String, morpholog_core::Term)],
    rest: bool,
    vocabulary: Vocabulary,
    table: &FieldTable,
    call_span: chumsky::span::SimpleSpan,
) -> Result<Vec<morpholog_core::Term>, Vec<(chumsky::span::SimpleSpan, String)>> {
    use morpholog_core::Term;
    let map = match vocabulary {
        Vocabulary::ClaimShaped | Vocabulary::PredicateOnly => &table.predicates,
        Vocabulary::Intent => &table.intents,
    };
    let fields = match map.get(head) {
        Some(DeclFields::Usable(fields)) => fields,
        Some(DeclFields::Ambiguous) => {
            return Err(vec![(
                call_span,
                format!("`{head}` is declared more than once, so its field names cannot resolve"),
            )]);
        }
        None => {
            let is_definition =
                vocabulary != Vocabulary::Intent && table.authored_definitions.contains(head);
            let message = match (vocabulary, is_definition) {
                (Vocabulary::ClaimShaped, true) => format!(
                    "`{head}` is a definition; definitions have parameters, not declared \
                     fields - use the positional form"
                ),
                // A definition call is not allowed here at all, so do not suggest one.
                (Vocabulary::PredicateOnly, true) => {
                    format!("`{head}` is a definition, not a predicate")
                }
                (Vocabulary::Intent, _) => {
                    format!("named fields need a declared intent; `{head}` is not one")
                }
                (_, false) => {
                    format!("named fields need a declared predicate; `{head}` is not one")
                }
            };
            return Err(vec![(call_span, message)]);
        }
    };
    let mut refusals = Vec::new();
    for (span, name, _) in entries {
        if !fields.contains(name) {
            refusals.push((
                *span,
                format!(
                    "`{head}` declares no field `{name}`; declared: {}",
                    fields.join(", ")
                ),
            ));
        }
    }
    if !rest {
        let missing: Vec<&str> = fields
            .iter()
            .filter(|f| !entries.iter().any(|(_, name, _)| name == *f))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            refusals.push((
                call_span,
                format!(
                    "a named pattern without `..` names every field of `{head}`; missing: {} \
                     (name them, or end the pattern with `..`)",
                    missing.join(", ")
                ),
            ));
        }
    }
    if !refusals.is_empty() {
        return Err(refusals);
    }
    Ok(fields
        .iter()
        .map(|field| {
            entries
                .iter()
                .find(|(_, name, _)| name == field)
                .map(|(_, _, term)| term.clone())
                .unwrap_or(Term::Wildcard)
        })
        .collect())
}

/// One written `field: term` entry, as the pattern parser collects it.
type NamedEntry = (chumsky::span::SimpleSpan, String, morpholog_core::Term);

/// Spanned refusal messages for the call site's emitter.
type Refusals = Vec<(chumsky::span::SimpleSpan, String)>;

/// Resolve a named `value` lookup to `(args, extract)`. Exactly one entry must be `field: _`, and
/// its declared position becomes `extract`; fields skipped by `..` are wildcards, not the read
/// field. Built on [`resolve_named`], so both come from the same field order.
pub(super) fn resolve_named_value(
    head: &str,
    entries: &[NamedEntry],
    rest: bool,
    table: &FieldTable,
    call_span: chumsky::span::SimpleSpan,
) -> Result<(Vec<morpholog_core::Term>, usize), Refusals> {
    use morpholog_core::Term;
    let args = resolve_named(
        head,
        entries,
        rest,
        Vocabulary::PredicateOnly,
        table,
        call_span,
    )?;
    let holes: Vec<&NamedEntry> = entries
        .iter()
        .filter(|(_, _, term)| matches!(term, Term::Wildcard))
        .collect();
    match holes.as_slice() {
        [(_, hole_field, _)] => {
            let Some(DeclFields::Usable(fields)) = table.predicates.get(head) else {
                unreachable!("resolve_named succeeded against this head")
            };
            let extract = fields
                .iter()
                .position(|f| f == hole_field)
                .unwrap_or_else(|| unreachable!("resolve_named validated the entry fields"));
            Ok((args, extract))
        }
        [] => Err(vec![(
            call_span,
            format!(
                "a named `value` lookup on `{head}` marks the value to extract with \
                 `field: _`; no field is marked"
            ),
        )]),
        many => Err(many
            .iter()
            .skip(1)
            .map(|(span, name, _)| {
                (
                    *span,
                    format!(
                        "`{name}: _` marks a second extraction hole; a `value` lookup \
                         extracts exactly one field (leave the others to `..`)"
                    ),
                )
            })
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::{DeclFields, scan};
    use crate::lexer::lex;

    /// Over every worked example, the token scan must agree exactly with the parser's field
    /// lists, so a new declaration shape cannot silently break named patterns.
    #[test]
    fn the_scan_agrees_with_the_parsed_declarations_over_the_gallery() {
        let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(&examples).expect("examples/ exists") {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(&dir).unwrap() {
                let path = file.unwrap().path();
                if path.extension().is_none_or(|e| e != "morph") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).unwrap();
                let tokens = lex(&src).expect("gallery sources lex");
                let table = scan(&tokens);
                let program = crate::parser::parse_program(&src).expect("gallery sources parse");
                // Both directions: nothing missed, nothing invented.
                let expected_predicates: std::collections::HashMap<String, DeclFields> = program
                    .predicates
                    .iter()
                    .map(|d| {
                        (
                            d.name.to_string(),
                            DeclFields::Usable(d.args.iter().map(|a| a.name.to_string()).collect()),
                        )
                    })
                    .collect();
                assert_eq!(
                    table.predicates,
                    expected_predicates,
                    "predicate tables diverge in {}",
                    path.display()
                );
                let expected_intents: std::collections::HashMap<String, DeclFields> = program
                    .intents
                    .iter()
                    .map(|d| {
                        (
                            d.name.to_string(),
                            DeclFields::Usable(d.args.iter().map(|a| a.name.to_string()).collect()),
                        )
                    })
                    .collect();
                assert_eq!(
                    table.intents,
                    expected_intents,
                    "intent tables diverge in {}",
                    path.display()
                );
                let expected_definitions: std::collections::HashSet<String> = program
                    .definitions
                    .iter()
                    .filter(|d| d.origin != morpholog_core::DefinitionOrigin::Discipline)
                    .map(|d| d.name.to_string())
                    .collect();
                assert_eq!(
                    table.authored_definitions,
                    expected_definitions,
                    "definition sets diverge in {}",
                    path.display()
                );
                checked += 1;
            }
        }
        assert!(
            checked > 15,
            "the gallery walk found only {checked} programmes"
        );
    }

    /// Fail-closed: a malformed declaration is absent, never guessed.
    #[test]
    fn a_malformed_declaration_is_left_out_of_the_table() {
        let tokens = lex("predicate Broken(a: Subject").expect("lexes");
        let table = scan(&tokens);
        assert!(
            table.predicates.is_empty(),
            "unclosed decl must not resolve"
        );
    }
}
