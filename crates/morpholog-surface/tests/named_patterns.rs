//! Named-field claim patterns: `Pred(field: x, ..)` parses to the same IR as its positional twin,
//! and is the formatter's form for long runs of wildcards. Covers that equality, every refusal by
//! message, and that the named and positional sources share one canonical hash.

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
    // One name as both predicate and intent, with different fields: the verb picks which.
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
fn a_named_call_on_a_definition_resolves_against_its_parameters() {
    // The definition comes after its use, as a predicate may.
    let named = "\
program defs
predicate Reading(r: Subject, level: Decimal)
invariant bad:
    in_band(level: l) implies 0 <= l
define in_band(level):
    Reading(_, level) and 0 <= level
";
    let positional = named.replace("in_band(level: l)", "in_band(l)");
    assert_eq!(
        parse_program(named).unwrap(),
        parse_program(&positional).unwrap(),
        "a named call and its positional twin must produce identical IR"
    );
    refuses(
        &named.replace("in_band(level: l)", "in_band(band: l)"),
        "`in_band` declares no parameter `band`; declared: level",
    );
    // `..` over the one parameter is the positional `in_band(_)`: lawful, the body binds it.
    assert_eq!(
        parse_program(&named.replace("in_band(level: l) implies 0 <= l", "in_band(..)")).unwrap(),
        parse_program(&positional.replace("in_band(l) implies 0 <= l", "in_band(_)")).unwrap(),
    );
}

#[test]
fn a_named_pattern_on_an_undeclared_head_is_refused() {
    refuses(
        &format!("{DECLS}invariant bad:\n    Ghost(field: x, ..) implies 0 <= x\n"),
        "named fields need a declared predicate or definition; `Ghost` is neither",
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

// ---- named value lookups: `field: _` is the extraction hole ----

/// Pull the lone invariant's comparison LHS out as the `ValueOf` under
/// test.
fn lookup_in(src: &str) -> morpholog_core::ValueExpr {
    let program = parse_program(src).unwrap();
    let morpholog_core::Prop::Compare { left, .. } = &program.invariants[0].body else {
        panic!(
            "expected a comparison body, got {:?}",
            program.invariants[0].body
        );
    };
    (**left).clone()
}

#[test]
fn a_named_value_lookup_with_a_first_wildcard_hole_equals_its_positional_twin() {
    let named = format!(
        "{DECLS}invariant capped:\n    \
         value Line(id: i, invoice: inv, rate: r, volume: v, net: _) <= 100\n"
    );
    let positional = format!("{DECLS}invariant capped:\n    value Line(i, inv, r, v, _) <= 100\n");
    assert_eq!(
        parse_program(&named).unwrap(),
        parse_program(&positional).unwrap(),
        "when the hole is the first wildcard, the named form is sugar for the positional one"
    );
}

#[test]
fn a_named_value_lookup_extracts_a_non_first_field() {
    // `..` skips `id`, putting a wildcard before the read field, which no positional spelling
    // can express.
    let src =
        format!("{DECLS}invariant capped:\n    value Line(invoice: inv, rate: _, ..) <= 100\n");
    let morpholog_core::ValueExpr::ValueOf { args, extract, .. } = lookup_in(&src) else {
        panic!("expected a value lookup");
    };
    assert_eq!(
        args,
        vec![
            morpholog_core::Term::Wildcard,
            morpholog_core::Term::Var("inv".into()),
            morpholog_core::Term::Wildcard,
            morpholog_core::Term::Wildcard,
            morpholog_core::Term::Wildcard,
        ]
    );
    assert_eq!(extract, 2, "the hole is `rate`, not the first wildcard");
}

#[test]
fn a_named_value_lookup_marks_exactly_one_hole() {
    refuses(
        &format!("{DECLS}invariant bad:\n    value Line(invoice: inv, ..) <= 100\n"),
        "no field is marked",
    );
    refuses(
        &format!("{DECLS}invariant bad:\n    value Line(rate: _, net: _, ..) <= 100\n"),
        "a second extraction hole",
    );
}

#[test]
fn a_named_value_lookup_still_checks_its_fields() {
    refuses(
        &format!("{DECLS}invariant bad:\n    value Line(nope: x, net: _, ..) <= 100\n"),
        "declares no field `nope`",
    );
    refuses(
        &format!("{DECLS}invariant bad:\n    value Line(net: _) <= 100\n"),
        "names every field",
    );
}

#[test]
fn a_named_value_lookup_on_a_definition_is_refused() {
    refuses(
        &format!(
            "{DECLS}define billed(inv):\n    Line(invoice: inv, ..)\n\n\
             invariant bad:\n    value billed(inv: _, ..) <= 100\n"
        ),
        "is a definition, not a predicate",
    );
}

#[test]
fn a_non_first_hole_lookup_round_trips_through_the_formatter() {
    let src =
        format!("{DECLS}invariant capped:\n    value Line(invoice: inv, rate: _, ..) <= 100\n");
    let program = parse_program(&src).unwrap();
    let formatted = format_program(&program);
    assert!(
        formatted.contains("value Line(invoice: inv, rate: _, ..)"),
        "positional text would reparse to a different hole, so the named \
         spelling is forced; got:\n{formatted}"
    );
    let reparsed = parse_program(&formatted).unwrap();
    assert_eq!(program, reparsed, "the forced rendering must be faithful");
    assert_eq!(
        canonical_hash(&program),
        canonical_hash(&reparsed),
        "and rules identity must survive the round trip"
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
    // Hashes of the positional spellings of files that now use named patterns. Matching them
    // shows the hash renders positionally, so a named source shares its positional twin's hash.
    let cases = [(
        "../../examples/15_metered_billing/metered_billing.morph",
        "sha256:afd333f4db4ca8ee099836105314e99b54e4758dfe77fa6168a3b6f6ea7ec4eb",
    )];
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
    // The metered-billing rules before two of them named their key: the
    // hash pinned when named patterns arrived must still come out.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/15_metered_billing/metered_billing.morph");
    let mut before = fs::read_to_string(&path).unwrap();
    for (now, then) in [
        ("VatRate(rate_id, rate) implies", "VatRate(_, rate) implies"),
        (
            "ChargeLine(line: line, rate_p_per_kwh: rate_p_per_kwh, volume_kwh: volume_kwh, ..)",
            "ChargeLine(rate_p_per_kwh: rate_p_per_kwh, volume_kwh: volume_kwh, ..)",
        ),
    ] {
        assert!(before.contains(now), "the file no longer spells `{now}`");
        before = before.replace(now, then);
    }
    assert_eq!(
        canonical_hash(&parse_program(&before).unwrap()),
        "sha256:1bdb4fb757bc8d9656b9823edd024f7fc904191a750b695f7033f3feead268e5",
        "the pre-respelling metered billing no longer hashes as it did"
    );
}

/// The biometric example's named patterns hash as their positional
/// spelling, rebuilt here as for metered billing below.
#[test]
fn biometric_oversight_hashes_as_its_positional_twin() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/13_biometric_identification_oversight/biometric_oversight.morph");
    let named = fs::read_to_string(&path).unwrap();
    let mut positional = named.clone();
    for (from, to) in [
        (
            "UseStarted(use: use, system: system, started_at: started_at, ..)",
            "UseStarted(use, system, _, _, started_at)",
        ),
        (
            "UseStarted(use: use, system: system, ..)",
            "UseStarted(use, system, _, _, _)",
        ),
        (
            "UseStarted(use: u, started_at: started, ..)",
            "UseStarted(u, _, _, _, started)",
        ),
        (
            "UseStarted(use: use, started_at: started_at, ..)",
            "UseStarted(use, _, _, _, started_at)",
        ),
        ("UseStarted(use: use, ..)", "UseStarted(use, _, _, _, _)"),
        (
            "MatchRecorded(match: match, use: use, ..)",
            "MatchRecorded(match, use, _, _, _)",
        ),
        (
            "MatchRecorded(match: match, ..)",
            "MatchRecorded(match, _, _, _, _)",
        ),
        (
            "IdentificationDecision(match: match, ..)",
            "IdentificationDecision(_, match, _, _, _)",
        ),
    ] {
        assert!(
            positional.contains(from),
            "the file no longer spells `{from}`"
        );
        positional = positional.replace(from, to);
    }
    let code: String = positional
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect();
    assert!(
        !code.contains("..)"),
        "a named pattern was left unrewritten"
    );
    assert_eq!(
        canonical_hash(&parse_program(&named).unwrap()),
        canonical_hash(&parse_program(&positional).unwrap())
    );
}

/// The named spelling of metered billing hashes exactly as its positional
/// twin, rebuilt here rather than pinned, so the property holds for the
/// file as it stands, whatever its rules say.
#[test]
fn metered_billing_hashes_as_its_positional_twin() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/15_metered_billing/metered_billing.morph");
    let named = fs::read_to_string(&path).unwrap();
    let mut positional = named.clone();
    for (from, to) in [
        (
            "ChargeLine(net_gbp: net_gbp, vat_rate_id: vat_rate_id, vat_gbp: vat_gbp, ..)",
            "ChargeLine(_, _, _, _, net_gbp, vat_rate_id, vat_gbp)",
        ),
        (
            "ChargeLine(vat_rate_id: vat_rate_id, ..)",
            "ChargeLine(_, _, _, _, _, vat_rate_id, _)",
        ),
        (
            "ChargeLine(line: line, rate_p_per_kwh: rate_p_per_kwh, volume_kwh: volume_kwh, ..)",
            "ChargeLine(line, _, rate_p_per_kwh, volume_kwh, _, _, _)",
        ),
        (
            "ChargeLine(invoice: invoice, net_gbp: n, ..)",
            "ChargeLine(_, invoice, _, _, n, _, _)",
        ),
        (
            "ChargeLine(invoice: invoice, vat_gbp: v, ..)",
            "ChargeLine(_, invoice, _, _, _, _, v)",
        ),
        (
            "ChargeLine(invoice: invoice, ..)",
            "ChargeLine(_, invoice, _, _, _, _, _)",
        ),
        (
            "ChargeLine(line: line, net_gbp: net_gbp, vat_gbp: vat_gbp, ..)",
            "ChargeLine(line, _, _, _, net_gbp, _, vat_gbp)",
        ),
        (
            "ChargeLine(line: line, ..)",
            "ChargeLine(line, _, _, _, _, _, _)",
        ),
        (
            "CreditNote(reverses: line, ..)",
            "CreditNote(_, line, _, _)",
        ),
        (
            "ChargeLine(line: line, invoice: invoice, net_gbp: net_gbp, vat_gbp: vat_gbp, ..)",
            "ChargeLine(line, invoice, _, _, net_gbp, _, vat_gbp)",
        ),
    ] {
        assert!(
            positional.contains(from),
            "the file no longer spells `{from}`"
        );
        positional = positional.replace(from, to);
    }
    let code: String = positional
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect();
    assert!(
        !code.contains("..)"),
        "a named pattern was left unrewritten"
    );
    assert_eq!(
        canonical_hash(&parse_program(&named).unwrap()),
        canonical_hash(&parse_program(&positional).unwrap())
    );
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
    // Wildcard runs inside an `if(...)` condition and under a branch also get the named form.
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
    // In propositions and bind the fix is the positional form; admit and retract take no
    // definitions at all, so the refusal just says what it is.
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
    // One `..` mistake, one diagnostic: no second complaint about the wildcards it fills in.
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

// ============================================================
// Definition calls: parameters play the part of fields
// ============================================================

const DATED: &str = "\
program dated
predicate Terms(trade: Subject, version: Subject, qty: Decimal, delivery: Subject, effective_from: Date)
    unique by (version)
    effective by (trade) on (effective_from)
predicate Settled(trade: Subject, on: Date, qty: Decimal)
";

/// Parse, format, reparse, format again: the IR, the text and the hash must all settle at once.
fn settles_at_once(src: &str, needle: &str) {
    let first = parse_program(src).unwrap();
    let formatted = format_program(&first);
    assert!(
        formatted.contains(needle),
        "expected the formatter to spell `{needle}`; got:\n{formatted}"
    );
    let second = parse_program(&formatted).unwrap();
    assert_eq!(first, second, "formatting changed the IR");
    assert_eq!(
        formatted,
        format_program(&second),
        "a second formatting pass changed the text"
    );
    assert_eq!(canonical_hash(&first), canonical_hash(&second));
}

#[test]
fn a_named_call_on_a_generated_selector_resolves_against_its_signature() {
    // The selector does not exist in the source: keys, then `as_of`, then the payload fields.
    let named = format!(
        "{DATED}invariant within:\n    Settled(t, d, q) and \
         terms_in_force_on(trade: t, as_of: d, qty: cap, ..) implies q <= cap\n"
    );
    let positional = format!(
        "{DATED}invariant within:\n    Settled(t, d, q) and \
         terms_in_force_on(t, d, _, cap, _) implies q <= cap\n"
    );
    let a = parse_program(&named).unwrap();
    let b = parse_program(&positional).unwrap();
    assert_eq!(a, b, "the named call must lower to its positional twin");
    assert_eq!(canonical_hash(&a), canonical_hash(&b));
    assert!(a.validate().is_ok(), "{:?}", a.validate());
    refuses(
        &named.replace("qty: cap", "quantity: cap"),
        "`terms_in_force_on` declares no parameter `quantity`; declared: trade, as_of, version, qty, delivery",
    );
}

#[test]
fn the_generated_as_of_parameter_is_named_as_the_lowering_renames_it() {
    // A field called `as_of` pushes the generated parameter to `as_of_`; the call uses that name.
    let src = "\
program dated
predicate Rate(charge: Subject, as_of: Date, amount: Decimal)
    effective by (charge) on (as_of)
invariant positive:
    rate_in_force_on(charge: c, as_of_: @2026-06-01, amount: a) implies 0 <= a
";
    let named = parse_program(src).unwrap();
    let positional = parse_program(&src.replace(
        "rate_in_force_on(charge: c, as_of_: @2026-06-01, amount: a)",
        "rate_in_force_on(c, @2026-06-01, a)",
    ))
    .unwrap();
    assert_eq!(named, positional);
    assert!(named.validate().is_ok(), "{:?}", named.validate());
    refuses(
        &src.replace("as_of_: @2026-06-01", "as_of: @2026-06-01"),
        "`rate_in_force_on` declares no parameter `as_of`; declared: charge, as_of_, amount",
    );
}

#[test]
fn rest_cannot_supply_an_input_the_selector_needs() {
    // `..` wildcards `as_of`, which the body only reads, so the call is as unbound as the
    // positional `_` would be.
    let src = format!(
        "{DATED}invariant bad:\n    Settled(t, d, q) and \
         terms_in_force_on(trade: t, qty: cap, ..) implies q <= cap\n"
    );
    let program = parse_program(&src).unwrap();
    let errs = program
        .validate()
        .expect_err("an unbound as_of must not validate");
    assert!(
        errs.iter().any(|e| e.to_string().contains("as_of")),
        "expected the refusal to name `as_of`; got {errs:?}"
    );
}

#[test]
fn a_definition_sharing_a_predicates_name_is_refused_whatever_the_spelling() {
    let src = "\
program clash
predicate Reading(r: Subject, level: Decimal)
define Reading(level):
    0 <= level
invariant bad:
    Reading(level: l, ..) implies 0 <= l
";
    // The parser accepts the declarations; validation owns the shared namespace.
    let program = parse_program(src).expect("the collision reaches validation");
    let errs = program
        .validate()
        .expect_err("a predicate and a definition of one name never validate");
    assert!(
        errs.iter().any(|e| matches!(
            e,
            morpholog_core::ValidationError::DefinitionNameCollision { .. }
        )),
        "got {errs:?}"
    );
}

#[test]
fn the_formatter_names_a_definition_call_with_a_wildcard_wall() {
    let authored = "\
program defs
predicate Reading(r: Subject, site: Subject, level: Decimal, taken_on: Date)
define reading_at(site, level, taken_on, r):
    Reading(r, site, level, taken_on)
invariant positive:
    reading_at(s, l, _, _) implies 0 <= l
";
    settles_at_once(authored, "reading_at(site: s, level: l, ..)");
    let generated = format!(
        "{DATED}invariant delivered:\n    Settled(t, d, _) and \
         terms_in_force_on(t, d, _, _, dl) implies Settled(t, d, _)\n"
    );
    settles_at_once(
        &generated,
        "terms_in_force_on(trade: t, as_of: d, delivery: dl, ..)",
    );
    // One wildcard stays positional, as for a claim pattern.
    let single = format!(
        "{DATED}invariant within:\n    Settled(t, d, q) and \
         terms_in_force_on(t, d, _, cap, dl) implies q <= cap\n"
    );
    settles_at_once(&single, "terms_in_force_on(t, d, _, cap, dl)");
}

#[test]
fn trade_lifecycle_hashes_as_its_positional_twin() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/10_trade_lifecycle/trade_lifecycle.morph");
    let named = fs::read_to_string(&path).unwrap();
    let now = "trade_terms_in_force_on(trade: trade, as_of: d, quantity: qty, ..)";
    assert!(named.contains(now), "the file no longer spells `{now}`");
    let positional = named.replace(now, "trade_terms_in_force_on(trade, d, _, qty, _)");
    let a = parse_program(&named).unwrap();
    let b = parse_program(&positional).unwrap();
    assert_eq!(a, b);
    // The hash the file had before the call was respelled.
    assert_eq!(
        canonical_hash(&a),
        "sha256:8f40dc6b6e53fc332379ad889d24154e9c1d5431b9db4559df2e51789b427daa"
    );
}
