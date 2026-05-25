//! Insurance claim settlement example.
//!
//! Authored as surface source at
//! `examples/05_insurance_claim_settlement/insurance_claim_settlement.morph`;
//! this module parses it and exposes the registered program plus the
//! by-name accessors the tests use, including the `PolicyLimitUsage`
//! derived claim. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{DerivedClaim, Invariant, PredicateDecl, Program, Transformation};

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

pub fn paid_implies_authorised() -> Invariant {
    crate::invariant(&PROGRAM, "paid_implies_authorised")
}

pub fn at_most_one_policy_per_id() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_policy_per_id")
}

pub fn at_most_one_claim_report_per_id() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_claim_report_per_id")
}

pub fn paid_implies_headroom() -> Invariant {
    crate::invariant(&PROGRAM, "paid_implies_headroom")
}

pub fn at_most_one_headroom_per_policy() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_headroom_per_policy")
}

pub fn headroom_consumed_by_payment() -> Invariant {
    crate::invariant(&PROGRAM, "headroom_consumed_by_payment")
}

pub fn settlement_id_uniquely_identifies_payment() -> Invariant {
    crate::invariant(&PROGRAM, "settlement_id_uniquely_identifies_payment")
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

pub fn authorise_settlement() -> Transformation {
    crate::transformation(&PROGRAM, "authorise_settlement")
}

pub fn policy_limit_usage() -> DerivedClaim {
    crate::derived(&PROGRAM, "PolicyLimitUsage")
}
