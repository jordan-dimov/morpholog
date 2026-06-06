//! Chess transition invariants example.
//!
//! Authored as surface source at
//! `examples/07_chess_transition_invariants/chess.morph`; this module
//! parses it and exposes the registered program plus the by-name
//! accessors the tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Invariant, PredicateDecl, Program};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "chess_transition_invariants",
        include_str!("../../../examples/07_chess_transition_invariants/chess.morph"),
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
