//! Re-export of the shared test helpers in `morpholog-test-support`.
//!
//! Each per-example integration test declares `mod common;` to bring
//! these helpers into scope under a single `common::` path. Keeping
//! this thin re-export avoids touching every test file when helpers
//! move - the test file imports stay `use common::{subj, dec, ...};`
//! regardless of whether the helpers live here or in the support
//! crate.

#![allow(unused_imports)]

pub use morpholog_test_support::*;

/// The worked-example programmes, enumerated for the cross-example
/// property tests (round-trip, format-smoke, validation, guarantees).
/// Test-only successor to the former public
/// `morpholog_examples::all_programs()` registry, removed when the CLI
/// moved to parsing `.morph` files by path instead of resolving built-in
/// names. A new example must be added here so the cross-example tests
/// cover it.
///
/// `dead_code`-allowed because `common` is compiled into every per-example
/// test binary, most of which include it only for the `subj`/`dec`/...
/// helpers and never call this.
#[allow(dead_code)]
pub fn all_programs() -> Vec<morpholog_core::Program> {
    use morpholog_examples::*;
    vec![
        settlement_netting::program(),
        verified_revenue::program(),
        double_entry_ledger::program(),
        approval_controls::program(),
        insurance_claim_settlement::program(),
        clinical_trial_enrolment::program(),
        chess_transition_invariants::program(),
        kyc_sanctions_screening::program(),
        carbon_credit_provenance::program(),
        trade_lifecycle::program(),
        borrowing_base::program(),
        laytime_demurrage::program(),
        biometric_identification_oversight::program(),
        margin_call_run::program(),
    ]
}
