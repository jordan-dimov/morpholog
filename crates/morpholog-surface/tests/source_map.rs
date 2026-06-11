//! The SourceMap: declaration and statement spans that survive
//! parsing, and the lookups that place a kernel finding (a
//! ValidationError, a Lint) back in the `.morph` text. Spans are
//! asserted by slicing the source - the test reads what the span
//! points at, not magic offsets.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ValidationContext, ValidationError, lints};
use morpholog_surface::{DeclKind, SourceMap, line_col, parse_program_with_sources};

const SOURCE: &str = r#"
program locating

predicate Account(account_id: Subject, balance: Decimal)
predicate Posting(posting_id: Subject, account_id: Subject, amount: Decimal)
    append only
predicate CurrentLabel(account_id: Subject, label_id: Subject)
    current pointer by (account_id)
predicate AccountActivity(account_id: Subject, activity: Decimal)

intent NotifyAudit(account_id: Subject)

define has_posting(account_id):
    Posting(_, account_id, _)

invariant postings_need_label:
    Posting(p, acct, amt) implies CurrentLabel(acct, _)

transformation post(posting_id, account_id, amount):
    require Account(account_id, _)
    admit Posting(posting_id, account_id, amount)
    emit NotifyAudit(account_id)

derived AccountActivity(account_id):
    over Posting(_, account_id, _)
    value activity = sum(amt | Posting(_, account_id, amt))
"#;

fn parsed() -> (morpholog_core::Program, SourceMap) {
    let (program, map) = parse_program_with_sources(SOURCE).expect("source should parse");
    program.validate().expect("source should validate");
    (program, map)
}

fn text_at(span: std::ops::Range<usize>) -> &'static str {
    &SOURCE[span]
}

#[test]
fn every_declaration_kind_is_mapped() {
    let (_, map) = parsed();
    let cases = [
        (DeclKind::Predicate, "Account", "predicate Account"),
        (DeclKind::Intent, "NotifyAudit", "intent NotifyAudit"),
        (DeclKind::Definition, "has_posting", "define has_posting"),
        (
            DeclKind::Invariant,
            "postings_need_label",
            "invariant postings_need_label",
        ),
        (DeclKind::Transformation, "post", "transformation post"),
        (
            DeclKind::DerivedClaim,
            "AccountActivity",
            "derived AccountActivity",
        ),
    ];
    for (kind, name, expected_prefix) in cases {
        let span = map
            .decl_span(kind, name)
            .unwrap_or_else(|| panic!("{kind:?} `{name}` should be mapped"));
        assert!(
            text_at(span.clone()).starts_with(expected_prefix),
            "{kind:?} `{name}` span points at: {:?}",
            text_at(span)
        );
    }
}

#[test]
fn statement_spans_follow_body_order() {
    let (_, map) = parsed();
    let prefixes = ["require Account", "admit Posting", "emit NotifyAudit"];
    for (i, prefix) in prefixes.iter().enumerate() {
        let span = map
            .statement_span("post", i)
            .unwrap_or_else(|| panic!("statement {i} should be mapped"));
        assert!(
            text_at(span.clone()).starts_with(prefix),
            "statement {i} span points at: {:?}",
            text_at(span)
        );
    }
    assert_eq!(map.statement_span("post", 3), None);
    assert_eq!(map.statement_span("no_such_transformation", 0), None);
}

// End-to-end: a real validation failure resolves through its context
// to the declaration that contains it.
#[test]
fn a_context_carrying_error_resolves_through_its_context() {
    let source = r#"
program broken

predicate Account(account_id: Subject, balance: Decimal)

invariant refers_to_nothing:
    Ghost(account_id)
"#;
    let (program, map) = parse_program_with_sources(source).expect("parses; validation fails");
    let errors = program.validate().expect_err("Ghost is undeclared");
    let span = map
        .span_for_error(&errors[0])
        .expect("the invariant is in source");
    assert!(
        source[span.clone()].starts_with("invariant refers_to_nothing"),
        "span points at: {:?}",
        &source[span]
    );
}

// A finding made inside a transformation body resolves to the
// statement it was made in, via the statement index the check
// carries in its context.
#[test]
fn a_statement_level_error_resolves_to_the_statement() {
    let source = r#"
program broken

predicate Account(account_id: Subject, balance: Decimal)

transformation touch(account_id):
    require Account(account_id, _)
    admit Account(account_id, mystery)
"#;
    let (program, map) = parse_program_with_sources(source).expect("parses; validation fails");
    let errors = program.validate().expect_err("mystery is unbound");
    let unbound = errors
        .iter()
        .find(|e| matches!(e, ValidationError::UnboundVariable { .. }))
        .expect("the unbound-variable error is among them");
    assert!(
        unbound.to_string().contains("statement 2"),
        "the rendered error names the statement: {unbound}"
    );
    let span = map.span_for_error(unbound).expect("statement is in source");
    assert!(
        source[span.clone()].starts_with("admit Account"),
        "span points at: {:?}",
        &source[span]
    );
}

#[test]
fn declaration_naming_errors_resolve_by_name() {
    let (_, map) = parsed();
    let parameter_error = ValidationError::ParameterNotReferenced {
        definition: "has_posting".to_string(),
        parameter: "ghost".to_string(),
    };
    let span = map.span_for_error(&parameter_error).expect("mapped");
    assert!(text_at(span).starts_with("define has_posting"));

    let discipline_error = ValidationError::DisciplineUnknownField {
        predicate: "Posting".to_string(),
        field: "ghost".to_string(),
    };
    let span = map.span_for_error(&discipline_error).expect("mapped");
    assert!(text_at(span).starts_with("predicate Posting"));
}

// A generated discipline invariant has no source declaration; findings
// against it resolve to None and render as plain text.
#[test]
fn findings_against_generated_invariants_resolve_to_none() {
    let (program, map) = parsed();
    let generated = program
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "current_label_unique_by_account_id")
        .expect("lowering generated the uniqueness invariant");
    let error = ValidationError::NestingTooDeep {
        context: ValidationContext::Invariant {
            name: generated.name.to_string(),
        },
    };
    assert_eq!(map.span_for_error(&error), None);
}

#[test]
fn a_lint_resolves_to_its_invariant() {
    let (program, map) = parsed();
    let found = lints(&program);
    assert_eq!(found.len(), 1, "the trip shape is deliberate: {found:?}");
    let span = map.span_for_lint(&found[0]).expect("authored invariant");
    assert!(text_at(span).starts_with("invariant postings_need_label"));
}

#[test]
fn line_col_is_one_based_lines_and_columns() {
    assert_eq!(line_col(SOURCE, 0), (1, 1));
    let intent_offset = SOURCE.find("intent NotifyAudit").unwrap();
    assert_eq!(line_col(SOURCE, intent_offset), (11, 1));
    let require_offset = SOURCE.find("require Account").unwrap();
    assert_eq!(line_col(SOURCE, require_offset), (20, 5));
    assert_eq!(
        line_col(SOURCE, SOURCE.len() + 100),
        line_col(SOURCE, SOURCE.len())
    );
}

#[test]
fn parse_program_and_with_sources_agree() {
    let (program, _) = parse_program_with_sources(SOURCE).expect("parses");
    let plain = morpholog_surface::parse_program(SOURCE).expect("parses");
    assert_eq!(program, plain);
}
