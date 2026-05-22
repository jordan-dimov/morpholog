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
-- top-level comment
program with_comments  -- comment after header

-- comment between decls
predicate Foo(a: Subject) -- comment after decl
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
    for source in ["", "   ", "\n\n\n", "-- just a comment\n"] {
        let errs = parse_program(source).expect_err("should fail");
        assert!(
            errs[0].message.contains("expected `program`"),
            "source {source:?} produced unexpected message: {}",
            errs[0].message
        );
    }
}

// ============================================================
// P3a: invariant declarations
// ============================================================

#[test]
fn parses_invariant_with_simple_body() {
    let source = r#"
program demo
invariant something: Foo(x)
"#;
    let program = parse_program(source).expect("parse should succeed");
    assert_eq!(program.invariants.len(), 1);
    let inv = &program.invariants[0];
    assert_eq!(inv.name, "something");
    assert_eq!(inv.version, 1);
    // Body is the claim Foo(x).
    use morpholog_core::Expr;
    assert!(matches!(&inv.body, Expr::Claim { .. }));
}

#[test]
fn parses_invariant_with_forall_body() {
    let source = r#"
program demo
predicate Line(line_id: Subject)
predicate Netted(line_id: Subject)

invariant all_lines_unnetted:
    forall line in lines: not Netted(line)
"#;
    let program = parse_program(source).expect("parse should succeed");
    assert_eq!(program.invariants.len(), 1);
    use morpholog_core::Expr;
    let inv = &program.invariants[0];
    assert!(matches!(&inv.body, Expr::Forall { .. }));
}

#[test]
fn parses_invariant_with_sum_le_body() {
    // The insurance aggregate-cap rule: sum + proposed <= limit.
    let source = r#"
program demo
invariant within_limit:
    sum(amount | SettlementPaid(claim, amount)) + proposed <= limit
"#;
    let program = parse_program(source).expect("parse should succeed");
    use morpholog_core::Expr;
    let inv = &program.invariants[0];
    assert!(matches!(&inv.body, Expr::Le(_, _)));
}

#[test]
fn invariants_default_to_version_one() {
    let source = "program demo\ninvariant x: Foo(y)\n";
    let program = parse_program(source).expect("parse should succeed");
    assert_eq!(program.invariants[0].version, 1);
}

#[test]
fn parses_multiple_invariants() {
    let source = r#"
program demo
invariant a: P()
invariant b: Q()
invariant c: R()
"#;
    let program = parse_program(source).expect("parse should succeed");
    assert_eq!(program.invariants.len(), 3);
    assert_eq!(program.invariants[0].name, "a");
    assert_eq!(program.invariants[1].name, "b");
    assert_eq!(program.invariants[2].name, "c");
}

#[test]
fn predicates_and_invariants_can_interleave() {
    // No order is imposed: the parser collects in source order
    // and partitions on the post-pass.
    let source = r#"
program demo
predicate Foo(x: Subject)
invariant cap_a: Foo(x)
predicate Bar(y: Subject)
invariant cap_b: Bar(y)
"#;
    let program = parse_program(source).expect("interleaved decls should parse");
    assert_eq!(program.predicates.len(), 2);
    assert_eq!(program.invariants.len(), 2);
    assert_eq!(program.predicates[0].name, "Foo");
    assert_eq!(program.predicates[1].name, "Bar");
    assert_eq!(program.invariants[0].name, "cap_a");
    assert_eq!(program.invariants[1].name, "cap_b");
}

#[test]
fn invariant_after_predicate_in_natural_order_also_works() {
    // Pin the canonical "predicates first" order too, since
    // format_program emits it that way.
    let source = r#"
program demo
predicate Foo(x: Subject)
predicate Bar(y: Subject)
invariant a: Foo(x)
invariant b: Bar(y)
"#;
    let program = parse_program(source).expect("predicates-first order should parse");
    assert_eq!(program.predicates.len(), 2);
    assert_eq!(program.invariants.len(), 2);
}

#[test]
fn missing_colon_after_invariant_name_is_error() {
    let source = "program demo\ninvariant cap Foo(x)\n";
    let errs = parse_program(source).expect_err("missing colon should fail");
    assert!(!errs.is_empty());
}

#[test]
fn invariant_without_body_is_error() {
    let source = "program demo\ninvariant cap:\n";
    let errs = parse_program(source).expect_err("missing body should fail");
    assert!(!errs.is_empty());
}

#[test]
fn duplicate_invariant_carries_both_spans() {
    let source = r#"
program demo
invariant cap: Foo(x)
invariant cap: Bar(y)
"#;
    let errs = parse_program(source).expect_err("duplicate should fail");
    let dup = errs
        .iter()
        .find(|e| e.message.contains("duplicate invariant"))
        .expect("expected duplicate-invariant diagnostic");
    assert!(
        dup.message.contains("cap"),
        "diagnostic should name the duplicated invariant"
    );
    assert!(
        !dup.secondary.is_empty(),
        "duplicate diagnostic should carry a secondary span"
    );
    assert_ne!(dup.primary, dup.secondary[0].0);
}

#[test]
fn version_syntax_is_rejected() {
    // P3a deliberately does not have version syntax. `(v1)` after
    // the invariant name fails with an unexpected-token error on
    // the `(`. When versioning gains real meaning, both formatter
    // and parser grow the clause together.
    let source = "program demo\ninvariant cap(v1): Foo(x)\n";
    let errs = parse_program(source).expect_err("version syntax should fail in v0");
    assert!(!errs.is_empty());
}

#[test]
fn invariant_cannot_use_reserved_keyword_as_name() {
    // `program`, `predicate`, `invariant`, and the others are
    // lexer-reserved. Using one as an invariant name fails because
    // the lexer never produces an Ident for it.
    let source = "program demo\ninvariant invariant: Foo(x)\n";
    let errs = parse_program(source).expect_err("reserved keyword as name should fail");
    assert!(!errs.is_empty());
}

// ============================================================
// P3b1: transformation declarations + gate statements
// ============================================================

#[test]
fn parses_transformation_with_zero_params() {
    let source = "program demo\n\
                  transformation noop():\n\
                  \x20\x20\x20\x20require Foo()\n";
    let program = parse_program(source).expect("parse should succeed");
    assert_eq!(program.transformations.len(), 1);
    let t = &program.transformations[0];
    assert_eq!(t.name, "noop");
    assert!(t.parameters.is_empty());
    assert_eq!(t.body.len(), 1);
}

#[test]
fn parses_transformation_with_params() {
    let source = "program demo\n\
                  transformation foo(x, y, z):\n\
                  \x20\x20\x20\x20require Bar(x)\n";
    let program = parse_program(source).expect("parse should succeed");
    let t = &program.transformations[0];
    assert_eq!(t.parameters, vec!["x", "y", "z"]);
}

#[test]
fn parses_require_statement() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20require Foo(x)\n";
    let program = parse_program(source).expect("parse should succeed");
    use morpholog_core::Stmt;
    assert!(matches!(
        program.transformations[0].body[0],
        Stmt::Require(_)
    ));
}

#[test]
fn parses_bind_statement() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20bind Foo(x, y)\n";
    let program = parse_program(source).expect("parse should succeed");
    use morpholog_core::Stmt;
    assert!(matches!(
        program.transformations[0].body[0],
        Stmt::BindOne(_)
    ));
}

#[test]
fn parses_let_statement_with_expression() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20let z = sum(amount | Foo(amount))\n";
    let program = parse_program(source).expect("parse should succeed");
    use morpholog_core::Stmt;
    let stmt = &program.transformations[0].body[0];
    let Stmt::Let { name, .. } = stmt else {
        panic!("expected Stmt::Let, got {stmt:?}");
    };
    assert_eq!(name, "z");
}

#[test]
fn parses_let_new_subject() {
    let source = "program demo\n\
                  transformation t():\n\
                  \x20\x20\x20\x20let s = new Subject()\n";
    let program = parse_program(source).expect("parse should succeed");
    use morpholog_core::Stmt;
    let stmt = &program.transformations[0].body[0];
    let Stmt::LetNewSubject { name } = stmt else {
        panic!("expected Stmt::LetNewSubject, got {stmt:?}");
    };
    assert_eq!(name, "s");
}

#[test]
fn statement_order_is_preserved() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20bind Foo(x, y)\n\
                  \x20\x20\x20\x20let z = y\n\
                  \x20\x20\x20\x20require z <= 10\n";
    let program = parse_program(source).expect("parse should succeed");
    use morpholog_core::Stmt;
    let body = &program.transformations[0].body;
    assert_eq!(body.len(), 3);
    assert!(matches!(body[0], Stmt::BindOne(_)));
    assert!(matches!(body[1], Stmt::Let { .. }));
    assert!(matches!(body[2], Stmt::Require(_)));
}

#[test]
fn transformation_can_interleave_with_predicates_and_invariants() {
    let source = "program demo\n\
                  predicate Foo(x: Subject)\n\
                  transformation make_foo(x):\n\
                  \x20\x20\x20\x20require Foo(x)\n\
                  invariant cap: Foo(_)\n\
                  predicate Bar(y: Decimal)\n";
    let program = parse_program(source).expect("interleaved parse should succeed");
    assert_eq!(program.predicates.len(), 2);
    assert_eq!(program.invariants.len(), 1);
    assert_eq!(program.transformations.len(), 1);
}

#[test]
fn duplicate_transformation_carries_both_spans() {
    let source = "program demo\n\
                  transformation t():\n\
                  \x20\x20\x20\x20require A()\n\
                  transformation t():\n\
                  \x20\x20\x20\x20require B()\n";
    let errs = parse_program(source).expect_err("duplicate transformation should fail");
    let dup = errs
        .iter()
        .find(|e| e.message.contains("duplicate transformation"))
        .expect("expected a duplicate-transformation diagnostic");
    assert!(dup.message.contains("`t`"));
    assert!(!dup.secondary.is_empty());
    assert_ne!(dup.primary, dup.secondary[0].0);
}

#[test]
fn admit_in_p3b1_is_rejected() {
    // P3b1 deliberately does NOT include admit / retract / emit /
    // for. Trying them should fail at parse with an unexpected-
    // token diagnostic; statement parsing will be extended in P3b2.
    //
    // Note: `admit` is a lexer-reserved keyword now (since it's a
    // surface verb the doctrine commits to), but the P3b1 statement
    // parser does not accept it.
    //
    // For this test we use a more obviously-rejected form: the
    // outer statement is recognised but `admit` inside the body
    // surfaces as an error.
    let source = "program demo\n\
                  transformation t():\n\
                  \x20\x20\x20\x20admit Foo()\n";
    let errs = parse_program(source).expect_err("admit should be unrecognised in P3b1");
    assert!(!errs.is_empty());
}

#[test]
fn reserved_keyword_cannot_be_transformation_name() {
    // Trying to use `predicate` as a transformation name fails:
    // the lexer recognises `predicate` as Token::KwPredicate, so
    // the parser never sees an Ident at that position.
    let source = "program demo\n\
                  transformation predicate():\n\
                  \x20\x20\x20\x20require A()\n";
    let errs = parse_program(source).expect_err("reserved keyword as name should fail");
    assert!(!errs.is_empty());
}

// ============================================================
// PR #62 review tightenings
// ============================================================

/// `bind` accepts only a claim pattern. Arbitrary expressions
/// (booleans, arithmetic, value lookups, etc.) are rejected at
/// the surface even though `Stmt::BindOne` can technically hold
/// any `Expr` in the kernel. See `parser/stmt.rs` module-level
/// doc for the doctrine rationale.
#[test]
fn bind_rejects_boolean_expression() {
    let source = "program demo\n\
                  transformation t():\n\
                  \x20\x20\x20\x20bind not Foo(x)\n";
    let errs = parse_program(source).expect_err("bind not Foo(x) should fail");
    assert!(!errs.is_empty());
}

#[test]
fn bind_rejects_comparison() {
    let source = "program demo\n\
                  transformation t():\n\
                  \x20\x20\x20\x20bind amount <= limit\n";
    let errs = parse_program(source).expect_err("bind amount <= limit should fail");
    assert!(!errs.is_empty());
}

#[test]
fn bind_rejects_value_lookup() {
    let source = "program demo\n\
                  transformation t():\n\
                  \x20\x20\x20\x20bind value Policy(x, _)\n";
    let errs = parse_program(source).expect_err("bind value Policy(...) should fail");
    assert!(!errs.is_empty());
}

#[test]
fn bind_accepts_claim_pattern() {
    // The valid surface form is a claim pattern: predicate name
    // followed by parenthesised term list.
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20bind Foo(x, y, _)\n";
    let program = parse_program(source).expect("claim-pattern bind should parse");
    use morpholog_core::{Expr, Stmt};
    let body = &program.transformations[0].body;
    assert_eq!(body.len(), 1);
    let Stmt::BindOne(Expr::Claim { predicate, args }) = &body[0] else {
        panic!(
            "expected Stmt::BindOne(Expr::Claim {{ .. }}); got {:?}",
            body[0]
        );
    };
    assert_eq!(predicate, "Foo");
    assert_eq!(args.len(), 3);
}

/// Top-level indentation (a top-level decl line that is not at
/// column 0) currently surfaces as a parse error because the
/// resulting `Indent` token isn't a valid top-level construct.
/// The diagnostic is generic but the behaviour is pinned so any
/// future improvement (e.g. a dedicated "unexpected top-level
/// indentation" diagnostic) lands as a deliberate change.
#[test]
fn unexpected_top_level_indentation_is_rejected() {
    let source = "program demo\n\
                  \x20\x20\x20\x20predicate Foo(x: Subject)\n";
    let errs = parse_program(source).expect_err("top-level indent should fail");
    assert!(!errs.is_empty());
}
