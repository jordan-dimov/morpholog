//! Unary minus is surface sugar: `-x` is the subtraction `0 - x` in the
//! IR, so it adds no node, moves no hash and changes no decision. The
//! formatter prints the subtraction back as `-x` in source form only;
//! the canonical encoding and display text are unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::{canonical_hash, format_program, format_prop_inline};
use morpholog_core::{Program, ValidationError};
use morpholog_surface::{parse_expression, parse_program};

fn programme(body: &str) -> Program {
    let source = format!("program p\npredicate P(x: Decimal)\n\ninvariant i:\n  {body}\n");
    parse_program(&source).unwrap_or_else(|e| panic!("`{body}` should parse: {e:?}"))
}

/// A programme spelling every negation as `0 - ...`, as an author had to
/// before the sugar, with the hash it had then.
const SPELLED_OUT: &str = "program negation
predicate Score(entry: Subject, points: Decimal)

invariant floored:
  Score(_, points) implies max(0 - 100, points) <= 100 and 0 - points <= 100 and 0 - (points + 1) <= 100 and 0 - (0 - points) <= 100 and 0.0 - points <= 100
";
const SPELLED_OUT_HASH: &str =
    "sha256:9cd03871944fef4c6981fd29fea01fb85666bb331aa1b6cda20584dc066a6f5c";

/// The same programme with the sugar.
const SUGARED: &str = "program negation
predicate Score(entry: Subject, points: Decimal)

invariant floored:
  Score(_, points) implies max(-100, points) <= 100 and -points <= 100 and -(points + 1) <= 100 and -(-points) <= 100 and 0.0 - points <= 100
";

/// The hash a programme had before the sugar existed is the hash it has
/// now, and the sugared spelling is the same programme.
#[test]
fn the_sugar_moves_no_hash() {
    let spelled_out = parse_program(SPELLED_OUT).unwrap();
    assert_eq!(canonical_hash(&spelled_out), SPELLED_OUT_HASH);
    let sugared = parse_program(SUGARED).unwrap();
    assert_eq!(sugared, spelled_out);
    assert_eq!(canonical_hash(&sugared), SPELLED_OUT_HASH);
}

/// Source form prints a subtraction from the literal `0` as a negation,
/// and parses it back to the same programme. The literal `0.0` is not
/// `0`, so that subtraction keeps its spelling.
#[test]
fn source_form_prints_negation_and_round_trips() {
    let spelled_out = parse_program(SPELLED_OUT).unwrap();
    let rendered = format_program(&spelled_out);
    for fragment in [
        "max(-100, points)",
        "(-points) <= 100",
        "(-(points + 1)) <= 100",
        "(-(-points)) <= 100",
        "(0.0 - points) <= 100",
    ] {
        assert!(
            rendered.contains(fragment),
            "`{fragment}` missing from:\n{rendered}"
        );
    }
    assert_eq!(parse_program(&rendered).unwrap(), spelled_out);
}

/// In an operand position a negation is parenthesised like any other
/// arithmetic, and still reparses.
#[test]
fn negation_as_an_operand_round_trips() {
    for (body, rendered_as) in [
        ("-x * 2 <= 1", "((-x) * 2) <= 1"),
        ("1 - -x <= 1", "(1 - (-x)) <= 1"),
        ("-abs(x) <= 1", "(-abs(x)) <= 1"),
    ] {
        let p = programme(body);
        let rendered = format_program(&p);
        assert!(
            rendered.contains(rendered_as),
            "`{body}` should render as `{rendered_as}`:\n{rendered}"
        );
        assert_eq!(parse_program(&rendered).unwrap(), p, "`{body}` round trip");
    }
}

/// Display text, the rendering rejection reasons and witnesses use,
/// stays the positional subtraction.
#[test]
fn display_text_keeps_the_subtraction() {
    let prop = parse_expression("-x <= 1").unwrap();
    assert_eq!(format_prop_inline(&prop), "(0 - x) <= 1");
}

/// `-q` on a quantity is `0 - q`, which the unit algebra has no rule for;
/// the refusal names the kinds, the same one `0 - q` earns.
#[test]
fn negating_a_quantity_is_refused_by_the_arithmetic_rules() {
    let source =
        "program p\npredicate Q(x: Decimal[USD])\n\ninvariant i:\n  Q(x) implies -x <= 0 USD\n";
    let errs = parse_program(source)
        .unwrap()
        .validate()
        .expect_err("no rule");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::NoArithRule { .. })),
        "{errs:?}"
    );
    let text = format!("{}", errs[0]);
    assert!(text.contains("Decimal[USD]"), "{text}");
}
