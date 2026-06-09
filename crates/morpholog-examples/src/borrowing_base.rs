//! Borrowing-base example.
//!
//! Authored as surface source at
//! `examples/11_borrowing_base/borrowing_base.morph`; this module parses
//! it and exposes the registered program plus the by-name accessors the
//! tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Definition, DerivedClaim, Invariant, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "borrowing_base",
        include_str!("../../../examples/11_borrowing_base/borrowing_base.morph"),
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

pub fn open_facility() -> Transformation {
    crate::transformation(&PROGRAM, "open_facility")
}

pub fn pledge_collateral() -> Transformation {
    crate::transformation(&PROGRAM, "pledge_collateral")
}

pub fn draw() -> Transformation {
    crate::transformation(&PROGRAM, "draw")
}

pub fn facility_utilisation() -> DerivedClaim {
    crate::derived(&PROGRAM, "FacilityUtilisation")
}
