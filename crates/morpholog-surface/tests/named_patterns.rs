//! Named-field claim patterns: `Pred(field: x, ..)` is parse-time sugar
//! lowered to the positional pattern - no IR change - and the
//! formatter's canonical form for wildcard walls. These tests hold the
//! acceptance side (IR equality with the positional twin), every
//! refusal by message, and the rules-identity property: a named source
//! and its positional twin share one canonical hash.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use morpholog_core::format::{canonical_hash, format_program};
use morpholog_surface::parse_program;

/// Parse must fail, and some diagnostic must carry `needle`.
fn refuses(src: &str, needle: &str) {
    let errs = parse_program(src).expect_err("expected a parse refusal");
    assert!(
        errs.iter().any(|d| d.message.contains(needle)),
        "expected a diagnostic containing {needle:?}; got {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

const DECLS: &str = "\
program named
predicate Line(id: Subject, invoice: Subject, rate: Decimal, volume: Decimal, net: Decimal)
intent Notice(recipient: Subject, body: Subject)
";

#[test]
fn a_named_pattern_lowers_to_its_positional_twin() {
    let named = format!(
        "{DECLS}invariant nets_positive:\n    Line(invoice: inv, net: n, ..) implies 0 <= n\n"
    );
    let positional =
        format!("{DECLS}invariant nets_positive:\n    Line(_, inv, _, _, n) implies 0 <= n\n");
    assert_eq!(
        parse_program(&named).unwrap(),
        parse_program(&positional).unwrap(),
        "named and positional spellings must produce identical IR"
    );
}

#[test]
fn named_entries_resolve_in_any_order() {
    let reordered = format!(
        "{DECLS}invariant nets_positive:\n    Line(net: n, invoice: inv, ..) implies 0 <= n\n"
    );
    let declared_order = format!(
        "{DECLS}invariant nets_positive:\n    Line(invoice: inv, net: n, ..) implies 0 <= n\n"
    );
    assert_eq!(
        parse_program(&reordered).unwrap(),
        parse_program(&declared_order).unwrap(),
        "entry order must not matter - that is half the point"
    );
}

#[test]
fn a_bare_rest_pattern_is_the_existence_check() {
    let named = format!("{DECLS}invariant some_line:\n    Line(..) implies Line(..)\n");
    let positional = format!(
        "{DECLS}invariant some_line:\n    Line(_, _, _, _, _) implies Line(_, _, _, _, _)\n"
    );
    assert_eq!(
        parse_program(&named).unwrap(),
        parse_program(&positional).unwrap()
    );
}

#[test]
fn named_total_admit_and_emit_lower_to_full_argument_lists() {
    let named = format!(
        "{DECLS}transformation add(id, inv, r, v, n, who, why):\n    \
         admit Line(id: id, invoice: inv, rate: r, volume: v, net: n)\n    \
         emit Notice(recipient: who, body: why)\n"
    );
    let positional = format!(
        "{DECLS}transformation add(id, inv, r, v, n, who, why):\n    \
         admit Line(id, inv, r, v, n)\n    \
         emit Notice(who, why)\n"
    );
    assert_eq!(
        parse_program(&named).unwrap(),
        parse_program(&positional).unwrap()
    );
}

#[test]
fn named_retract_widens_like_its_positional_twin() {
    let named = format!("{DECLS}transformation drop(inv):\n    retract Line(invoice: inv, ..)\n");
    let positional =
        format!("{DECLS}transformation drop(inv):\n    retract Line(_, inv, _, _, _)\n");
    assert_eq!(
        parse_program(&named).unwrap(),
        parse_program(&positional).unwrap()
    );
}

#[test]
fn a_forall_source_takes_the_named_form() {
    let named =
        format!("{DECLS}invariant all_positive:\n    forall n in Line(net: n, ..): 0 <= n\n");
    let positional =
        format!("{DECLS}invariant all_positive:\n    forall n in Line(_, _, _, _, n): 0 <= n\n");
    assert_eq!(
        parse_program(&named).unwrap(),
        parse_program(&positional).unwrap()
    );
}

#[test]
fn a_multi_line_named_pattern_needs_no_layout_care() {
    // Parens disable layout, so the entries may sit anywhere.
    let named = format!(
        "{DECLS}invariant nets_positive:\n    (Line(\n            invoice: inv,\n        net: n,\n            ..\n    ) implies 0 <= n)\n"
    );
    let positional =
        format!("{DECLS}invariant nets_positive:\n    (Line(_, inv, _, _, n) implies 0 <= n)\n");
    assert_eq!(
        parse_program(&named).unwrap(),
        parse_program(&positional).unwrap()
    );
}

#[test]
fn the_same_name_resolves_per_vocabulary() {
    // One name, two vocabularies, different field lists: the statement
    // verb picks the table, so both resolve correctly side by side.
    let src = "\
program two_vocabularies
predicate Notice(account: Subject, code: Subject)
intent Notice(recipient: Subject, body: Subject)
transformation act(a, c, r, b):
    require Notice(account: a, code: c)
    admit Notice(account: a, code: c)
    emit Notice(recipient: r, body: b)
";
    let positional = "\
program two_vocabularies
predicate Notice(account: Subject, code: Subject)
intent Notice(recipient: Subject, body: Subject)
transformation act(a, c, r, b):
    require Notice(a, c)
    admit Notice(a, c)
    emit Notice(r, b)
";
    assert_eq!(
        parse_program(src).unwrap(),
        parse_program(positional).unwrap()
    );
}

// ============================================================
// Refusals
// ============================================================

#[test]
fn mixing_named_and_positional_is_refused() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Line(inv, net: n, ..) implies 0 <= n\n"),
        "all-named or all-positional",
    );
}

#[test]
fn a_duplicate_field_entry_is_refused() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Line(net: a, net: b, ..) implies a = b\n"),
        "named twice",
    );
}

#[test]
fn an_unknown_field_is_refused_naming_the_declared_ones() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Line(nett: n, ..) implies 0 <= n\n"),
        "declares no field `nett`; declared: id, invoice, rate, volume, net",
    );
}

#[test]
fn a_named_pattern_without_rest_must_be_total() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Line(invoice: inv, net: n) implies 0 <= n\n"),
        "missing: id, rate, volume",
    );
}

#[test]
fn rest_must_come_last() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Line(.., net: n) implies 0 <= n\n"),
        "put it last",
    );
}

#[test]
fn rest_alone_with_positional_terms_is_refused() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Line(inv, ..) implies Line(..)\n"),
        "`..` belongs to a named pattern",
    );
}

#[test]
fn rest_is_refused_on_admit_and_emit() {
    refuses(
        &format!("{DECLS}transformation bad(id):\n    admit Line(id: id, ..)\n"),
        "`..` is not allowed in `admit`",
    );
    refuses(
        &format!("{DECLS}transformation bad(r):\n    emit Notice(recipient: r, ..)\n"),
        "`..` is not allowed in `emit`",
    );
}

#[test]
fn a_named_pattern_on_a_definition_is_refused_by_kind() {
    // The definition may follow its attempted use - the refusal must
    // still name what it is.
    let src = "\
program defs
predicate Reading(r: Subject, level: Decimal)
invariant bad:
    in_band(level: l) implies 0 <= l
define in_band(level):
    Reading(_, level) and 0 <= level
";
    refuses(src, "is a definition; definitions have parameters");
}

#[test]
fn a_named_pattern_on_an_undeclared_head_is_refused() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Ghost(field: x, ..) implies 0 <= x\n"),
        "named fields need a declared predicate; `Ghost` is not one",
    );
}

#[test]
fn a_named_pattern_under_a_duplicate_declaration_refuses_to_guess() {
    let src = "\
program dup
predicate P(a: Subject)
predicate P(b: Subject)
invariant bad:
    P(a: x) implies P(a: x)
";
    refuses(src, "declared more than once");
}

#[test]
fn value_lookups_stay_positional() {
    refuses(
        &format!(
            "{DECLS}invariant bad:\n    Line(invoice: inv, ..) implies 0 <= value Line(net: _, ..) default 0\n"
        ),
        "`value` takes the positional form only",
    );
}

#[test]
fn parse_expression_has_no_declarations_to_resolve_against() {
    let errs = morpholog_surface::parse_expression("Line(net: n, ..) implies 0 <= n")
        .expect_err("a named pattern needs declarations this entry point does not have");
    assert!(
        errs.iter()
            .any(|d| d.message.contains("named fields need a declared predicate")),
        "got {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// ============================================================
// Rules identity: the named canonical form must not move any hash
// ============================================================

#[test]
fn the_gallery_hashes_did_not_move_when_named_patterns_arrived() {
    // Recorded from the positional sources BEFORE the named sugar and
    // the named canonical form existed; the same files now carry named
    // patterns, so these literals prove both halves at once: the hash
    // renders positionally, and a named source shares its positional
    // twin's identity.
    let cases = [
        (
            "../../examples/15_metered_billing/metered_billing.morph",
            "sha256:5777060fd9d8f488c726c55c4fe1678a63bdecaaced234361bb834c1aee96ef3",
        ),
        (
            "../../examples/13_biometric_identification_oversight/biometric_oversight.morph",
            "sha256:9ed56ed8180cd6b3d916025dd7d43b8c5d34d7801fd02d4ff9b9a36dea8b4c32",
        ),
    ];
    for (rel, expected) in cases {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        let src = fs::read_to_string(&path).unwrap();
        let p = parse_program(&src).unwrap();
        assert_eq!(
            canonical_hash(&p),
            expected,
            "rules identity moved for {rel} - the hash must render positionally"
        );
    }
}

#[test]
fn the_formatter_prints_the_named_canonical_form_for_walls() {
    let src =
        format!("{DECLS}invariant nets_positive:\n    Line(_, inv, _, _, n) implies 0 <= n\n");
    let p = parse_program(&src).unwrap();
    let formatted = format_program(&p);
    assert!(
        formatted.contains("Line(invoice: inv, net: n, ..)"),
        "a two-wildcard run renders named; got:\n{formatted}"
    );
    // And the canonical form reparses to the same IR - the round trip.
    assert_eq!(parse_program(&formatted).unwrap(), p);
}

#[test]
fn a_single_wildcard_stays_positional_in_the_canonical_form() {
    let src = "\
program single
predicate Pair(a: Subject, b: Subject)
invariant some:
    Pair(x, _) implies Pair(x, _)
";
    let p = parse_program(src).unwrap();
    let formatted = format_program(&p);
    assert!(
        formatted.contains("Pair(x, _)"),
        "one wildcard is legible where it stands; got:\n{formatted}"
    );
}
#[test]
fn a_declaration_repeating_a_field_name_is_refused_at_parse() {
    refuses(
        "program dup\npredicate P(x: Decimal, x: Decimal)\ninvariant ok: P(a, b) implies a <= b\n",
        "declares argument `x` more than once",
    );
    refuses(
        "program dup\npredicate P(a: Subject)\nintent N(x: Subject, x: Subject)\ntransformation t(a, y):\n    admit P(a)\n    emit N(y, y)\n",
        "declares argument `x` more than once",
    );
}

#[test]
fn the_naming_context_reaches_inside_if() {
    // The one formatter arm that escaped the recursive context in the
    // first landing: a wall in an if(...) condition, and another under
    // a branch, must both take the named canonical form.
    let src = "\
program cond
predicate Line(id: Subject, invoice: Subject, rate: Decimal, volume: Decimal, net: Decimal)
predicate Out(invoice: Subject, x: Decimal)
transformation act(inv):
    let x = if(Line(_, inv, _, _, _), sum(n | Line(_, inv, _, _, n)), 0)
    admit Out(inv, x)
";
    let p = parse_program(src).unwrap();
    let formatted = format_program(&p);
    assert!(
        formatted.contains("if(Line(invoice: inv, ..)"),
        "the condition's wall renders named; got:\n{formatted}"
    );
    assert!(
        formatted.contains("sum(n | Line(invoice: inv, net: n, ..))"),
        "the branch's wall renders named; got:\n{formatted}"
    );
    assert_eq!(parse_program(&formatted).unwrap(), p);
}

#[test]
fn admit_and_retract_name_the_definition_without_a_false_repair() {
    // "use the positional form" is a true repair in claim positions and
    // bind; on retract/admit a definition is not lawful at all, so the
    // refusal says what it is instead.
    let src = "\
program defs
predicate Reading(r: Subject, level: Decimal)
define in_band(level):
    Reading(_, level) and 0 <= level
transformation bad(l):
    retract in_band(level: l)
";
    refuses(src, "is a definition, not a predicate");
}

#[test]
fn a_refused_rest_on_admit_yields_one_diagnostic_not_two() {
    // The `..` refusal must not cascade into a second complaint about
    // the wildcards resolution synthesised to keep the parse alive.
    let errs = parse_program(&format!(
        "{DECLS}transformation bad(id):\n    admit Line(id: id, ..)\n"
    ))
    .expect_err("`..` on admit refuses");
    assert!(
        errs.iter()
            .any(|d| d.message.contains("`..` is not allowed in `admit`")),
        "got {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert!(
        !errs
            .iter()
            .any(|d| d.message.contains("wildcard `_` is not allowed")),
        "one authored mistake, one diagnostic: {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
