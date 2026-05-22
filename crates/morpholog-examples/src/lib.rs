//! Worked examples - the IR for the canonical Morpholog illustrations.
//!
//! These constructors serve double duty as both test fixtures (used by
//! `morpholog-core`'s own tests and by `morpholog-postgres` integration
//! tests) and as the canonical illustrations of what Morpholog programs
//! look like as IR data. The corresponding surface-syntax `.morph` files
//! live under `examples/<dir>/<example>.morph`.
//!
//! **Not a stable API.** This module is `pub` so integration tests and
//! documentation can share canonical IR fixtures; it is *not* a
//! user-facing API and it is *not* the future surface language. The
//! shapes here may change as the IR evolves, and the module may be
//! moved behind a feature flag or out of the public surface entirely
//! once a real parser and example-loading mechanism exist. Treat these
//! as teaching fixtures, not a contract.

pub mod approval_controls;
pub mod chess_transition_invariants;
pub mod clinical_trial_enrolment;
pub mod double_entry_ledger;
pub mod insurance_claim_settlement;
pub mod settlement_netting;
pub mod verified_revenue;

/// All built-in worked example programs, in the order they were
/// developed. Returned as owned [`morpholog_core::Program`] values so callers
/// can iterate, look up a specific one by `name`, or hand each to
/// `propose_against_pg`.
///
/// Used by the CLI's `propose` subcommand to resolve a program name
/// supplied on the command line to its [`morpholog_core::Program`] value. The
/// list is the canonical built-in registry; future user-supplied
/// programs (post-parser) would live alongside, not replace, these.
pub fn all_programs() -> Vec<morpholog_core::Program> {
    vec![
        settlement_netting::program(),
        verified_revenue::program(),
        double_entry_ledger::program(),
        approval_controls::program(),
        insurance_claim_settlement::program(),
        clinical_trial_enrolment::program(),
        chess_transition_invariants::program(),
    ]
}
