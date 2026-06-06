//! KYC sanctions and PEP screening example.
//!
//! Authored as surface source at
//! `examples/08_kyc_sanctions_screening/kyc.morph`; this module parses it
//! and exposes the registered program plus the by-name accessors the
//! tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Invariant, PredicateDecl, Program};

// List-type and disposition constants - named so transformation bodies
// (now in the `.morph` source) and tests cannot drift on spelling.
pub const SANCTIONS: &str = "sanctions";
pub const PEP: &str = "pep";

pub const DISP_CLEAN: &str = "clean";
pub const DISP_MATCH: &str = "match";
pub const DISP_ADJUDICATED_CLEAR: &str = "adjudicated_clear";

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "kyc_sanctions_screening",
        include_str!("../../../examples/08_kyc_sanctions_screening/kyc.morph"),
    )
});

pub fn program() -> Program {
    PROGRAM.clone()
}

pub fn all_predicates() -> Vec<PredicateDecl> {
    PROGRAM.predicates.clone()
}

pub fn all_invariants() -> Vec<Invariant> {
    PROGRAM.invariants.clone()
}
