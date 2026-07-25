//! The lint tier's occupants. The gate-vs-invariant lint catches the
//! revocation-rewrites-history shape (append-only antecedent, pointer
//! consequent, forward direction only - the reverse is correct
//! doctrine). The unsupplied-antecedent lint catches an antecedent
//! referencing a predicate the programme declares no transformation to
//! admit. Every worked example stays clean of both, pinned by the
//! cross-example test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Lint, lints};
use morpholog_surface::parse_program;

fn lints_of(source: &str) -> Vec<Lint> {
    let program = parse_program(source).expect("programme should parse");
    program.validate().expect("programme should validate");
    lints(&compiled(&program))
}

/// The predicate list of the single unsupplied-antecedent finding, or
/// empty if there is none.
fn unsupplied_missing(found: &[Lint]) -> Vec<&str> {
    found
        .iter()
        .find_map(|l| match l {
            Lint::UnsuppliedAntecedent { missing, .. } => {
                Some(missing.iter().map(String::as_str).collect())
            }
            Lint::GateVsInvariant { .. } | Lint::GoverningSelectionWithoutTotality { .. } => None,
        })
        .unwrap_or_default()
}

/// The (invariant, predicates) of the single governing-selection
/// finding, or None.
fn governing_finding(found: &[Lint]) -> Option<(&str, Vec<&str>)> {
    found.iter().find_map(|l| match l {
        Lint::GoverningSelectionWithoutTotality {
            invariant,
            predicates,
        } => Some((
            invariant.as_str(),
            predicates.iter().map(String::as_str).collect(),
        )),
        Lint::GateVsInvariant { .. } | Lint::UnsuppliedAntecedent { .. } => None,
    })
}

const TRIP: &str = r#"
program trip

predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

transformation record(d, doc):
    admit Decision(d, doc)

invariant decisions_need_live_mandate:
    Decision(d, doc) implies CurrentMandate(doc, _)
"#;

/// Build a CompiledProgram for the analysis entry points, which now
/// take `&CompiledProgram`.
fn compiled(p: &morpholog_core::Program) -> morpholog_core::CompiledProgram {
    morpholog_core::CompiledProgram::new(p.clone()).expect("fixture is valid")
}

#[test]
fn an_append_only_antecedent_requiring_a_pointer_fires_with_both_names() {
    let found = lints_of(TRIP);
    assert_eq!(found.len(), 1, "exactly one finding: {found:?}");
    let Lint::GateVsInvariant {
        invariant,
        append_only,
        pointer,
    } = &found[0]
    else {
        panic!("expected GateVsInvariant: {found:?}");
    };
    assert_eq!(invariant, "decisions_need_live_mandate");
    assert_eq!(append_only, "Decision");
    assert_eq!(pointer, "CurrentMandate");
}

// The reverse direction - "the pointer names a record that exists" -
// is correct doctrine: retracting the pointer makes the rule vacuous,
// never violated. It must stay silent.
#[test]
fn the_reverse_direction_is_correct_doctrine_and_stays_silent() {
    let found = lints_of(
        r#"
program reverse

predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, decision_id: Subject)
    current pointer by (doc)

transformation point(doc, d):
    admit CurrentMandate(doc, d)

invariant mandate_points_at_a_decision:
    CurrentMandate(doc, d) implies Decision(d, doc)
"#,
    );
    assert!(found.is_empty(), "got {found:?}");
}

// A NEGATIVE pointer reference gets stronger when the pointer is
// retracted - the opposite of the bug - so polarity must silence it.
#[test]
fn a_negated_pointer_consequent_stays_silent() {
    let found = lints_of(
        r#"
program negated

predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

transformation record(d, doc):
    admit Decision(d, doc)

invariant closed_decisions_have_no_mandate:
    Decision(d, doc) implies not CurrentMandate(doc, _)
"#,
    );
    assert!(found.is_empty(), "got {found:?}");
}

// A pointer requirement hidden behind a named condition still fires:
// the walker descends through the definition's body.
#[test]
fn a_pointer_behind_a_defined_call_still_fires() {
    let found = lints_of(
        r#"
program through_define

predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

transformation record(d, doc):
    admit Decision(d, doc)

define mandated(doc):
    CurrentMandate(doc, _)

invariant decisions_need_live_mandate:
    Decision(d, doc) implies mandated(doc)
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
}

// Every worked example is lint-clean: the disciplines sweep declared
// the doctrine without introducing the shape the lint names. (08's
// onboarding rule has the pointer-consequent shape on purpose, and
// stays clean precisely because OnboardedCustomer is deliberately not
// append-only - continuous compliance is its intent.)
#[test]
fn every_worked_example_is_lint_clean() {
    for (name, program) in tests_common_all_programs() {
        let found = lints(&compiled(&program));
        assert!(found.is_empty(), "{name} should be lint-clean: {found:?}");
    }
}

/// Parse every example .morph the same way the cross-example property
/// tests do, without depending on their private helper.
fn tests_common_all_programs() -> Vec<(String, morpholog_core::Program)> {
    let examples_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(examples_dir).expect("examples dir") {
        let dir = entry.expect("dir entry").path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir).expect("example dir") {
            let path = file.expect("file entry").path();
            if path.extension().is_some_and(|e| e == "morph") {
                let source = std::fs::read_to_string(&path).expect("readable .morph");
                let program = parse_program(&source)
                    .unwrap_or_else(|e| panic!("{} should parse: {e:?}", path.display()));
                out.push((path.display().to_string(), program));
            }
        }
    }
    assert!(!out.is_empty(), "found no example programmes");
    out
}

// The implication itself can hide behind a named condition too: the
// implication collector descends through `define` bodies (the same
// red line the consequent walker already honoured), so the trip
// shape is linted wherever it is spelled.
#[test]
fn an_implication_inside_a_defined_call_still_fires() {
    let found = lints_of(
        r#"
program implication_through_define

predicate Registered(doc: Subject)
predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

transformation record(d, doc):
    admit Decision(d, doc)

define decisions_mandated(doc):
    Registered(doc) and (Decision(_, doc) implies CurrentMandate(doc, _))

invariant registered_docs_decisions_mandated:
    decisions_mandated(doc)
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    let Lint::GateVsInvariant {
        append_only,
        pointer,
        ..
    } = &found[0]
    else {
        panic!("expected GateVsInvariant: {found:?}");
    };
    assert_eq!(append_only, "Decision");
    assert_eq!(pointer, "CurrentMandate");
}

// A negated implication asserts no implication at all -
// `not (A implies B)` is `A and not B` - so the lint must not read
// one out of it.
#[test]
fn a_negated_implication_is_not_linted() {
    let found = lints_of(
        r#"
program negated_implication

predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

invariant never_all_three:
    not (Decision(d, doc) implies CurrentMandate(doc, _))
"#,
    );
    assert!(found.is_empty(), "got {found:?}");
}

// An antecedent referencing a predicate the programme declares no
// supplier for is flagged, and the hint names that predicate.
#[test]
fn an_antecedent_with_no_declared_supplier_is_flagged() {
    let found = lints_of(
        r#"
program unsupplied

predicate Trade(trade: Subject)
predicate Settled(trade: Subject)

transformation capture(t):
    admit Trade(t)

invariant settled_trades_are_captured:
    Settled(t) implies Trade(t)
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    let Lint::UnsuppliedAntecedent { invariant, .. } = &found[0] else {
        panic!("expected UnsuppliedAntecedent: {found:?}");
    };
    assert_eq!(invariant, "settled_trades_are_captured");
    assert_eq!(unsupplied_missing(&found), ["Settled"]);
}

// Once a transformation can admit it, the same antecedent is live again.
#[test]
fn a_supplied_antecedent_stays_silent() {
    let found = lints_of(
        r#"
program supplied

predicate Trade(trade: Subject)
predicate Settled(trade: Subject)

transformation capture(t):
    admit Trade(t)

transformation settle(t):
    admit Settled(t)

invariant settled_trades_are_captured:
    Settled(t) implies Trade(t)
"#,
    );
    assert!(found.is_empty(), "got {found:?}");
}

// One unsupplied branch of an `or` does not block the antecedent if
// another branch has a supplier.
#[test]
fn an_unsupplied_or_branch_with_a_supplied_alternative_stays_silent() {
    let found = lints_of(
        r#"
program live_or

predicate Trade(trade: Subject)
predicate Settled(trade: Subject)

transformation capture(t):
    admit Trade(t)

invariant either_way_captured:
    (Settled(t) or Trade(t)) implies Trade(t)
"#,
    );
    assert!(found.is_empty(), "got {found:?}");
}

// An `or` whose every branch is unsupplied is blocked, and the hint
// names every branch (collectively the cause, not each on its own).
#[test]
fn an_or_of_only_unsupplied_branches_names_them_all() {
    let found = lints_of(
        r#"
program dead_or

predicate Trade(trade: Subject)
predicate Settled(trade: Subject)
predicate Approved(trade: Subject)

transformation capture(t):
    admit Trade(t)

invariant settled_or_approved_are_captured:
    (Settled(t) or Approved(t)) implies Trade(t)
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(unsupplied_missing(&found), ["Approved", "Settled"]);
}

// A mandatory unsupplied conjunct blocks the antecedent; an unsupplied
// predicate in a parallel `or` that could bind otherwise is NOT named.
#[test]
fn only_the_mandatory_dead_conjunct_is_named() {
    let found = lints_of(
        r#"
program conjunct_and_optional

predicate Trade(trade: Subject)
predicate Settled(trade: Subject)
predicate Approved(trade: Subject)

transformation capture(t):
    admit Trade(t)

invariant captured_when_settled_and_maybe_approved:
    (Settled(t) and (Approved(t) or Trade(t))) implies Trade(t)
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(unsupplied_missing(&found), ["Settled"]);
}

// The unsupplied requirement can hide behind a `define`; the detector
// descends into the body, as the gate-vs-invariant walker does.
#[test]
fn an_unsupplied_antecedent_behind_a_define_fires() {
    let found = lints_of(
        r#"
program dead_define

predicate Trade(trade: Subject)
predicate Settled(trade: Subject)

transformation capture(t):
    admit Trade(t)

define settled(t):
    Settled(t)

invariant settled_trades_are_captured:
    settled(t) implies Trade(t)
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(unsupplied_missing(&found), ["Settled"]);
}

// An unsupplied requirement under `exists` blocks the same way.
#[test]
fn an_unsupplied_antecedent_under_exists_fires() {
    let found = lints_of(
        r#"
program dead_exists

predicate Trade(trade: Subject)
predicate Settled(settlement: Subject, trade: Subject)

transformation capture(t):
    admit Trade(t)

invariant settled_trades_are_captured:
    (exists s: Settled(s, t)) implies Trade(t)
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(unsupplied_missing(&found), ["Settled"]);
}

// One dead and one live implication in the same invariant: the dead one
// is flagged, but the invariant is not declared to enforce nothing - the
// live implication still does its work.
#[test]
fn a_dead_implication_beside_a_live_one_flags_only_the_dead() {
    let found = lints_of(
        r#"
program mixed_implications

predicate Trade(trade: Subject)
predicate Confirmed(trade: Subject)
predicate Settled(trade: Subject)

transformation capture(t):
    admit Trade(t)

transformation confirm(t):
    admit Confirmed(t)

invariant captured_and_settled_are_traded:
    (Confirmed(t) implies Trade(t)) and (Settled(t) implies Trade(t))
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(unsupplied_missing(&found), ["Settled"]);
}

// A prohibition is not implication-shaped, so it has no antecedent to
// block - the lint leaves it alone even with an unsupplied predicate.
#[test]
fn a_prohibition_invariant_is_not_flagged() {
    let found = lints_of(
        r#"
program prohibition

predicate Settled(settlement: Subject, trade: Subject)

invariant no_settlement_yet:
    not (exists s: Settled(s, t))
"#,
    );
    assert!(found.is_empty(), "got {found:?}");
}

// Both unsupplied conjuncts are named once, deduped across the
// invariant's implications.
#[test]
fn each_missing_predicate_is_named_once() {
    let found = lints_of(
        r#"
program multi_missing

predicate Trade(trade: Subject)
predicate Settled(trade: Subject)
predicate Approved(trade: Subject)

transformation capture(t):
    admit Trade(t)

invariant settled_and_approved_are_captured:
    (Settled(t) implies Trade(t)) and (Approved(t) implies Trade(t))
"#,
    );
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(unsupplied_missing(&found), ["Approved", "Settled"]);
}

// Findings come back in invariant-declaration order: the unsupplied
// finding for the first-declared invariant precedes the gate finding for
// the second.
#[test]
fn findings_are_returned_in_invariant_declaration_order() {
    let found = lints_of(
        r#"
program order

predicate Settled(trade: Subject)
predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

transformation record(d, doc):
    admit Decision(d, doc)

invariant aaa_unsupplied:
    Settled(t) implies Decision(t, t)

invariant zzz_gate:
    Decision(d, doc) implies CurrentMandate(doc, _)
"#,
    );
    assert_eq!(found.len(), 2, "got {found:?}");
    assert!(
        matches!(&found[0], Lint::UnsuppliedAntecedent { invariant, .. } if invariant == "aaa_unsupplied"),
        "got {found:?}"
    );
    assert!(
        matches!(&found[1], Lint::GateVsInvariant { invariant, .. } if invariant == "zzz_gate"),
        "got {found:?}"
    );
}

// ============================================================
// The governing-selection-without-totality lint: the effective-time
// vacuity smell. A dated claim bounded on-or-before a coordinate plus
// a negated exists excluding a strictly later version selects "the
// version in force" - and passes vacuously when no version exists,
// unless another invariant carries the totality-backstop shape.
// ============================================================

const TARIFF_SELECTION: &str = r#"
program tariffs

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant reading_priced_by_governing_tariff:
    (Reading(m, d, _) and Tariff(m, rate, ef) and ef on_or_before d and not (exists later: Tariff(m, _, later) and later on_or_before d and later after ef)) implies rate <= 1000
"#;

const TOTALITY_COMPANION: &str = r#"
invariant every_reading_day_has_a_tariff:
    Reading(m, d, _) implies (exists e: Tariff(m, _, e) and e on_or_before d)
"#;

#[test]
fn a_governing_selection_without_a_companion_fires_naming_the_predicate() {
    let found = lints_of(TARIFF_SELECTION);
    assert_eq!(found.len(), 1, "exactly one finding: {found:?}");
    let (invariant, predicates) = governing_finding(&found).expect("the governing finding");
    assert_eq!(invariant, "reading_priced_by_governing_tariff");
    assert_eq!(predicates, ["Tariff"]);
}

#[test]
fn the_totality_companion_suppresses_the_finding() {
    let source = format!("{TARIFF_SELECTION}{TOTALITY_COMPANION}");
    assert_eq!(lints_of(&source), vec![], "the backstop closes the smell");
}

// The real-world spelling: the whole selection lives inside a `define`
// (example 10's terms_in_force_on shape), so detection must expand
// defined calls.
#[test]
fn a_selection_spelled_inside_a_define_still_fires() {
    let found = lints_of(
        r#"
program tariffs_defined

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

define tariff_in_force_on(m, d, rate):
    Tariff(m, rate, ef) and ef on_or_before d and not (exists later: Tariff(m, _, later) and later on_or_before d and later after ef)

invariant reading_priced_by_governing_tariff:
    (Reading(m, d, _) and tariff_in_force_on(m, d, rate)) implies rate <= 1000
"#,
    );
    let (invariant, predicates) = governing_finding(&found).expect("fires through the define");
    assert_eq!(invariant, "reading_priced_by_governing_tariff");
    assert_eq!(predicates, ["Tariff"]);
}

#[test]
fn the_timestamp_domain_fires_like_the_date_domain() {
    let found = lints_of(
        r#"
program marks

predicate MarkStruck(book: Subject, at: Timestamp, level: Decimal)
predicate Curve(book: Subject, price: Decimal, published_at: Timestamp)

transformation strike(b, at, level):
    admit MarkStruck(b, at, level)

transformation publish(b, p, at):
    admit Curve(b, p, at)

invariant mark_uses_governing_curve:
    (MarkStruck(b, at, _) and Curve(b, price, pub) and pub at_or_before at and not (exists later: Curve(b, _, later) and later at_or_before at and later strictly_after pub)) implies price <= 100000
"#,
    );
    let (invariant, predicates) = governing_finding(&found).expect("timestamps fire too");
    assert_eq!(invariant, "mark_uses_governing_curve");
    assert_eq!(predicates, ["Curve"]);
}

// Near-misses that must stay clean: the lint accuses the pattern, not
// ordinary temporal logic.
#[test]
fn a_uniqueness_not_exists_with_no_date_compare_stays_clean() {
    assert_eq!(
        lints_of(
            r#"
program unique_shape

predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant one_rate_per_meter:
    (Tariff(m, rate, ef) and not (exists other: Tariff(m, other, ef) and other != rate)) implies rate <= 1000
"#,
        ),
        vec![]
    );
}

#[test]
fn a_date_window_without_the_not_exists_tiebreak_stays_clean() {
    assert_eq!(
        lints_of(
            r#"
program window

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant priced_by_any_effective_tariff:
    (Reading(m, d, _) and Tariff(m, rate, ef) and ef on_or_before d) implies rate <= 1000
"#,
        ),
        vec![]
    );
}

#[test]
fn a_selection_in_the_consequent_stays_clean() {
    assert_eq!(
        lints_of(
            r#"
program consequent_side

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant every_reading_has_a_governing_tariff:
    Reading(m, d, _) implies (exists ef: Tariff(m, _, ef) and ef on_or_before d and not (exists later: Tariff(m, _, later) and later on_or_before d and later after ef))
"#,
        ),
        vec![]
    );
}

// Evidence never crosses `or` branches: a candidate claim in one
// branch and the excluder in another is a pattern no branch contains.
#[test]
fn evidence_does_not_combine_across_or_branches() {
    assert_eq!(
        lints_of(
            r#"
program split_branches

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant split:
    ((Reading(m, d, _) and Tariff(m, rate, ef) and ef on_or_before d and rate <= 1000) or (Reading(m2, d2, _) and not (exists later: Tariff(m2, _, later) and later on_or_before d2 and later before d2))) implies 1 <= 1
"#,
        ),
        vec![]
    );
}

// A temporal comparison inside the negated exists that does not relate
// the excluded version to the candidate is not a tiebreak.
#[test]
fn an_unrelated_temporal_compare_in_the_not_exists_stays_clean() {
    assert_eq!(
        lints_of(
            r#"
program unrelated_compare

predicate Invoice(invoice_id: Subject, issued: Date, due: Date)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)
predicate Reading(meter: Subject, day: Date, kwh: Decimal)

transformation record(inv, issued, due, m, d, k):
    admit Invoice(inv, issued, due)
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant odd_but_not_a_selection:
    (Reading(m, d, _) and Invoice(inv, issued, due) and Tariff(m, rate, ef) and ef on_or_before d and not (exists x: Tariff(m, _, x) and issued before due)) implies rate <= 1000
"#,
        ),
        vec![]
    );
}

// Companion shapes that must NOT suppress: a guarantee that is
// optional, conditional, or undated does not close the hole.
#[test]
fn a_disjunctive_companion_does_not_suppress() {
    let source = format!(
        "{TARIFF_SELECTION}
predicate Exceptional(meter: Subject)

transformation flag(m):
    admit Exceptional(m)

invariant weak_backstop:
    Reading(m, d, _) implies (Exceptional(m) or (exists e: Tariff(m, _, e) and e on_or_before d))
"
    );
    let found = lints_of(&source);
    assert!(
        governing_finding(&found).is_some(),
        "an or-branch witness is not a guarantee: {found:?}"
    );
}

#[test]
fn a_conditional_companion_does_not_suppress() {
    let source = format!(
        "{TARIFF_SELECTION}
predicate Flagged(meter: Subject)

transformation flag(m):
    admit Flagged(m)

invariant conditional_backstop:
    Reading(m, d, _) implies (Flagged(m) implies (exists e: Tariff(m, _, e) and e on_or_before d))
"
    );
    let found = lints_of(&source);
    assert!(
        governing_finding(&found).is_some(),
        "a conditional witness is not a guarantee: {found:?}"
    );
}

#[test]
fn an_undated_existence_companion_does_not_suppress() {
    let source = format!(
        "{TARIFF_SELECTION}
invariant undated_backstop:
    Reading(m, d, _) implies (exists e: Tariff(m, _, e))
"
    );
    let found = lints_of(&source);
    assert!(
        governing_finding(&found).is_some(),
        "some-P-somewhere is not an effective-by-coordinate witness: {found:?}"
    );
}

// An invariant cannot back itself: the companion is by definition a
// different rule.
#[test]
fn an_invariant_cannot_suppress_its_own_finding() {
    let found = lints_of(
        r#"
program self_backing

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant selects_and_promises:
    (Reading(m, d, _) and Tariff(m, rate, ef) and ef on_or_before d and not (exists later: Tariff(m, _, later) and later on_or_before d and later after ef)) implies (exists e2: Tariff(m, _, e2) and e2 on_or_before d)
"#,
    );
    assert!(
        governing_finding(&found).is_some(),
        "self-suppression must be impossible: {found:?}"
    );
}

// Partial backing pins the semantics of `predicates`: only UNBACKED
// selected predicates are named.
#[test]
fn partial_backing_names_only_the_unbacked_predicate() {
    let found = lints_of(
        r#"
program partial

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)
predicate VatRate(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

transformation set_vat(m, r, ef):
    admit VatRate(m, r, ef)

invariant priced_and_taxed_by_governing_versions:
    (Reading(m, d, _) and Tariff(m, t_rate, t_ef) and t_ef on_or_before d and not (exists t_later: Tariff(m, _, t_later) and t_later on_or_before d and t_later after t_ef) and VatRate(m, v_rate, v_ef) and v_ef on_or_before d and not (exists v_later: VatRate(m, _, v_later) and v_later on_or_before d and v_later after v_ef)) implies t_rate + v_rate <= 1000

invariant every_reading_day_has_a_tariff:
    Reading(m, d, _) implies (exists e: Tariff(m, _, e) and e on_or_before d)
"#,
    );
    let (invariant, predicates) = governing_finding(&found).expect("the unbacked half fires");
    assert_eq!(invariant, "priced_and_taxed_by_governing_versions");
    assert_eq!(predicates, ["VatRate"], "Tariff is backed, VatRate is not");
}

// A companion whose witness lives behind a define still suppresses.
#[test]
fn a_companion_spelled_through_a_define_suppresses() {
    let source = format!(
        "{TARIFF_SELECTION}
define has_effective_tariff(m, d):
    exists e: Tariff(m, _, e) and e on_or_before d

invariant every_reading_day_has_a_tariff:
    Reading(m, d, _) implies has_effective_tariff(m, d)
"
    );
    assert_eq!(lints_of(&source), vec![], "the define expands: {source}");
}

// Generated discipline invariants are machinery, never flagged; the
// authored selection still is.
#[test]
fn discipline_invariants_are_never_flagged() {
    let found = lints_of(
        r#"
program disciplined

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)
    unique by (meter, effective_from)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant reading_priced_by_governing_tariff:
    (Reading(m, d, _) and Tariff(m, rate, ef) and ef on_or_before d and not (exists later: Tariff(m, _, later) and later on_or_before d and later after ef)) implies rate <= 1000
"#,
    );
    assert_eq!(found.len(), 1, "only the authored invariant: {found:?}");
    let (invariant, _) = governing_finding(&found).expect("the authored finding");
    assert_eq!(invariant, "reading_priced_by_governing_tariff");
}

// The mirrored spelling - excluding a strictly EARLIER version -
// selects the earliest-in-force, which is vacuous over an empty set in
// exactly the same way. The tiebreak is direction-insensitive on
// purpose.
#[test]
fn an_earliest_version_selection_fires_too() {
    let found = lints_of(
        r#"
program earliest

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant reading_priced_by_first_tariff:
    (Reading(m, d, _) and Tariff(m, rate, ef) and ef on_or_before d and not (exists earlier: Tariff(m, _, earlier) and earlier on_or_before d and earlier before ef)) implies rate <= 1000
"#,
    );
    let (invariant, predicates) = governing_finding(&found).expect("the earliest twin fires");
    assert_eq!(invariant, "reading_priced_by_first_tariff");
    assert_eq!(predicates, ["Tariff"]);
}

// Direction is load-bearing in the window: a candidate bounded
// on-or-AFTER the coordinate is a forward window, not "the version in
// force at a coordinate".
#[test]
fn a_forward_window_does_not_fire() {
    assert_eq!(
        lints_of(
            r#"
program forward_window

predicate Reading(meter: Subject, day: Date, kwh: Decimal)
predicate Tariff(meter: Subject, rate: Decimal, effective_from: Date)

transformation record_reading(m, d, k):
    admit Reading(m, d, k)

transformation set_tariff(m, r, ef):
    admit Tariff(m, r, ef)

invariant next_tariff_capped:
    (Reading(m, d, _) and Tariff(m, rate, ef) and ef on_or_after d and not (exists later: Tariff(m, _, later) and later after ef)) implies rate <= 1000
"#,
        ),
        vec![]
    );
}

// A future-only witness guarantees a version AFTER the coordinate and
// closes no on-or-before hole - protection in appearance only, so it
// must not suppress.
#[test]
fn a_future_only_companion_does_not_suppress() {
    let source = format!(
        "{TARIFF_SELECTION}
invariant future_backstop:
    Reading(m, d, _) implies (exists e: Tariff(m, _, e) and e after d)
"
    );
    let found = lints_of(&source);
    assert!(
        governing_finding(&found).is_some(),
        "a future-only witness is not a backstop: {found:?}"
    );
}
