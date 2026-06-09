//! Verified revenue example.
//!
//! Authored as surface source at
//! `examples/02_verified_revenue/verified_revenue.morph`; this module
//! parses it and exposes the registered program plus the by-name
//! accessors the tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Definition, Invariant, PredicateDecl, Program, Transformation};

/// The purpose subject identifying bank debt-service-coverage usage.
pub const BANK_DEBT_SERVICE: &str = "bank_debt_service";

/// The purpose subject identifying investor reporting usage.
pub const INVESTOR_REPORTING: &str = "investor_reporting";

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "verified_revenue",
        include_str!("../../../examples/02_verified_revenue/verified_revenue.morph"),
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

pub fn definitions() -> Vec<Definition> {
    PROGRAM.definitions.clone()
}

pub fn admit_independent_verification() -> Transformation {
    crate::transformation(&PROGRAM, "admit_independent_verification")
}

pub fn correct_independent_verification() -> Transformation {
    crate::transformation(&PROGRAM, "correct_independent_verification")
}

pub fn grant_standing() -> Transformation {
    crate::transformation(&PROGRAM, "grant_standing")
}

pub fn revoke_standing() -> Transformation {
    crate::transformation(&PROGRAM, "revoke_standing")
}

pub fn admit_debt_service_revenue() -> Transformation {
    crate::transformation(&PROGRAM, "admit_debt_service_revenue")
}

pub fn admit_investor_reported_revenue() -> Transformation {
    crate::transformation(&PROGRAM, "admit_investor_reported_revenue")
}
