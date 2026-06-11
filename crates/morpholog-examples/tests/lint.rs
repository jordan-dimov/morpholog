//! The gate-vs-invariant lint: the revocation-rewrites-history shape
//! made mechanical by the declared disciplines. Forward direction only
//! (append-only antecedent, pointer consequent); the reverse is
//! correct doctrine and stays silent, as does every worked example -
//! the disciplines sweep left the repo's own doctrine clean, and the
//! cross-example test pins that.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Lint, lints};
use morpholog_surface::parse_program;

fn lints_of(source: &str) -> Vec<Lint> {
    let program = parse_program(source).expect("programme should parse");
    program.validate().expect("programme should validate");
    lints(&program)
}

const TRIP: &str = r#"
program trip

predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

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
    } = &found[0];
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
    } = &found[0];
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
