//! Settlement netting example.
//!
//! Authored as surface source at
//! `examples/01_settlement_netting/netting.morph`; this module parses it
//! and exposes the registered program plus the by-name accessors the
//! tests use. There is no hand-built IR - the `.morph` file is the source.

use std::sync::LazyLock;

use morpholog_core::{Definition, Invariant, PredicateDecl, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "settlement_netting",
        include_str!("../../../examples/01_settlement_netting/netting.morph"),
    )
});

/// The settlement-netting example as a [`Program`]. Stable identifier:
/// `"settlement_netting"`.
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

pub fn create_net_settlement() -> Transformation {
    crate::transformation(&PROGRAM, "create_net_settlement")
}

pub fn net_settlement_has_lines() -> Invariant {
    crate::invariant(&PROGRAM, "net_settlement_has_lines")
}

pub fn net_amount_equals_lines() -> Invariant {
    crate::invariant(&PROGRAM, "net_amount_equals_lines")
}

pub fn no_double_netting() -> Invariant {
    crate::invariant(&PROGRAM, "no_double_netting")
}
