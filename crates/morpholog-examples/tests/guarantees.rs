//! Integration tests for `inspect guarantees` (`morpholog_core::guarantees`).
//!
//! Exercised across *every* worked example, not just carbon, so the
//! derivation is demonstrably general - a mechanical reading of any
//! programme's invariants, never handcrafted to flatter one example.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::all_programs;
use morpholog_core::{Guarantee, guarantees, render_guarantees};
use morpholog_examples::{approval_controls, carbon_credit_provenance as cc};

#[test]
fn every_registered_program_yields_one_guarantee_per_invariant() {
    for program in all_programs() {
        let gs = guarantees(&program);
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
    let gs = guarantees(&cc::program());
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
    let gs = guarantees(&cc::program());
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
    let gs = guarantees(&program);
    let prose = render_guarantees(&program.name, &gs);

    assert_eq!(prose, render_guarantees(&program.name, &gs));
    assert!(prose.contains("Guarantees of `carbon_credit_provenance`"));
    assert!(prose.contains("retirement_terminal"));
    assert!(prose.contains("forbids:"));
}

#[test]
fn a_program_with_no_invariants_guarantees_nothing_structurally() {
    // approval_controls declares no invariants - revocation prevents
    // future approvals via a gate, not an invariant.
    let program = approval_controls::program();
    let gs = guarantees(&program);
    assert!(gs.is_empty());
    assert!(
        render_guarantees(&program.name, &gs).contains("nothing structurally impossible"),
        "render was: {}",
        render_guarantees(&program.name, &gs),
    );
}

#[test]
fn guarantees_json_round_trips() {
    let gs = guarantees(&cc::program());
    let json = serde_json::to_string(&gs).unwrap();
    let parsed: Vec<Guarantee> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, gs);
}
