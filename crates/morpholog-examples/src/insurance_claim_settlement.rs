//! Insurance claim settlement example.
//!
//! Authored as surface source at
//! `examples/05_insurance_claim_settlement/insurance_claim_settlement.morph`;
//! this module parses it and exposes the registered program plus the
//! by-name accessors the tests use, including the `PolicyLimitUsage`
//! derived claim. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Definition, DerivedClaim, Invariant, PredicateDecl, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "insurance_claim_settlement",
        include_str!(
            "../../../examples/05_insurance_claim_settlement/insurance_claim_settlement.morph"
        ),
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

pub fn paid_implies_authorised() -> Invariant {
    crate::invariant(&PROGRAM, "paid_implies_authorised")
}

pub fn paid_implies_headroom() -> Invariant {
    crate::invariant(&PROGRAM, "paid_implies_headroom")
}

pub fn issue_policy() -> Transformation {
    crate::transformation(&PROGRAM, "issue_policy")
}

pub fn report_claim() -> Transformation {
    crate::transformation(&PROGRAM, "report_claim")
}

pub fn grant_settlement_authority() -> Transformation {
    crate::transformation(&PROGRAM, "grant_settlement_authority")
}

pub fn set_coverage_terms() -> Transformation {
    crate::transformation(&PROGRAM, "set_coverage_terms")
}

pub fn authorise_settlement() -> Transformation {
    crate::transformation(&PROGRAM, "authorise_settlement")
}

pub fn policy_limit_usage() -> DerivedClaim {
    crate::derived(&PROGRAM, "PolicyLimitUsage")
}
