//! Integration tests for `inspect guarantees` (`morpholog_core::guarantees`).
//!
//! Run over every worked example, to show the derivation is a mechanical
//! reading of any programme's invariants, not tuned to one example.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::all_programs;
use morpholog_core::{Guarantee, guarantees, render_guarantees};
use morpholog_examples::{approval_controls, carbon_credit_provenance as cc};

/// Build a PreparedProgram for the analysis entry points.
fn prepared(p: &morpholog_core::Program) -> morpholog_core::PreparedProgram {
    morpholog_core::PreparedProgram::new(p.clone()).expect("fixture is valid")
}

#[test]
fn every_registered_program_yields_one_guarantee_per_invariant() {
    for program in all_programs() {
        let gs = guarantees(&prepared(&program));
        assert_eq!(
            gs.len(),
            program.invariants.len(),
            "program `{}` should yield one guarantee per invariant",
            program.name,
        );
        for (g, inv) in gs.iter().zip(program.invariants.iter()) {
            assert_eq!(g.invariant, inv.name.as_str());
            assert!(!g.rule.is_empty(), "every guarantee renders its rule");
        }
    }
}

#[test]
fn a_not_invariant_names_the_forbidden_state() {
    let gs = guarantees(&prepared(&cc::program()));
    let terminal = gs
        .iter()
        .find(|g| g.invariant == "retirement_terminal")
        .expect("carbon declares retirement_terminal");
    // The bad state is the inner of the `not(...)`.
    let forbids = terminal
        .forbids
        .as_ref()
        .expect("a not-invariant forbids a concrete state");
    assert!(
        forbids.contains("Retired") && forbids.contains("HeldBy"),
        "forbids was: {forbids}",
    );
}

#[test]
fn an_implies_invariant_has_no_mechanical_forbidden_state() {
    let gs = guarantees(&prepared(&cc::program()));
    let double = gs
        .iter()
        .find(|g| g.invariant == "issued_unique_by_measurement")
        .expect("carbon declares issued_unique_by_measurement");
    // An implies-shaped guarantee carries its rule, not a forbids clause.
    assert!(double.forbids.is_none());
    assert!(double.rule.contains("implies"));
}

#[test]
fn render_is_deterministic_prose() {
    let program = cc::program();
    let gs = guarantees(&prepared(&program));
    let prose = render_guarantees(&program.name, &gs);

    assert_eq!(prose, render_guarantees(&program.name, &gs));
    assert!(prose.contains("Guarantees of `carbon_credit_provenance`"));
    assert!(prose.contains("retirement_terminal"));
    assert!(prose.contains("forbids:"));
}

#[test]
fn a_program_with_no_invariants_guarantees_nothing_structurally() {
    let program = morpholog_surface::parse_program(
        "program gates_only\n\npredicate Note(text: Subject)\n\ntransformation note(text):\n    admit Note(text)\n",
    )
    .unwrap();
    let gs = guarantees(&prepared(&program));
    assert!(gs.is_empty());
    assert!(
        render_guarantees(&program.name, &gs).contains("nothing structurally impossible"),
        "render was: {}",
        render_guarantees(&program.name, &gs),
    );
}

#[test]
fn approval_controls_guarantees_document_identity_and_nothing_about_authority() {
    // Revocation stops future approvals through a gate, so no standing
    // rule ties a past approval to live authority. What the record does
    // guarantee is that a document keeps one kind and one amount.
    let gs = guarantees(&prepared(&approval_controls::program()));
    let names: Vec<&str> = gs.iter().map(|g| g.invariant.as_str()).collect();
    assert_eq!(
        names,
        [
            "document_unique_by_doc_id",
            "document_amount_unique_by_doc_id"
        ]
    );
}

#[test]
fn guarantees_json_round_trips() {
    let gs = guarantees(&prepared(&cc::program()));
    let json = serde_json::to_string(&gs).unwrap();
    let parsed: Vec<Guarantee> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, gs);
}
