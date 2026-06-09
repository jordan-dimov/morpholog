//! Carbon-credit provenance example.
//!
//! Authored as surface source at
//! `examples/09_carbon_credit_provenance/carbon_credit_provenance.morph`;
//! this module parses it and exposes the registered program plus the
//! by-name accessors the tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Definition, Invariant, PredicateDecl, Program, Transformation};

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

pub fn definitions() -> Vec<Definition> {
    PROGRAM.definitions.clone()
}

pub fn issued_unique_by_measurement() -> Invariant {
    crate::invariant(&PROGRAM, "issued_unique_by_measurement")
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
