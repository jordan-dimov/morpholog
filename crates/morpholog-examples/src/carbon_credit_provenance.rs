//! Carbon-credit provenance example.
//!
//! Authored as surface source at
//! `examples/09_carbon_credit_provenance/carbon_credit_provenance.morph`;
//! this module parses it and exposes the registered program plus the
//! by-name accessors the tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Invariant, PredicateDecl, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "carbon_credit_provenance",
        include_str!(
            "../../../examples/09_carbon_credit_provenance/carbon_credit_provenance.morph"
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

pub fn at_most_one_verified_quantity_per_measurement() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_verified_quantity_per_measurement")
}

pub fn no_double_issuance() -> Invariant {
    crate::invariant(&PROGRAM, "no_double_issuance")
}

pub fn credit_backed_by_one_measurement() -> Invariant {
    crate::invariant(&PROGRAM, "credit_backed_by_one_measurement")
}

pub fn single_custody() -> Invariant {
    crate::invariant(&PROGRAM, "single_custody")
}

pub fn retirement_terminal() -> Invariant {
    crate::invariant(&PROGRAM, "retirement_terminal")
}

pub fn at_most_one_obligation_per_id() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_obligation_per_id")
}

pub fn obligation_not_both_satisfied_and_breached() -> Invariant {
    crate::invariant(&PROGRAM, "obligation_not_both_satisfied_and_breached")
}

pub fn grant_accreditation() -> Transformation {
    crate::transformation(&PROGRAM, "grant_accreditation")
}

pub fn revoke_accreditation() -> Transformation {
    crate::transformation(&PROGRAM, "revoke_accreditation")
}

pub fn verify_measurement() -> Transformation {
    crate::transformation(&PROGRAM, "verify_measurement")
}

pub fn attest_measurement() -> Transformation {
    crate::transformation(&PROGRAM, "attest_measurement")
}

pub fn issue_credit() -> Transformation {
    crate::transformation(&PROGRAM, "issue_credit")
}

pub fn transfer_credit() -> Transformation {
    crate::transformation(&PROGRAM, "transfer_credit")
}

pub fn retire_credit() -> Transformation {
    crate::transformation(&PROGRAM, "retire_credit")
}

pub fn raise_obligation() -> Transformation {
    crate::transformation(&PROGRAM, "raise_obligation")
}

pub fn discharge_obligation() -> Transformation {
    crate::transformation(&PROGRAM, "discharge_obligation")
}

pub fn sweep_obligation() -> Transformation {
    crate::transformation(&PROGRAM, "sweep_obligation")
}
