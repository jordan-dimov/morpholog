//! Integration tests for the v0 surface parser.
//!
//! Tests use hand-crafted `.morph` text and assert on the parsed
//! [`morpholog_core::Program`]. No `format_program` round-trip is
//! attempted here - that test would force the parser to recognise
//! invariants / transformations / derived claims, which are out of
//! scope for the predicate-declaration grammar.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::PredicateArgKind;
use morpholog_surface::parse_program;

// ---- Test machinery: one case per source the parser must refuse ----
//
// `parse_err!` pins whole-programme sources that must fail to parse,
// with any diagnostic. Tests that inspect a specific diagnostic
// message or its spans stay written out in full.
macro_rules! parse_err {
    ($name:ident, $src:expr) => {
        #[test]
        fn $name() {
            match parse_program($src) {
                Err(errs) => assert!(!errs.is_empty(), "source: {}", $src),
                Ok(_) => panic!("source should fail to parse:\n{}", $src),
            }
        }
    };
}

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
    let kinds: Vec<PredicateArgKind> = program.predicates[0]
        .args
        .iter()
        .map(|a| a.kind.clone())
        .collect();
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
fn quantity_kind_parses_with_unit_brackets() {
    let source = r#"program cargo

predicate Parcel(parcel: Subject, qty: Decimal[t])
predicate Rate(voyage: Subject, daily: Decimal[USD])
"#;
    let program = parse_program(source).expect("parse should succeed");
    assert_eq!(
        program.predicates[0].args[1].kind,
        PredicateArgKind::Quantity(morpholog_core::Unit::from("t")),
    );
    assert_eq!(
        program.predicates[1].args[1].kind,
        PredicateArgKind::Quantity(morpholog_core::Unit::from("USD")),
    );
}

#[test]
fn quantity_literals_parse_in_statement_and_expression_position() {
    // A numeric literal followed by an identifier in term position is
    // a quantity literal: whole, fractional, and the zero a unitful
    // aggregate seeds with. The invariant exercises expression
    // position; the admits exercise statement-arg position.
    let source = r#"program cargo

predicate Parcel(parcel: Subject, qty: Decimal[t])

invariant parcel_within_largest_hold:
    Parcel(p, q) implies q <= 50000 t

transformation load(parcel):
    admit Parcel(parcel, 25000 t)

transformation seed(parcel):
    admit Parcel(parcel, 0 t)

transformation precise(parcel):
    admit Parcel(parcel, 100.50 t)
"#;
    let program = parse_program(source).expect("parse should succeed");
    assert!(program.validate().is_ok(), "{:?}", program.validate());
    // Round-trip: the canonical rendering reparses to the same IR, so
    // the formatter and parser agree on both the kind annotation and
    // the literal.
    let rendered = morpholog_core::format::format_program(&program);
    let reparsed = parse_program(&rendered)
        .unwrap_or_else(|e| panic!("canonical text must reparse: {e:?}\n{rendered}"));
    assert_eq!(program, reparsed);
}

#[test]
fn unit_brackets_on_a_non_decimal_kind_are_a_parse_error() {
    let source = r#"program bad

predicate P(x: Subject[USD])
"#;
    let errs = parse_program(source).expect_err("unit on Subject must not parse");
    assert!(
        errs.iter()
            .any(|e| format!("{e:?}").contains("only `Decimal` takes a unit annotation")),
        "expected the only-Decimal diagnostic; got {errs:?}"
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

parse_err!(
    missing_program_header_is_error,
    "predicate Foo(a: Subject)\n"
);

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

// `Money` is not a known kind keyword; the lexer treats it as an
// identifier, and the parser then fails to match where a kind token
// is required.
parse_err!(
    unknown_kind_keyword_surfaces_as_error,
    "\nprogram demo\npredicate Foo(amount: Money)\n"
);

parse_err!(
    missing_colon_in_arg_surfaces_as_error,
    "\nprogram demo\npredicate Foo(amount Decimal)\n"
);

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
// Invariant declarations
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
    use morpholog_core::Prop;
    assert!(matches!(&inv.body, Prop::Claim { .. }));
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
    use morpholog_core::Prop;
    let inv = &program.invariants[0];
    assert!(matches!(&inv.body, Prop::Forall { .. }));
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
    use morpholog_core::{CompareOp, OrderedDomain, Prop};
    let inv = &program.invariants[0];
    assert!(matches!(
        &inv.body,
        Prop::Compare {
            op: CompareOp::Le,
            domain: OrderedDomain::Decimal,
            ..
        }
    ));
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

parse_err!(
    missing_colon_after_invariant_name_is_error,
    "program demo\ninvariant cap Foo(x)\n"
);

parse_err!(
    invariant_without_body_is_error,
    "program demo\ninvariant cap:\n"
);

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

// The parser deliberately does not have version syntax. `(v1)` after
// the invariant name fails with an unexpected-token error on the `(`.
// When versioning gains real meaning, both formatter and parser grow
// the clause together.
parse_err!(
    version_syntax_is_rejected,
    "program demo\ninvariant cap(v1): Foo(x)\n"
);

// `program`, `predicate`, `invariant`, and the others are
// lexer-reserved. Using one as a declaration name fails because the
// lexer never produces an Ident for it.
parse_err!(
    invariant_cannot_use_reserved_keyword_as_name,
    "program demo\ninvariant invariant: Foo(x)\n"
);

// ============================================================
// Transformation declarations + gate statements
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
        Stmt::Require { .. }
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
        Stmt::BindOne { .. }
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
    assert_eq!(name.as_str(), "z");
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
    assert_eq!(name.as_str(), "s");
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
    assert!(matches!(body[0], Stmt::BindOne { .. }));
    assert!(matches!(body[1], Stmt::Let { .. }));
    assert!(matches!(body[2], Stmt::Require { .. }));
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

// ============================================================
// State-mutating statements + iteration
// ============================================================

#[test]
fn parses_admit_statement() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20admit Foo(x)\n";
    let program = parse_program(source).expect("admit should parse");
    use morpholog_core::Stmt;
    let body = &program.transformations[0].body;
    let Stmt::Assert(claim) = &body[0] else {
        panic!("expected Stmt::Assert, got {:?}", body[0]);
    };
    assert_eq!(claim.predicate.as_str(), "Foo");
    assert_eq!(claim.args.len(), 1);
}

parse_err!(
    reserved_keyword_cannot_be_transformation_name,
    "program demo\n\
     transformation predicate():\n\
     \x20\x20\x20\x20require A()\n"
);

// ============================================================
// Review tightenings: bind is parser-restricted to a claim pattern
// ============================================================

// `bind` accepts only a claim pattern. Arbitrary propositions
// (booleans, comparisons, etc.) are rejected at the surface even
// though `Stmt::BindOne` can technically hold any `Prop` in the
// kernel. See `parser/stmt.rs` module-level doc for the doctrine
// rationale.
parse_err!(
    bind_rejects_boolean_expression,
    "program demo\n\
     transformation t():\n\
     \x20\x20\x20\x20bind not Foo(x)\n"
);

parse_err!(
    bind_rejects_comparison,
    "program demo\n\
     transformation t():\n\
     \x20\x20\x20\x20bind amount <= limit\n"
);

parse_err!(
    bind_rejects_value_lookup,
    "program demo\n\
     transformation t():\n\
     \x20\x20\x20\x20bind value Policy(x, _)\n"
);

#[test]
fn bind_accepts_claim_pattern() {
    // The valid surface form is a claim pattern: predicate name
    // followed by parenthesised term list.
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20bind Foo(x, y, _)\n";
    let program = parse_program(source).expect("claim-pattern bind should parse");
    use morpholog_core::{Prop, Stmt};
    let body = &program.transformations[0].body;
    assert_eq!(body.len(), 1);
    let Stmt::BindOne {
        prop: Prop::Claim { predicate, args },
        ..
    } = &body[0]
    else {
        panic!("expected a bind of Prop::Claim {{ .. }}; got {:?}", body[0]);
    };
    assert_eq!(predicate.as_str(), "Foo");
    assert_eq!(args.len(), 3);
}

// Top-level indentation (a top-level decl line that is not at
// column 0) currently surfaces as a parse error because the
// resulting `Indent` token isn't a valid top-level construct.
// The diagnostic is generic but the behaviour is pinned so any
// future improvement (e.g. a dedicated "unexpected top-level
// indentation" diagnostic) lands as a deliberate change.
parse_err!(
    unexpected_top_level_indentation_is_rejected,
    "program demo\n\
     \x20\x20\x20\x20predicate Foo(x: Subject)\n"
);

#[test]
fn parses_retract_statement() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20retract Foo(x, _)\n";
    let program = parse_program(source).expect("retract should parse");
    use morpholog_core::Stmt;
    let body = &program.transformations[0].body;
    let Stmt::Retract { predicate, args } = &body[0] else {
        panic!("expected Stmt::Retract, got {:?}", body[0]);
    };
    assert_eq!(predicate.as_str(), "Foo");
    assert_eq!(args.len(), 2);
    use morpholog_core::Term;
    assert!(matches!(args[1], Term::Wildcard));
}

#[test]
fn parses_emit_statement() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20emit Notify(x)\n";
    let program = parse_program(source).expect("emit should parse");
    use morpholog_core::Stmt;
    let body = &program.transformations[0].body;
    let Stmt::Emit(intent) = &body[0] else {
        panic!("expected Stmt::Emit, got {:?}", body[0]);
    };
    assert_eq!(intent.name, "Notify");
    assert_eq!(intent.args.len(), 1);
}

#[test]
fn parses_for_block_with_single_statement() {
    let source = "program demo\n\
                  transformation t(items):\n\
                  \x20\x20\x20\x20for item in items:\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20admit Foo(item)\n";
    let program = parse_program(source).expect("for block should parse");
    use morpholog_core::Stmt;
    let body = &program.transformations[0].body;
    let Stmt::For {
        binding,
        body: for_body,
        ..
    } = &body[0]
    else {
        panic!("expected Stmt::For, got {:?}", body[0]);
    };
    assert_eq!(binding.as_str(), "item");
    assert_eq!(for_body.len(), 1);
    assert!(matches!(for_body[0], Stmt::Assert(_)));
}

#[test]
fn for_body_preserves_statement_order() {
    let source = "program demo\n\
                  transformation t(items):\n\
                  \x20\x20\x20\x20for item in items:\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20bind Foo(item, x)\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20require x <= 100\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20admit Bar(item, x)\n";
    let program = parse_program(source).expect("multi-statement for body should parse");
    use morpholog_core::Stmt;
    let Stmt::For { body: for_body, .. } = &program.transformations[0].body[0] else {
        panic!("expected Stmt::For");
    };
    assert_eq!(for_body.len(), 3);
    assert!(matches!(for_body[0], Stmt::BindOne { .. }));
    assert!(matches!(for_body[1], Stmt::Require { .. }));
    assert!(matches!(for_body[2], Stmt::Assert(_)));
}

#[test]
fn nested_for_blocks_parse() {
    // for x in xs:
    //     for y in ys:
    //         admit P(x, y)
    let source = "program demo\n\
                  transformation t(xs, ys):\n\
                  \x20\x20\x20\x20for x in xs:\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20for y in ys:\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20admit P(x, y)\n";
    let program = parse_program(source).expect("nested for should parse");
    use morpholog_core::Stmt;
    let Stmt::For {
        body: outer_body, ..
    } = &program.transformations[0].body[0]
    else {
        panic!("expected outer Stmt::For");
    };
    assert_eq!(outer_body.len(), 1);
    let Stmt::For {
        body: inner_body, ..
    } = &outer_body[0]
    else {
        panic!("expected inner Stmt::For");
    };
    assert!(matches!(inner_body[0], Stmt::Assert(_)));
}

#[test]
fn for_inside_mixed_transformation_body() {
    // Top-level transformation body mixes a for-block with other
    // statements before and after. Confirms the parser resumes at
    // the outer statement level after the `for` block's Dedent.
    let source = "program demo\n\
                  transformation t(claim, items):\n\
                  \x20\x20\x20\x20require Claim(claim)\n\
                  \x20\x20\x20\x20for item in items:\n\
                  \x20\x20\x20\x20\x20\x20\x20\x20admit Line(claim, item)\n\
                  \x20\x20\x20\x20admit Settled(claim)\n";
    let program = parse_program(source).expect("mixed body should parse");
    use morpholog_core::Stmt;
    let body = &program.transformations[0].body;
    assert_eq!(body.len(), 3);
    assert!(matches!(body[0], Stmt::Require { .. }));
    assert!(matches!(body[1], Stmt::For { .. }));
    assert!(matches!(body[2], Stmt::Assert(_)));
}

// A `for ... :` with no body content (immediately followed by
// outer-level statements) fails to parse because the body production
// requires at least one statement.
parse_err!(
    empty_for_body_is_rejected,
    "program demo\n\
     transformation t(items):\n\
     \x20\x20\x20\x20for item in items:\n\
     \x20\x20\x20\x20admit Done()\n"
);

// Statements outside a transformation body are not legal top-level
// declarations.
parse_err!(top_level_admit_is_rejected, "program demo\nadmit Foo()\n");

parse_err!(
    top_level_for_is_rejected,
    "program demo\n\
     for x in xs:\n\
     \x20\x20\x20\x20admit Foo(x)\n"
);

// Admit/emit reject wildcards at parse time because the kernel
// rejects them at runtime; the parser refuses to produce IR the
// kernel will refuse to evaluate.

#[test]
fn admit_rejects_wildcard_arg() {
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20admit Foo(x, _)\n";
    let errs = parse_program(source).expect_err("admit with wildcard should fail");
    assert!(
        errs.iter()
            .any(|d| d.message.contains("wildcard") && d.message.contains("`admit`")),
        "expected wildcard-in-admit diagnostic; got: {errs:?}"
    );
}

#[test]
fn emit_rejects_wildcard_arg() {
    let source = "program demo\n\
                  transformation t():\n\
                  \x20\x20\x20\x20emit Notify(_)\n";
    let errs = parse_program(source).expect_err("emit with wildcard should fail");
    assert!(
        errs.iter()
            .any(|d| d.message.contains("wildcard") && d.message.contains("`emit`")),
        "expected wildcard-in-emit diagnostic; got: {errs:?}"
    );
}

#[test]
fn retract_still_accepts_wildcard_arg() {
    // Wildcards are MEANINGFUL in retract (pattern-based
    // retraction). The surface preserves that.
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20retract Foo(x, _)\n";
    let program = parse_program(source).expect("retract with wildcard should parse");
    use morpholog_core::{Stmt, Term};
    let Stmt::Retract { args, .. } = &program.transformations[0].body[0] else {
        panic!("expected Stmt::Retract");
    };
    assert!(matches!(args[1], Term::Wildcard));
}

#[test]
fn bind_still_accepts_wildcard_arg() {
    // Wildcards are meaningful in bind (matching any value at
    // that position while extracting other positions). Preserved.
    let source = "program demo\n\
                  transformation t(x):\n\
                  \x20\x20\x20\x20bind Foo(x, _, y)\n";
    let program = parse_program(source).expect("bind with wildcard should parse");
    use morpholog_core::Stmt;
    assert!(matches!(
        program.transformations[0].body[0],
        Stmt::BindOne { .. }
    ));
}

// ============================================================
// Derived claims
// ============================================================

#[test]
fn parses_simple_derived_claim() {
    let source = "program demo\n\
                  predicate Foo(x: Subject, amount: Decimal)\n\
                  \n\
                  derived Total(x):\n\
                  \x20\x20\x20\x20over Foo(x, _)\n\
                  \x20\x20\x20\x20value sum_amount = sum(a | Foo(x, a))\n";
    let program = parse_program(source).expect("derived claim should parse");
    assert_eq!(program.derived_claims.len(), 1);
    let d = &program.derived_claims[0];
    assert_eq!(d.predicate.as_str(), "Total");
    assert_eq!(d.keys, vec!["x"]);
    assert_eq!(d.values.len(), 1);
    assert_eq!(d.values[0].name, "sum_amount");
}

#[test]
fn parses_derived_with_multiple_values() {
    let source = "program demo\n\
                  predicate Debit(account: Subject, amount: Decimal)\n\
                  predicate Credit(account: Subject, amount: Decimal)\n\
                  \n\
                  derived AccountSummary(account):\n\
                  \x20\x20\x20\x20over Debit(account, _)\n\
                  \x20\x20\x20\x20value total_debits = sum(d | Debit(account, d))\n\
                  \x20\x20\x20\x20value total_credits = sum(c | Credit(account, c))\n";
    let program = parse_program(source).expect("multi-value derived should parse");
    let d = &program.derived_claims[0];
    assert_eq!(d.values.len(), 2);
    assert_eq!(d.values[0].name, "total_debits");
    assert_eq!(d.values[1].name, "total_credits");
}

#[test]
fn parses_derived_with_multiple_keys() {
    let source = "program demo\n\
                  predicate Posting(account: Subject, period: Subject, amount: Decimal)\n\
                  \n\
                  derived PeriodRow(account, period):\n\
                  \x20\x20\x20\x20over Posting(account, period, _)\n\
                  \x20\x20\x20\x20value total = sum(a | Posting(account, period, a))\n";
    let program = parse_program(source).expect("multi-key derived should parse");
    let d = &program.derived_claims[0];
    assert_eq!(d.keys, vec!["account", "period"]);
}

parse_err!(
    derived_with_no_value_clauses_is_rejected,
    "program demo\n\
     predicate Foo(x: Subject)\n\
     \n\
     derived Empty(x):\n\
     \x20\x20\x20\x20over Foo(x)\n"
);

#[test]
fn duplicate_derived_name_carries_both_spans() {
    let source = "program demo\n\
                  predicate Foo(x: Subject, a: Decimal)\n\
                  \n\
                  derived Total(x):\n\
                  \x20\x20\x20\x20over Foo(x, _)\n\
                  \x20\x20\x20\x20value v = sum(a | Foo(x, a))\n\
                  \n\
                  derived Total(x):\n\
                  \x20\x20\x20\x20over Foo(x, _)\n\
                  \x20\x20\x20\x20value v = sum(a | Foo(x, a))\n";
    let errs = parse_program(source).expect_err("duplicate derived should fail");
    let dup = errs
        .iter()
        .find(|d| d.message.contains("duplicate derived"))
        .expect("expected duplicate diagnostic");
    assert!(!dup.secondary.is_empty());
}

#[test]
fn derived_can_interleave_with_other_top_level_decls() {
    let source = "program demo\n\
                  predicate Foo(x: Subject, a: Decimal)\n\
                  \n\
                  derived Total(x):\n\
                  \x20\x20\x20\x20over Foo(x, _)\n\
                  \x20\x20\x20\x20value v = sum(a | Foo(x, a))\n\
                  \n\
                  predicate Bar(y: Subject)\n";
    let program = parse_program(source).expect("interleaved decls should parse");
    assert_eq!(program.predicates.len(), 2);
    assert_eq!(program.derived_claims.len(), 1);
}

#[test]
fn duplicate_keys_in_derived_are_rejected() {
    let source = "program demo\n\
                  predicate Foo(x: Subject, a: Decimal)\n\
                  \n\
                  derived Test(x, x):\n\
                  \x20\x20\x20\x20over Foo(x, _)\n\
                  \x20\x20\x20\x20value v = sum(a | Foo(x, a))\n";
    let errs = parse_program(source).expect_err("duplicate keys should fail");
    assert!(
        errs.iter().any(|d| d.message.contains("duplicate key")),
        "expected duplicate-key diagnostic; got: {errs:?}"
    );
}

#[test]
fn duplicate_value_names_in_derived_are_rejected() {
    let source = "program demo\n\
                  predicate Foo(x: Subject, a: Decimal)\n\
                  \n\
                  derived Test(x):\n\
                  \x20\x20\x20\x20over Foo(x, _)\n\
                  \x20\x20\x20\x20value v = sum(a | Foo(x, a))\n\
                  \x20\x20\x20\x20value v = sum(a | Foo(x, a))\n";
    let errs = parse_program(source).expect_err("duplicate values should fail");
    assert!(
        errs.iter()
            .any(|d| d.message.contains("duplicate value name")),
        "expected duplicate-value-name diagnostic; got: {errs:?}"
    );
}

// ============================================================
// Time values: timestamp literals, duration constructor, the
// instant/span comparators, and the formatter round-trip.
// ============================================================

const TIME_SOURCE: &str = "program time_demo
predicate NorTendered(voyage: Subject, at: Timestamp)
predicate Commenced(voyage: Subject, at: Timestamp)
predicate CountingInterval(interval: Subject, voyage: Subject, len: Duration)
predicate AllowedLaytime(voyage: Subject, allowed: Duration)

invariant commencement_not_before_nor:
    Commenced(v, c) and NorTendered(v, n) implies n at_or_before c

invariant counted_within_allowance:
    AllowedLaytime(v, a) implies sum(len | CountingInterval(_, v, len)) no_longer_than a

transformation commence_after_turn(voyage):
    bind NorTendered(voyage, n)
    let c = n + duration(PT6H)
    admit Commenced(voyage, c)

transformation tender_at_noon(voyage):
    admit NorTendered(voyage, @2026-10-24T12:00:00Z)
";

#[test]
fn time_programme_parses_and_validates() {
    let program = parse_program(TIME_SOURCE).expect("time programme should parse");
    assert!(
        program.validate().is_ok(),
        "should validate: {:?}",
        program.validate()
    );
    assert_eq!(program.invariants.len(), 2);
}

#[test]
fn time_programme_round_trips_through_the_formatter() {
    let program = parse_program(TIME_SOURCE).expect("parse");
    let formatted = morpholog_core::format::format_program(&program);
    let reparsed = parse_program(&formatted)
        .unwrap_or_else(|e| panic!("formatted source should reparse; got {e:?}\n{formatted}"));
    assert_eq!(reparsed, program, "round-trip must be lossless");
}

#[test]
fn malformed_duration_literal_is_a_parse_diagnostic() {
    let source = "program bad
predicate P(x: Duration)
transformation t(v):
    admit P(duration(NOT_ISO))
";
    let err = parse_program(source).expect_err("NOT_ISO is not a duration");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("invalid duration literal"),
        "diagnostic should name the bad literal: {msg}"
    );
}

#[test]
fn duration_stays_usable_as_a_variable_name() {
    // The constructor is contextual: `duration` followed by `(` only.
    let source = "program ctx
predicate Holds(duration: Duration)
transformation t(duration):
    admit Holds(duration)
";
    let program = parse_program(source).expect("`duration` as a name should parse");
    assert!(program.validate().is_ok());
}

#[test]
fn timestamp_literal_with_offset_parses() {
    let source = "program offsets
predicate E(at: Timestamp)
transformation t(v):
    admit E(@2026-10-25T01:30:00+01:00)
";
    let program = parse_program(source).expect("offset timestamp should lex and parse");
    assert!(program.validate().is_ok());
}

// The `duration(...)` payload lexes as a single identifier, so a
// leading sign cannot appear. Negative spans arise from arithmetic
// (`a - b`), never from literals; if a model ever genuinely needs a
// negative literal, that example reopens this.
parse_err!(
    negative_duration_literals_are_deliberately_unsupported_in_surface,
    "program neg
predicate P(x: Duration)
transformation t(v):
    admit P(duration(-PT6H))
"
);

#[test]
fn every_time_comparator_form_parses_and_round_trips() {
    // The Le forms are exercised by the laytime programme; this pins
    // the remaining six (and re-pins the two) so no comparator token
    // is dark: each parses to its (op, domain) pair and survives the
    // formatter round-trip.
    let source = "program comparators
predicate E(v: Subject, a: Timestamp, b: Timestamp, x: Duration, y: Duration)

invariant ts_forms:
    E(v, a, b, x, y) implies (a at_or_before b and a strictly_before b and b at_or_after a and b strictly_after a)

invariant dur_forms:
    E(v, a, b, x, y) implies (x no_longer_than y and x shorter_than y and y no_shorter_than x and y longer_than x)
";
    let program = parse_program(source).expect("all comparator forms should parse");
    assert!(program.validate().is_ok(), "{:?}", program.validate());
    let formatted = morpholog_core::format::format_program(&program);
    let reparsed = parse_program(&formatted)
        .unwrap_or_else(|e| panic!("formatted source should reparse; got {e:?}\n{formatted}"));
    assert_eq!(reparsed, program);
}

#[test]
fn conditionals_parse_nest_and_round_trip() {
    // The forcing shape: a compound condition (a claim with commas,
    // `and`, `=`), nested conditionals in BOTH branch positions, and
    // arithmetic precedence around the self-delimiting form.
    let source = "program conds
predicate TariffCharge(charge: Subject, source: Subject)
predicate MeterReading(meter: Subject, qty: Decimal)
predicate Out(line: Subject, v: Decimal)

invariant precedence_and_nesting:
    Out(_, v) and TariffCharge(charge, source) implies v = 1 + if(TariffCharge(charge, #meter) and source = #meter, if(MeterReading(_, _), 2, 3), if(MeterReading(_, _), 4, 5)) * 4

transformation record(line, charge, meter, proposed):
    require TariffCharge(charge, _)
    let applied = if(TariffCharge(charge, #meter), value MeterReading(meter, _), proposed)
    admit Out(line, applied)
";
    let program = parse_program(source).expect("conditionals should parse");
    assert!(program.validate().is_ok(), "{:?}", program.validate());
    let formatted = morpholog_core::format::format_program(&program);
    let reparsed = parse_program(&formatted)
        .unwrap_or_else(|e| panic!("formatted source should reparse; got {e:?}\n{formatted}"));
    assert_eq!(reparsed, program, "round-trip must be lossless");
    // Precedence: the `* 4` binds to the conditional, the `1 +` sits
    // outside - the canonical rendering parenthesises the infix
    // grouping, and the conditional itself needs none.
    assert!(formatted.contains("1 + (if("), "{formatted}");
}

#[test]
fn period_index_parses_in_gates_lets_and_invariants_and_round_trips() {
    let source = "program periods
const anchor = (@2000-04-01)
predicate Run(r: Subject, starts_on: Date, ends_on: Date, year: Decimal)

invariant runs_stay_inside_one_year:
    Run(_, starts_on, ends_on, _) implies period_index(anchor, span(P1Y), starts_on) = period_index(anchor, span(P1Y), ends_on)

transformation open_run(r, starts_on, ends_on):
    require inside_one_year: period_index(anchor, span(P1Y), starts_on) = period_index(anchor, span(P1Y), ends_on)
    let year = period_index(anchor, span(P1Y), starts_on)
    admit Run(r, starts_on, ends_on, year)
";
    let program = parse_program(source).expect("period_index should parse");
    assert!(program.validate().is_ok(), "{:?}", program.validate());
    let formatted = morpholog_core::format::format_program(&program);
    let reparsed = parse_program(&formatted)
        .unwrap_or_else(|e| panic!("formatted source should reparse; got {e:?}\n{formatted}"));
    assert_eq!(reparsed, program, "round-trip must be lossless");
}

#[test]
fn period_index_is_lawful_inside_a_const_initialiser() {
    // Pure over literals, so the consts walk recurses through it: a
    // const-held index substitutes to exactly the IR the hand-inlined
    // expression parses to.
    let via_const = "program epoch
const year = (period_index(@2000-04-01, span(P1Y), @2026-07-01))
predicate Run(r: Subject, year: Decimal)
transformation open_run(r):
    let y = year
    admit Run(r, y)
";
    let inlined = "program epoch
predicate Run(r: Subject, year: Decimal)
transformation open_run(r):
    let y = period_index(@2000-04-01, span(P1Y), @2026-07-01)
    admit Run(r, y)
";
    let a = parse_program(via_const).expect("the const initialiser should parse");
    assert!(a.validate().is_ok(), "{:?}", a.validate());
    let b = parse_program(inlined).expect("the inlined form should parse");
    assert_eq!(
        morpholog_core::format::format_program(&a),
        morpholog_core::format::format_program(&b),
        "const substitution and hand-inlining must canonicalise identically"
    );
}

#[test]
fn period_index_stays_usable_as_a_variable_name() {
    let source = "program ctx
predicate Holds(period_index: Decimal)
transformation t(period_index):
    require period_index + 1 <= 10
    admit Holds(period_index)
";
    let program = parse_program(source).expect("`period_index` as a name should parse");
    assert!(program.validate().is_ok());
}

#[test]
fn a_two_argument_period_index_is_a_parse_error() {
    let source = "program bad
predicate P(v: Decimal)
transformation t(v):
    let x = period_index(@2000-04-01, span(P1Y))
    admit P(x)
";
    parse_program(source)
        .map(|_| ())
        .expect_err("the extractor takes anchor, span, and position");
}

#[test]
fn if_stays_usable_as_a_variable_name() {
    // Contextual: a constructor only when followed by `(`. As a bare
    // identifier - even in arithmetic - it is an ordinary variable.
    let source = "program ctx
predicate Holds(if: Decimal)
transformation t(if):
    require if + 1 <= 10
    admit Holds(if)
";
    let program = parse_program(source).expect("`if` as a name should parse");
    assert!(program.validate().is_ok(), "{:?}", program.validate());
}

#[test]
fn a_two_argument_if_is_a_parse_error() {
    let source = "program bad
predicate P(v: Decimal)
transformation t(v):
    let x = if(P(_), 1)
    admit P(x)
";
    parse_program(source)
        .map(|_| ())
        .expect_err("a conditional takes a proposition and two values");
}

#[test]
fn span_literals_parse_in_value_position_and_round_trip() {
    let source = "program spans
predicate Period(p: Subject, ends_on: Date)
predicate Notice(p: Subject, as_of: Date, days_late: Decimal)

invariant lateness_is_the_records_own_count:
    Notice(p, as_of, days_late) and Period(p, ends_on) implies days_late = as_of - (ends_on + span(P45D))

transformation schedule(p, prior_end, next):
    require Period(p, prior_end)
    let next_end = prior_end + span(P3M)
    admit Period(next, next_end)
";
    let program = parse_program(source).expect("span literals should parse");
    assert!(program.validate().is_ok(), "{:?}", program.validate());
    let formatted = morpholog_core::format::format_program(&program);
    let reparsed = parse_program(&formatted)
        .unwrap_or_else(|e| panic!("formatted source should reparse; got {e:?}\n{formatted}"));
    assert_eq!(reparsed, program, "round-trip must be lossless");
}

#[test]
fn span_stays_usable_as_a_variable_name() {
    // The constructor is contextual: `span` followed by `(` only.
    let source = "program ctx
predicate Holds(span: Decimal)
transformation t(span):
    admit Holds(span)
";
    let program = parse_program(source).expect("`span` as a name should parse");
    assert!(program.validate().is_ok());
}

#[test]
fn a_span_cannot_escape_through_an_any_slot_at_check_time() {
    // The runtime refuses this too, but an authoring mistake known
    // from the programme must not survive `check` and become an
    // operational proposal error. `Any` is kind-compatible with
    // everything, so without the dedicated refusal this validated.
    let source = "program leak
predicate Holds(payload: Any)
transformation leak(l):
    let sp = span(P3M)
    admit Holds(sp)
";
    let program = parse_program(source).expect("parses");
    let errors = program.validate().expect_err("the span must not escape");
    assert!(
        errors
            .iter()
            .any(|e| format!("{e}").contains("cannot leave expression position")),
        "got: {errors:?}"
    );
}

#[test]
fn a_span_cannot_escape_through_an_emit_or_a_derived_value() {
    let emit_source = "program leak_emit
intent Notify(payload: Any)
predicate P(x: Subject)
transformation leak(l):
    let sp = span(P45D)
    admit P(l)
    emit Notify(sp)
";
    let derived_source = "program leak_derived
predicate Period(p: Subject, ends_on: Date)
predicate Out(p: Subject, v: Any)
derived Out(p):
    over Period(p, _)
    value v = span(P3M)
";
    for source in [emit_source, derived_source] {
        let program = parse_program(source).expect("parses");
        let errors = program.validate().expect_err("the span must not escape");
        assert!(
            errors
                .iter()
                .any(|e| format!("{e}").contains("cannot leave expression position")),
            "got: {errors:?}"
        );
    }
}

#[test]
fn a_parameter_inferred_as_a_span_has_no_lawful_call_and_is_refused() {
    // `sp` is only ever used as a span operand, so inference lands it
    // on CalendarSpan - but no transition argument may carry one, so
    // every invocation would be refused. That contradiction is the
    // author's to fix, at check time. (The known side arrives via
    // `bind`: a `require` walks a cloned scope, so kinds observed
    // there do not export - matching the runtime's no-export rule.)
    let source = "program impossible
predicate SomeDate(d: Date)
transformation shift(d, sp):
    bind SomeDate(d)
    let moved = d + sp
    admit SomeDate(moved)
";
    let program = parse_program(source).expect("parses");
    let errors = program.validate().expect_err("the parameter is unfillable");
    assert!(
        errors
            .iter()
            .any(|e| format!("{e}").contains("parameter `sp`")),
        "got: {errors:?}"
    );
}

#[test]
fn a_time_unit_span_literal_names_duration_as_the_fix() {
    let source = "program bad_span
predicate P(d: Date)
transformation t(d):
    require d before d + span(PT6H)
    admit P(d)
";
    let err = parse_program(source).expect_err("PT6H is not a calendar span");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("invalid span literal") && msg.contains("duration("),
        "diagnostic should point at duration(...): {msg}"
    );
}

#[test]
fn a_bare_number_span_payload_is_a_parse_diagnostic() {
    let source = "program bad_span
predicate P(d: Date)
transformation t(d):
    require d before d + span(3)
    admit P(d)
";
    let err = parse_program(source).expect_err("a bare number is not a span");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("invalid span literal"),
        "diagnostic should name the bad literal: {msg}"
    );
}

#[test]
fn the_refused_span_grammar_forms_each_get_a_diagnostic() {
    // Lowercase units, signs, fractions, combined weeks, empty P: the
    // kernel's own grammar refuses each with a named reason; the parse
    // diagnostic carries it.
    for bad in ["P3m", "P1W2D", "P1M1M", "P3M1Y", "P0DT0S"] {
        let source = format!(
            "program bad_span
predicate P(d: Date)
transformation t(d):
    require d before d + span({bad})
    admit P(d)
"
        );
        let err = parse_program(&source)
            .map(|_| ())
            .expect_err(&format!("`{bad}` should be refused"));
        let msg = format!("{err:?}");
        assert!(
            msg.contains("invalid span literal"),
            "`{bad}` diagnostic: {msg}"
        );
    }
}

#[test]
fn an_impossible_calendar_timestamp_is_a_lex_diagnostic() {
    // Shape-valid but not a real instant: month 13. Caught at lex via
    // jiff, the same early-diagnostic treatment `duration(...)` gets.
    let source = "program bad_instant
predicate E(at: Timestamp)
transformation t(v):
    admit E(@2026-13-40T12:00:00Z)
";
    let err = parse_program(source).expect_err("month 13 is not an instant");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("invalid timestamp literal"),
        "diagnostic should name the bad literal: {msg}"
    );
}

#[test]
fn sum_seeds_lower_from_declared_kinds_through_defined_calls() {
    use morpholog_core::{Prop, SumSeed, ValueExpr};
    let src = "\
program seeds
predicate Parcel(p: Subject, qty: Decimal[t])
predicate Interval(i: Subject, len: Duration)
define parcel_qty(p, q):
    Parcel(p, q)
invariant quantity_seed:
    Parcel(x, _) implies sum(q | parcel_qty(_, q)) <= 100 t
invariant duration_seed:
    Interval(x, _) implies sum(len | Interval(_, len)) no_longer_than duration(PT48H)
invariant count_stays_decimal:
    Parcel(x, _) implies sum(1 | Parcel(_, _)) <= 10
invariant literal_target_carries_its_unit:
    Parcel(x, _) implies sum(1 t | Parcel(_, _)) <= 100 t
";
    let program = morpholog_surface::parse_program(src).unwrap();
    let seed_of = |name: &str| {
        let inv = program
            .invariants
            .iter()
            .find(|i| i.name.as_str() == name)
            .unwrap();
        let Prop::Implies { right, .. } = &inv.body else {
            panic!("implication expected");
        };
        let Prop::Compare { left, .. } = right.as_ref() else {
            panic!("comparison expected");
        };
        let ValueExpr::Sum { seed, .. } = left.as_ref() else {
            panic!("sum expected");
        };
        seed.clone()
    };
    // The quantity seed resolves THROUGH the defined call: `q` is bound
    // by `parcel_qty`'s body, not by any claim the invariant names.
    assert_eq!(seed_of("quantity_seed"), SumSeed::Quantity("t".into()));
    assert_eq!(seed_of("duration_seed"), SumSeed::Duration);
    assert_eq!(seed_of("count_stays_decimal"), SumSeed::Decimal);
    // A literal target carries its kind itself - no claim position
    // needed, and the empty count stays in tonnes.
    assert_eq!(
        seed_of("literal_target_carries_its_unit"),
        SumSeed::Quantity("t".into())
    );
}

#[test]
fn sum_seeds_come_from_the_summed_variable_not_the_first_kinded_position() {
    use morpholog_core::{Prop, SumSeed, ValueExpr};
    // Every position of Mixed carries a different kind, and the
    // duration sits first: a seed resolver that matched by kind alone
    // (any variable at a duration/quantity position) would answer
    // Duration for all four sums. Each seed must come from the
    // position that binds the SUMMED variable.
    let src = "\
program seeds_by_variable
predicate Mixed(dur: Duration, weight: Decimal[t], amount: Decimal, cash: Decimal[USD])
invariant dur_seed:
    Mixed(_, _, _, _) implies sum(d | Mixed(d, w, a, c)) no_longer_than duration(PT48H)
invariant weight_seed:
    Mixed(_, _, _, _) implies sum(w | Mixed(d, w, a, c)) <= 100 t
invariant amount_seed:
    Mixed(_, _, _, _) implies sum(a | Mixed(d, w, a, c)) <= 100
invariant cash_seed:
    Mixed(_, _, _, _) implies sum(c | Mixed(d, w, a, c)) <= 100 USD
";
    let program = morpholog_surface::parse_program(src).unwrap();
    let seed_of = |name: &str| {
        let inv = program
            .invariants
            .iter()
            .find(|i| i.name.as_str() == name)
            .unwrap();
        let Prop::Implies { right, .. } = &inv.body else {
            panic!("implication expected");
        };
        let Prop::Compare { left, .. } = right.as_ref() else {
            panic!("comparison expected");
        };
        let ValueExpr::Sum { seed, .. } = left.as_ref() else {
            panic!("sum expected");
        };
        seed.clone()
    };
    assert_eq!(seed_of("dur_seed"), SumSeed::Duration);
    assert_eq!(seed_of("weight_seed"), SumSeed::Quantity("t".into()));
    assert_eq!(seed_of("amount_seed"), SumSeed::Decimal);
    assert_eq!(seed_of("cash_seed"), SumSeed::Quantity("USD".into()));
}

#[test]
fn the_first_position_binding_the_variable_decides_the_seed() {
    use morpholog_core::{Prop, SumSeed, ValueExpr};
    // The same variable at two positions of different kinds: the
    // documented rule is first-found-decides, so the decimal position
    // wins over the quantity position behind it. (Lowering semantics
    // only - the kernel's kind checks judge such a programme
    // separately.)
    let src = "\
program first_position_decides
predicate Twice(a: Decimal, b: Decimal[t])
invariant twice_capped:
    Twice(_, _) implies sum(x | Twice(x, x)) <= 100
";
    let program = morpholog_surface::parse_program(src).unwrap();
    let Prop::Implies { right, .. } = &program.invariants[0].body else {
        panic!("implication expected");
    };
    let Prop::Compare { left, .. } = right.as_ref() else {
        panic!("comparison expected");
    };
    let ValueExpr::Sum { seed, .. } = left.as_ref() else {
        panic!("sum expected");
    };
    assert_eq!(*seed, SumSeed::Decimal);
}
