//! The declared-field table a named claim pattern resolves against.
//!
//! Named patterns (`Pred(field: x, ..)`) need each declaration's field
//! order before the parser has built any declaration - and declarations
//! may follow their uses - so a tolerant scan over the RAW token stream
//! runs first and the parser builders capture its result. The scan is
//! lexical, never a second grammar: it records only what a named
//! pattern needs (names and field lists), fails CLOSED on anything
//! malformed (the real parser owns the syntax error), and refuses to
//! guess between duplicate declarations (the real parser owns that
//! diagnostic too). A conformance test holds the scan to the parsed
//! declarations over the whole example gallery, so it cannot silently
//! drift from the real grammar.
//!
//! Predicates and intents are separate vocabularies (one name may be
//! both, with different fields), so the table keeps them apart and the
//! enclosing site picks: `emit` resolves against intents, every
//! claim-shaped site against predicates. Authored `define` names are
//! recorded so a named pattern on one gets the truthful refusal
//! (definitions have parameters, not declared fields); generated
//! definitions do not exist at token time and fall into the generic
//! undeclared-head refusal.

use std::collections::{HashMap, HashSet};

use crate::lexer::Token;

/// One declaration's usable field list, or the refusal to have one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeclFields {
    Usable(Vec<String>),
    /// Declared more than once in its vocabulary - resolution against
    /// it would be source-order-dependent, so it refuses instead.
    Ambiguous,
}

#[derive(Debug, Clone, Default)]
pub(super) struct FieldTable {
    pub(super) predicates: HashMap<String, DeclFields>,
    pub(super) intents: HashMap<String, DeclFields>,
    pub(super) authored_definitions: HashSet<String>,
}

impl FieldTable {
    /// The table for entry points with no programme in hand
    /// (`parse_expression` / `parse_value_expr`): every named pattern
    /// refuses with the undeclared-head message.
    pub(super) fn empty() -> Self {
        Self::default()
    }
}

/// Scan the raw (pre-layout) token stream for `predicate` / `intent` /
/// `define` declarations. Tolerant and fail-closed: a declaration whose
/// shape the scan cannot follow is simply absent from the table.
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

/// Follow one declaration head: `Ident ( ... )` where, at paren depth
/// 1, every `Ident` immediately followed by `:` is a field name and
/// everything from the `:` to the next depth-1 `,` (the kind, however
/// its grammar grows) is skipped. Returns `(name, fields, index past
/// the closing paren)`, or `None` when the shape does not hold.
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
        // The parens held something, yet no `name:` fields scanned - a
        // declaration shape the scan cannot follow. Fail closed; the
        // real parser owns the syntax error.
        return None;
    }
    Some((name.clone(), fields, i))
}

/// Which declaration vocabulary a named pattern resolves against - the
/// enclosing site decides. One name may be a predicate AND an intent,
/// with different fields; the claim-shaped/predicate-only split exists
/// because the repair text differs: a definition call is lawful in a
/// proposition or `bind` (positionally), but `admit`/`retract` take
/// predicates only, so "use the positional form" would be a false
/// repair there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Vocabulary {
    /// Proposition positions and `bind`: predicates, where a positional
    /// DEFINITION call is also lawful.
    ClaimShaped,
    /// `admit`/`retract`: predicates only.
    PredicateOnly,
    Intent,
}

/// Resolve a named pattern's entries to the positional argument vector:
/// declared fields in declaration order, mentioned entries in place,
/// wildcards for the rest. Refusals come back as spanned messages for
/// the call site's emitter; the entries' terms (written order) are the
/// error-path filler so the parse can continue to its diagnostics.
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
                // A positional definition call is not lawful here
                // either, so offering it would be a false repair.
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

#[cfg(test)]
mod tests {
    use super::{DeclFields, scan};
    use crate::lexer::lex;

    /// The anti-drift gate: over every gallery programme, the tolerant
    /// token scan must agree exactly with the real parser's declared
    /// field lists. If the declaration grammar grows a shape the scan
    /// cannot follow, this names it before a named pattern misresolves.
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
                // Whole-structure equality, both directions: every
                // parsed declaration is scanned correctly AND the scan
                // invented nothing the parser does not know.
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
