//! Worked examples - the canonical Morpholog illustrations.
//!
//! The single authored form of each example is its surface-syntax
//! `.morph` file under `examples/<dir>/<example>.morph`. Each module here
//! embeds that source, parses it once into a [`morpholog_core::Program`],
//! and exposes it plus the by-name accessors (`program`, `all_invariants`,
//! individual transformations and invariants) that the `morpholog-core`
//! and `morpholog-postgres` tests use as fixtures. There is no hand-built
//! IR: the `.morph` file is the source of truth, so the teaching surface
//! and the runnable program cannot drift.
//!
//! **Not a stable API.** This module is `pub` so tests and documentation
//! can share the canonical programs; it is not a user-facing API.

use morpholog_core::{Invariant, Program, Transformation};

/// Parse a built-in example's embedded `.morph` source into its
/// [`Program`]. A built-in example that fails to parse is a build-time
/// bug, so this panics rather than returning an error.
pub(crate) fn parse_example(name: &str, source: &str) -> Program {
    morpholog_surface::parse_program(source).unwrap_or_else(|diagnostics| {
        let rendered: String = diagnostics.iter().map(|d| d.render(name, source)).collect();
        panic!("built-in example `{name}` must parse:\n{rendered}")
    })
}

/// Clone a named invariant out of a parsed example program. A missing
/// name is a bug in the accessor, not a runtime condition.
pub(crate) fn invariant(program: &Program, name: &str) -> Invariant {
    program
        .invariant(name)
        .unwrap_or_else(|| panic!("example invariant `{name}` not found"))
        .clone()
}

/// Clone a named transformation out of a parsed example program.
pub(crate) fn transformation(program: &Program, name: &str) -> Transformation {
    program
        .transformation(name)
        .unwrap_or_else(|| panic!("example transformation `{name}` not found"))
        .clone()
}

/// Clone a derived claim (by its output predicate name) out of a parsed
/// example program.
pub(crate) fn derived(program: &Program, predicate: &str) -> morpholog_core::DerivedClaim {
    program
        .derived_claim(predicate)
        .unwrap_or_else(|| panic!("example derived claim `{predicate}` not found"))
        .clone()
}

pub mod approval_controls;
pub mod biometric_identification_oversight;
pub mod borrowing_base;
pub mod carbon_credit_provenance;
pub mod chess_transition_invariants;
pub mod clinical_trial_enrolment;
pub mod double_entry_ledger;
pub mod insurance_claim_settlement;
pub mod kyc_sanctions_screening;
pub mod laytime_demurrage;
pub mod margin_call_run;
pub mod settlement_netting;
pub mod trade_lifecycle;
pub mod verified_revenue;
