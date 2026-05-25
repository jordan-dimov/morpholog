//! KYC sanctions and PEP screening example.
//!
//! Authored as surface source at
//! `examples/08_kyc_sanctions_screening/kyc.morph`; this module parses it
//! and exposes the registered program plus the by-name accessors the
//! tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{IntentDecl, Invariant, PredicateDecl, Program, Transformation};

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

pub fn all_intents() -> Vec<IntentDecl> {
    PROGRAM.intents.clone()
}

pub fn all_invariants() -> Vec<Invariant> {
    PROGRAM.invariants.clone()
}

pub fn all_transformations() -> Vec<Transformation> {
    PROGRAM.transformations.clone()
}

pub fn at_most_one_current_screening_per_customer_and_list_type() -> Invariant {
    crate::invariant(
        &PROGRAM,
        "at_most_one_current_screening_per_customer_and_list_type",
    )
}

pub fn onboarded_requires_current_clean_sanctions() -> Invariant {
    crate::invariant(&PROGRAM, "onboarded_requires_current_clean_sanctions")
}

pub fn onboarded_requires_current_clean_pep() -> Invariant {
    crate::invariant(&PROGRAM, "onboarded_requires_current_clean_pep")
}

pub fn onboarded_requires_no_unresolved_match() -> Invariant {
    crate::invariant(&PROGRAM, "onboarded_requires_no_unresolved_match")
}

pub fn register_customer() -> Transformation {
    crate::transformation(&PROGRAM, "register_customer")
}

pub fn request_screening() -> Transformation {
    crate::transformation(&PROGRAM, "request_screening")
}

pub fn record_clean_screening_result() -> Transformation {
    crate::transformation(&PROGRAM, "record_clean_screening_result")
}

pub fn record_match_screening_result() -> Transformation {
    crate::transformation(&PROGRAM, "record_match_screening_result")
}

pub fn adjudicate_match_as_false_positive() -> Transformation {
    crate::transformation(&PROGRAM, "adjudicate_match_as_false_positive")
}

pub fn onboard_customer() -> Transformation {
    crate::transformation(&PROGRAM, "onboard_customer")
}

pub fn reject_customer() -> Transformation {
    crate::transformation(&PROGRAM, "reject_customer")
}
