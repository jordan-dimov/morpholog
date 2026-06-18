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
    lints(&program)
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
            Lint::GateVsInvariant { .. } => None,
        })
        .unwrap_or_default()
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
        let found = lints(&program);
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
