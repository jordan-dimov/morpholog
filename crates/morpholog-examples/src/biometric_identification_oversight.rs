//! Biometric identification under human oversight (EU AI Act
//! Articles 12 and 14) example.
//!
//! Authored as surface source at
//! `examples/13_biometric_identification_oversight/biometric_oversight.morph`;
//! this module parses it and exposes the registered program plus the
//! by-name accessors the tests use. There is no hand-built IR: the
//! `.morph` file is the source of truth.

use std::sync::LazyLock;

use morpholog_core::{Definition, DerivedClaim, Invariant, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "biometric_identification_oversight",
        include_str!(
            "../../../examples/13_biometric_identification_oversight/biometric_oversight.morph"
        ),
    )
});

pub fn program() -> Program {
    PROGRAM.clone()
}

pub fn all_invariants() -> Vec<Invariant> {
    PROGRAM.invariants.clone()
}

pub fn definitions() -> Vec<Definition> {
    PROGRAM.definitions.clone()
}

pub fn deploy_system() -> Transformation {
    crate::transformation(&PROGRAM, "deploy_system")
}

pub fn place_version_in_service() -> Transformation {
    crate::transformation(&PROGRAM, "place_version_in_service")
}

pub fn assign_oversight() -> Transformation {
    crate::transformation(&PROGRAM, "assign_oversight")
}

pub fn revoke_oversight() -> Transformation {
    crate::transformation(&PROGRAM, "revoke_oversight")
}

pub fn start_use() -> Transformation {
    crate::transformation(&PROGRAM, "start_use")
}

pub fn record_match() -> Transformation {
    crate::transformation(&PROGRAM, "record_match")
}

pub fn end_use() -> Transformation {
    crate::transformation(&PROGRAM, "end_use")
}

pub fn verify_match() -> Transformation {
    crate::transformation(&PROGRAM, "verify_match")
}

pub fn decide_on_identification() -> Transformation {
    crate::transformation(&PROGRAM, "decide_on_identification")
}

pub fn use_period() -> DerivedClaim {
    crate::derived(&PROGRAM, "UsePeriod")
}
