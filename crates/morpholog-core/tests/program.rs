//! Tests for the [`morpholog_core::Program`] packaging abstraction.
//!
//! `Program` is the smallest public container for a governed domain
//! model: a name, a set of invariants, a set of transformations. Each
//! worked example exposes a `program()` constructor; the tests below
//! verify that:
//!
//! - the constructed `Program`s have the expected stable names,
//! - their invariant and transformation counts match what each example
//!   actually contains,
//! - `transformation(name)` and `invariant(name)` lookups find the
//!   expected items and return `None` for unknown names,
//! - the returned references point at the same data as the direct
//!   per-example constructor functions (i.e. `Program` is a
//!   composition of existing pieces, not a parallel definition).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::examples::{
    claim_standing, double_entry_ledger, revenue_restatement, settlement_netting,
};

#[test]
fn settlement_netting_program_has_expected_shape() {
    let p = settlement_netting::program();
    assert_eq!(p.name, "settlement_netting");
    assert_eq!(p.invariants.len(), 3);
    assert_eq!(p.transformations.len(), 1);

    assert!(p.transformation("create_net_settlement").is_some());
    assert!(p.invariant("net_amount_equals_lines").is_some());
    assert!(p.invariant("no_double_netting").is_some());

    assert!(p.transformation("post_simple_entry").is_none());
    assert!(p.invariant("does_not_exist").is_none());
}

#[test]
fn revenue_restatement_program_has_expected_shape() {
    let p = revenue_restatement::program();
    assert_eq!(p.name, "revenue_restatement");
    assert_eq!(p.invariants.len(), 3);
    assert_eq!(p.transformations.len(), 4);

    assert!(p.transformation("admit_independent_verification").is_some());
    assert!(p.transformation("recognise_bank_revenue").is_some());
    assert!(
        p.transformation("correct_independent_verification")
            .is_some()
    );
    assert!(p.transformation("restate_bank_revenue").is_some());

    assert!(
        p.invariant("current_recognition_matches_current_verification")
            .is_some()
    );
}

#[test]
fn claim_standing_program_has_expected_shape() {
    let p = claim_standing::program();
    assert_eq!(p.name, "claim_standing");
    assert_eq!(p.invariants.len(), 2);
    assert_eq!(p.transformations.len(), 5);

    assert!(p.transformation("admit_independent_verification").is_some());
    assert!(p.transformation("grant_standing").is_some());
    assert!(p.transformation("revoke_standing").is_some());
    assert!(p.transformation("admit_debt_service_revenue").is_some());
    assert!(
        p.transformation("admit_investor_reported_revenue")
            .is_some()
    );

    assert!(p.invariant("admissibility_has_provenance").is_some());
    assert!(p.invariant("admissibility_excludes_revocation").is_some());
}

#[test]
fn double_entry_ledger_program_has_expected_shape() {
    let p = double_entry_ledger::program();
    assert_eq!(p.name, "double_entry_ledger");
    assert_eq!(p.invariants.len(), 3);
    assert_eq!(p.transformations.len(), 4);

    assert!(p.transformation("post_simple_entry").is_some());
    assert!(p.transformation("post_split_entry").is_some());
    assert!(p.transformation("close_period").is_some());
    assert!(p.transformation("restate_entry").is_some());

    assert!(p.invariant("balanced_posted_entry").is_some());
    assert!(p.invariant("journal_entry_has_lines").is_some());
    assert!(p.invariant("at_most_one_direct_successor").is_some());
}

#[test]
fn program_is_composition_not_a_parallel_definition() {
    // Pin the contract that program() composes the existing constructor
    // functions rather than redefining the IR. If a future refactor
    // drifts the two apart (e.g. program() forgets to include a newly
    // added transformation), this assertion catches it.
    let p = settlement_netting::program();
    let direct = settlement_netting::create_net_settlement();
    assert_eq!(
        p.transformation("create_net_settlement"),
        Some(&direct),
        "program's create_net_settlement must equal the direct constructor"
    );

    let direct_inv = settlement_netting::no_double_netting();
    assert_eq!(p.invariant("no_double_netting"), Some(&direct_inv));
}

#[test]
fn unknown_lookups_return_none() {
    // Lookup is the v0 way for the CLI / external callers to find a
    // transformation by name. Make sure missing entries don't panic
    // or wrap-around silently.
    let p = double_entry_ledger::program();
    assert!(p.transformation("not_a_real_transformation").is_none());
    assert!(p.transformation("").is_none());
    assert!(p.invariant("not_a_real_invariant").is_none());
}

#[test]
fn all_programs_registry_contains_every_per_example_program() {
    // `examples::all_programs()` is the canonical built-in registry
    // the CLI uses to resolve a `--program` name supplied on the
    // command line. If a new worked example lands but the contributor
    // forgets to add its `program()` constructor to `all_programs()`,
    // `morpholog propose <new_example> ...` would silently fail with
    // "program not found".
    //
    // Pin the contract by checking that every per-example `program()`
    // is reachable through the registry by name. The list below is
    // load-bearing: it should be updated whenever a new example is
    // added, in the same commit that adds the example to
    // `all_programs()`.
    let registry = morpholog_core::examples::all_programs();
    let registry_names: Vec<&str> = registry.iter().map(|p| p.name.as_str()).collect();

    for expected_name in [
        settlement_netting::program().name.as_str(),
        revenue_restatement::program().name.as_str(),
        claim_standing::program().name.as_str(),
        double_entry_ledger::program().name.as_str(),
    ] {
        assert!(
            registry_names.contains(&expected_name),
            "all_programs() registry must include `{expected_name}`; \
             currently contains: {registry_names:?}"
        );
    }
}

#[test]
fn all_programs_registry_has_unique_names() {
    // The CLI resolves a `--program` name by linear search through
    // `all_programs()` and returns the first match. Duplicate names
    // would make one of the duplicates unreachable, silently. Pin
    // uniqueness so the failure surfaces immediately rather than at
    // CLI invocation time.
    let registry = morpholog_core::examples::all_programs();
    let mut names: Vec<&str> = registry.iter().map(|p| p.name.as_str()).collect();
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        total,
        "program names in all_programs() must be unique"
    );
}
