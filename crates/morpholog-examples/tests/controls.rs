//! Worked-example coverage of the gate-protection map - the `front_loads`
//! cross-link `inspect controls` draws between each gate and the standing
//! invariant it pre-checks. Exercised over the real examples so the
//! correspondence is demonstrably mechanical, including the honest
//! non-pairing a `sum(..) <= ..` cap produces.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::all_programs;
use morpholog_core::controls;
use morpholog_examples::{
    approval_controls, biometric_identification_oversight as bio, trade_lifecycle,
};

#[test]
fn every_example_front_loads_link_names_a_declared_invariant() {
    for program in all_programs() {
        let declared: BTreeSet<String> = program
            .invariants
            .iter()
            .map(|i| i.name.to_string())
            .collect();
        for t in &controls(&program).transformations {
            for link in t.gates.iter().flat_map(|g| &g.front_loads) {
                assert!(
                    declared.contains(&link.invariant),
                    "program `{}`: front-loads names undeclared invariant `{}`",
                    program.name,
                    link.invariant,
                );
                assert!(
                    !link.shared.is_empty(),
                    "a link names the shared predicates"
                );
                assert!(
                    !link.failure_shape.is_empty(),
                    "a link renders its failure shape"
                );
            }
        }
    }
}

#[test]
fn biometric_two_verifications_gate_front_loads_its_standing_invariant() {
    let matrix = controls(&bio::program());
    let decide = matrix
        .transformations
        .iter()
        .find(|t| t.transformation == "decide_on_identification")
        .expect("decision transformation present");
    let link = decide
        .gates
        .iter()
        .flat_map(|g| &g.front_loads)
        .find(|l| l.invariant == "decision_rests_on_two_distinct_prior_verifications")
        .expect("the decision gate front-loads its standing invariant");
    assert!(
        link.triggered_by
            .contains(&"IdentificationDecision".to_string()),
        "{link:?}"
    );
    assert!(
        link.shared.contains(&"MatchVerified".to_string()),
        "{link:?}"
    );
}

#[test]
fn trade_terms_gate_front_loads_the_backstop_not_the_quantity_cap() {
    // The settle gate requires effective terms, so it front-loads
    // `settled_date_has_effective_terms`. The quantity cap is a
    // `sum(..) <= qty` consequent with no positively-required predicate,
    // so it is honestly NOT front-loaded - the gate does not pre-check it.
    let matrix = controls(&trade_lifecycle::program());
    let settle = matrix
        .transformations
        .iter()
        .find(|t| t.transformation == "settle_trade")
        .expect("settle transformation present");
    let linked: BTreeSet<&str> = settle
        .gates
        .iter()
        .flat_map(|g| &g.front_loads)
        .map(|l| l.invariant.as_str())
        .collect();
    assert!(
        linked.contains("settled_date_has_effective_terms"),
        "the terms gate front-loads the backstop: {linked:?}",
    );
    assert!(
        !linked.contains("settled_within_effective_terms"),
        "the sum cap is honestly left unlinked: {linked:?}",
    );
}

#[test]
fn approval_authority_gates_front_load_nothing() {
    // Authority is an action-time gate with no standing-invariant
    // counterpart (revoking it does not invalidate past approvals), so the
    // map draws no front-loads link - the correct doctrine, not a gap.
    let matrix = controls(&approval_controls::program());
    let links = matrix
        .transformations
        .iter()
        .flat_map(|t| &t.gates)
        .flat_map(|g| &g.front_loads)
        .count();
    assert_eq!(links, 0, "authority gates have no invariant to front-load");
}

#[test]
fn trade_front_line_coverage_separates_front_loaded_from_backstop() {
    // The invariant-side view: the terms backstop is front-loaded by the
    // settle gate, while the quantity cap (a sum(..) <= qty consequent) is
    // a true backstop - triggered by transformations, but no gate
    // front-loads it.
    let cov = controls(&trade_lifecycle::program()).front_line_coverage;
    let backstop = cov
        .iter()
        .find(|i| i.invariant == "settled_within_effective_terms")
        .expect("the quantity cap is an authored implication invariant");
    assert!(backstop.front_loaded_by.is_empty(), "{backstop:?}");
    assert!(
        !backstop.triggered_by_transformations.is_empty(),
        "settle_trade triggers it, so it is a backstop not dormant: {backstop:?}"
    );

    let front = cov
        .iter()
        .find(|i| i.invariant == "settled_date_has_effective_terms")
        .expect("the terms backstop is an authored implication invariant");
    assert!(
        front
            .front_loaded_by
            .iter()
            .any(|g| g.transformation == "settle_trade"),
        "{front:?}"
    );
}

#[test]
fn every_front_line_coverage_row_names_a_declared_invariant() {
    for program in all_programs() {
        let declared: BTreeSet<String> = program
            .invariants
            .iter()
            .map(|i| i.name.to_string())
            .collect();
        for row in &controls(&program).front_line_coverage {
            assert!(
                declared.contains(&row.invariant),
                "program `{}`: coverage names undeclared invariant `{}`",
                program.name,
                row.invariant,
            );
            assert!(
                !row.failure_shape.is_empty(),
                "every row renders its failure shape"
            );
        }
    }
}
