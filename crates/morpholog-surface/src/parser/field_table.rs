//! The declared-name table a named pattern resolves against.
//!
//! A named pattern (`Pred(field: x, ..)`) needs the declaration's field order, but the
//! declaration may come later in the file. So a quick scan of the raw tokens collects field lists
//! before parsing. It is not a second grammar: anything it cannot follow, or any name declared
//! twice, is left out, and the real parser reports the error. A test checks the scan against the
//! parser over every worked example.
//!
//! Predicates and intents are kept apart, since one name can be both with different fields.
//! Definitions resolve against their parameters: an authored one's as written, and the selector
//! an `effective by` clause generates by the signature the lowering will give it.

use std::collections::HashMap;

use crate::lexer::Token;

/// One declaration's usable name list, or the refusal to have one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeclFields {
    Usable(Vec<String>),
    /// Declared more than once, so which names apply would depend on source order.
    Ambiguous,
}

#[derive(Debug, Clone, Default)]
pub(super) struct FieldTable {
    pub(super) predicates: HashMap<String, DeclFields>,
    pub(super) intents: HashMap<String, DeclFields>,
    /// Parameter lists, authored and generated alike.
    pub(super) definitions: HashMap<String, DeclFields>,
}

impl FieldTable {
    /// The table for parsing a lone expression: every named pattern is refused as undeclared.
    pub(super) fn empty() -> Self {
        Self::default()
    }
}

fn record(map: &mut HashMap<String, DeclFields>, name: String, fields: Vec<String>) {
    map.entry(name)
        .and_modify(|f| *f = DeclFields::Ambiguous)
        .or_insert(DeclFields::Usable(fields));
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
                if let Some((name, fields, next)) = scan_decl(tokens, i + 1, true) {
                    if is_predicate {
                        if let Some(selector) = scan_effective_by(tokens, next, &name, &fields) {
                            let parameters = selector
                                .parameters
                                .iter()
                                .map(|v| v.as_str().to_string())
                                .collect();
                            record(&mut table.definitions, selector.name, parameters);
                        }
                        record(&mut table.predicates, name, fields);
                    } else {
                        record(&mut table.intents, name, fields);
                    }
                    i = next;
                } else {
                    i += 1;
                }
            }
            Token::KwDefine => {
                if let Some((name, parameters, next)) = scan_decl(tokens, i + 1, false) {
                    record(&mut table.definitions, name, parameters);
                    i = next;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    table
}

/// Follow one declaration head `Ident ( ... )`. At paren depth 1, each `Ident :` is a field name
/// and its kind is skipped up to the next `,`; with `typed` off, each `Ident` is a parameter.
/// Returns `(name, names, index past the closing paren)`, or `None` when the shape does not hold.
fn scan_decl<S>(
    tokens: &[(Token, S)],
    start: usize,
    typed: bool,
) -> Option<(String, Vec<String>, usize)> {
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
                let named = matches!(tokens.get(i + 1).map(|t| &t.0), Some(Token::Colon));
                if named == typed {
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
        // Something in the parens, but nothing of the expected shape: leave it to the real parser.
        return None;
    }
    Some((name.clone(), fields, i))
}

/// The selector an `effective by (keys) on (field)` clause after a predicate head will generate,
/// with the signature the lowering gives it. The clauses run from the head to the next keyword;
/// the first clause of that shape counts, as it does in the lowering.
fn scan_effective_by<S>(
    tokens: &[(Token, S)],
    start: usize,
    predicate: &str,
    fields: &[String],
) -> Option<morpholog_core::InForceSignature> {
    let end = (start..tokens.len())
        .find(|&i| tokens[i].0.is_keyword())
        .unwrap_or(tokens.len());
    let word =
        |i: usize, w: &str| matches!(&tokens.get(i)?.0, Token::Ident(s) if s == w).then_some(());
    let mut i = start;
    while i < end {
        if word(i, "effective").is_some() && word(i + 1, "by").is_some() {
            let (keys, after_keys) = scan_ident_list(tokens, i + 2)?;
            word(after_keys, "on")?;
            let (on, _) = scan_ident_list(tokens, after_keys + 1)?;
            let [on] = on.as_slice() else {
                return None;
            };
            return morpholog_core::in_force_signature(&predicate.into(), fields, &keys, on);
        }
        i += 1;
    }
    None
}

/// `( Ident, Ident, ... )` at `start`: the names and the index past the closing paren.
fn scan_ident_list<S>(tokens: &[(Token, S)], start: usize) -> Option<(Vec<String>, usize)> {
    if !matches!(tokens.get(start)?.0, Token::LParen) {
        return None;
    }
    let mut names = Vec::new();
    let mut i = start + 1;
    loop {
        match &tokens.get(i)?.0 {
            Token::Ident(name) => names.push(name.clone()),
            _ => return None,
        }
        i += 1;
        match &tokens.get(i)?.0 {
            Token::Comma => i += 1,
            Token::RParen => return Some((names, i + 1)),
            _ => return None,
        }
        // A trailing comma closes the list, as in the grammar.
        if matches!(tokens.get(i)?.0, Token::RParen) {
            return Some((names, i + 1));
        }
    }
}

/// Which declarations a named pattern resolves against, chosen by where it appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Vocabulary {
    /// Propositions and `bind`: predicates, and definitions by their parameters.
    ClaimShaped,
    /// `admit`/`retract`/`value`: predicates only.
    PredicateOnly,
    Intent,
}

/// Resolve a named pattern to positional arguments in declared order, with wildcards for
/// unmentioned names. Errors come back as spanned messages.
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
    // A predicate and a definition of one name is a validation error; until then the predicate
    // reads as it always did.
    let (found, what) = match map.get(head) {
        Some(found) => (Some(found), "field"),
        None if vocabulary == Vocabulary::ClaimShaped => (table.definitions.get(head), "parameter"),
        None => (None, "field"),
    };
    let fields = match found {
        Some(DeclFields::Usable(fields)) => fields,
        Some(DeclFields::Ambiguous) => {
            return Err(vec![(
                call_span,
                format!("`{head}` is declared more than once, so its {what} names cannot resolve"),
            )]);
        }
        None => {
            let is_definition =
                vocabulary != Vocabulary::Intent && table.definitions.contains_key(head);
            let message = match (vocabulary, is_definition) {
                // A definition call is not allowed here at all, so do not suggest one.
                (Vocabulary::PredicateOnly, true) => {
                    format!("`{head}` is a definition, not a predicate")
                }
                (Vocabulary::Intent, _) => {
                    format!("named fields need a declared intent; `{head}` is not one")
                }
                (Vocabulary::ClaimShaped, _) => format!(
                    "named fields need a declared predicate or definition; `{head}` is neither"
                ),
                (Vocabulary::PredicateOnly, false) => {
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
                    "`{head}` declares no {what} `{name}`; declared: {}",
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
                    "a named pattern without `..` names every {what} of `{head}`; missing: {} \
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
                // Generated selectors included: the scan predicts the lowering's signature,
                // and this is where the prediction is held to it.
                let expected_definitions: std::collections::HashMap<String, DeclFields> = program
                    .definitions
                    .iter()
                    .map(|d| {
                        (
                            d.name.to_string(),
                            DeclFields::Usable(
                                d.parameters
                                    .iter()
                                    .map(|v| v.as_str().to_string())
                                    .collect(),
                            ),
                        )
                    })
                    .collect();
                assert_eq!(
                    table.definitions,
                    expected_definitions,
                    "definition tables diverge in {}",
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
