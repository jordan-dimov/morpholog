//! Laytime and demurrage example.
//!
//! Authored as surface source at
//! `examples/12_laytime_demurrage/laytime.morph`; this module parses
//! it and exposes the registered program plus the by-name accessors
//! the tests use. There is no hand-built IR: the `.morph` file is the
//! source of truth.

use std::sync::LazyLock;

use morpholog_core::{DerivedClaim, Invariant, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "laytime_demurrage",
        include_str!("../../../examples/12_laytime_demurrage/laytime.morph"),
    )
});

pub fn program() -> Program {
    PROGRAM.clone()
}

pub fn all_invariants() -> Vec<Invariant> {
    PROGRAM.invariants.clone()
}

pub fn fix_voyage() -> Transformation {
    crate::transformation(&PROGRAM, "fix_voyage")
}

pub fn tender_nor() -> Transformation {
    crate::transformation(&PROGRAM, "tender_nor")
}

pub fn commence_laytime() -> Transformation {
    crate::transformation(&PROGRAM, "commence_laytime")
}

pub fn record_counting_interval() -> Transformation {
    crate::transformation(&PROGRAM, "record_counting_interval")
}

pub fn complete_cargo_ops() -> Transformation {
    crate::transformation(&PROGRAM, "complete_cargo_ops")
}

pub fn declare_capacity() -> Transformation {
    crate::transformation(&PROGRAM, "declare_capacity")
}

pub fn load_parcel() -> Transformation {
    crate::transformation(&PROGRAM, "load_parcel")
}

pub fn agree_demurrage_rate() -> Transformation {
    crate::transformation(&PROGRAM, "agree_demurrage_rate")
}

pub fn settle_demurrage() -> Transformation {
    crate::transformation(&PROGRAM, "settle_demurrage")
}

pub fn time_on_demurrage() -> DerivedClaim {
    crate::derived(&PROGRAM, "TimeOnDemurrage")
}

pub fn demurrage_due() -> DerivedClaim {
    crate::derived(&PROGRAM, "DemurrageDue")
}
