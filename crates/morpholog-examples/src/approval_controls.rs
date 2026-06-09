//! Approval controls example.
//!
//! Authored as surface source at
//! `examples/04_approval_controls/approval_controls.morph`; this module
//! parses it and exposes the registered program plus the by-name
//! accessors the tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Definition, Invariant, PredicateDecl, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "approval_controls",
        include_str!("../../../examples/04_approval_controls/approval_controls.morph"),
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

pub fn grant_approval_authority() -> Transformation {
    crate::transformation(&PROGRAM, "grant_approval_authority")
}

pub fn revoke_approval_authority() -> Transformation {
    crate::transformation(&PROGRAM, "revoke_approval_authority")
}

pub fn approve_document() -> Transformation {
    crate::transformation(&PROGRAM, "approve_document")
}

pub fn grant_approval_limit() -> Transformation {
    crate::transformation(&PROGRAM, "grant_approval_limit")
}

pub fn revoke_approval_limit() -> Transformation {
    crate::transformation(&PROGRAM, "revoke_approval_limit")
}

pub fn approve_within_limit() -> Transformation {
    crate::transformation(&PROGRAM, "approve_within_limit")
}
