//! Integration tests for the v0 surface parser.
//!
//! Tests use hand-crafted `.morph` text and assert on the parsed
//! [`morpholog_core::Program`]. No `format_program` round-trip is
//! attempted here - that test would force the parser to recognise
//! invariants / transformations / derived claims, which are out of
//! scope for PR P1.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::PredicateArgKind;
use morpholog_surface::parse_program;

#[test]
fn happy_path_one_program_three_predicates() {
    let source = r#"
program billing

predicate Policy(policy_id: Subject, aggregate_limit: Decimal)
predicate ClaimReported(claim_id: Subject, policy_id: Subject, amount: Decimal)
predicate SettlementPaid(claim_id: Subject, paid_at: Date)
"#;
    let program = parse_program(source).expect("parse should succeed");
    assert_eq!(program.name, "billing");
    assert_eq!(program.predicates.len(), 3);
    assert!(program.invariants.is_empty());
    assert!(program.transformations.is_empty());
    assert!(program.derived_claims.is_empty());

    let policy = &program.predicates[0];
    assert_eq!(policy.name, "Policy");
    assert_eq!(policy.args.len(), 2);
    assert_eq!(policy.args[0].name, "policy_id");
    assert_eq!(policy.args[0].kind, PredicateArgKind::Subject);
    assert_eq!(policy.args[1].name, "aggregate_limit");
    assert_eq!(policy.args[1].kind, PredicateArgKind::Decimal);
}

#[test]
fn all_kind_keywords_lex_and_parse() {
    let source = r#"
program kinds_demo

predicate Every(a: Subject, b: Decimal, c: Date, d: Bool, e: Collection, f: Any)
"#;
    let program = parse_program(source).expect("parse should succeed");
    let kinds: Vec<PredicateArgKind> = program.predicates[0].args.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![
            PredicateArgKind::Subject,
            PredicateArgKind::Decimal,
            PredicateArgKind::Date,
            PredicateArgKind::Bool,
            PredicateArgKind::Collection,
            PredicateArgKind::Any,
        ]
    );
}

#[test]
fn trailing_comma_in_arg_list_allowed() {
    let source = r#"
program demo
predicate Foo(a: Subject, b: Decimal,)
"#;
    let program = parse_program(source).expect("trailing comma should parse");
    assert_eq!(program.predicates[0].args.len(), 2);
}

#[test]
fn line_comments_are_skipped() {
    let source = r#"
// top-level comment
program with_comments  // comment after header

// comment between decls
predicate Foo(a: Subject) // comment after decl
predicate Bar(b: Decimal)
"#;
    let program = parse_program(source).expect("comments should be skipped");
    assert_eq!(program.name, "with_comments");
    assert_eq!(program.predicates.len(), 2);
    assert_eq!(program.predicates[0].name, "Foo");
    assert_eq!(program.predicates[1].name, "Bar");
}

#[test]
fn empty_arg_list_is_allowed() {
    let source = r#"
program demo
predicate Marker()
"#;
    let program = parse_program(source).expect("empty arg list should parse");
    assert_eq!(program.predicates[0].name, "Marker");
    assert!(program.predicates[0].args.is_empty());
}

#[test]
fn program_with_no_predicates_is_valid() {
    let source = "program empty_for_now\n";
    let program = parse_program(source).expect("header-only programme should parse");
    assert_eq!(program.name, "empty_for_now");
    assert!(program.predicates.is_empty());
}

#[test]
fn missing_program_header_is_error() {
    let source = "predicate Foo(a: Subject)\n";
    let errs = parse_program(source).expect_err("missing header should fail");
    assert!(!errs.is_empty());
}

#[test]
fn empty_source_is_error() {
    let errs = parse_program("").expect_err("empty source should fail");
    assert!(!errs.is_empty());
    assert!(
        errs[0].message.contains("expected `program`"),
        "expected diagnostic to mention `program` header; got: {}",
        errs[0].message
    );
}

#[test]
fn unknown_kind_keyword_surfaces_as_error() {
    // `Money` is not a known kind keyword; the lexer treats it as
    // an identifier, and the parser then fails to match where a
    // kind token is required.
    let source = r#"
program demo
predicate Foo(amount: Money)
"#;
    let errs = parse_program(source).expect_err("unknown kind should fail");
    assert!(!errs.is_empty());
}

#[test]
fn missing_colon_in_arg_surfaces_as_error() {
    let source = r#"
program demo
predicate Foo(amount Decimal)
"#;
    let errs = parse_program(source).expect_err("missing colon should fail");
    assert!(!errs.is_empty());
}

#[test]
fn duplicate_predicate_carries_both_spans() {
    let source = r#"
program demo
predicate Foo(a: Subject)
predicate Foo(b: Decimal)
"#;
    let errs = parse_program(source).expect_err("duplicate should fail");
    let dup = errs
        .iter()
        .find(|e| e.message.contains("duplicate"))
        .expect("expected a duplicate-predicate diagnostic");
    assert!(
        dup.message.contains("Foo"),
        "diagnostic should name the duplicated predicate"
    );
    assert!(
        !dup.secondary.is_empty(),
        "duplicate diagnostic should carry a secondary span pointing at the first declaration"
    );
    assert!(
        dup.secondary[0].1.contains("previously declared"),
        "secondary note should explain it's the previous declaration"
    );
    // The two spans must be distinct (otherwise the "duplicate"
    // diagnostic is pointing at itself).
    assert_ne!(dup.primary, dup.secondary[0].0);
}

#[test]
fn render_produces_ariadne_output() {
    let source = "predicate Foo(a: Subject)\n";
    let errs = parse_program(source).expect_err("missing header should fail");
    let rendered = errs[0].render("test.morph", source);
    // ariadne emits ANSI-colored output by default; just check
    // that the source name appears and the rendering is non-empty.
    assert!(!rendered.is_empty());
    assert!(rendered.contains("test.morph"));
}

/// Whitespace-only and empty files both report "expected `program`
/// header" via the parser's custom diagnostic, not a confusing lex
/// error about expected punctuation. Regression test for the
/// lexer's trailing-padding handling.
#[test]
fn empty_and_whitespace_only_sources_produce_friendly_error() {
    for source in ["", "   ", "\n\n\n", "// just a comment\n"] {
        let errs = parse_program(source).expect_err("should fail");
        assert!(
            errs[0].message.contains("expected `program`"),
            "source {source:?} produced unexpected message: {}",
            errs[0].message
        );
    }
}
