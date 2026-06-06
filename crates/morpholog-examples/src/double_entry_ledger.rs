//! Double-entry ledger example.
//!
//! Authored as surface source at
//! `examples/03_double_entry_ledger/ledger.morph`; this module parses it
//! and exposes the registered program plus the by-name accessors the
//! tests use, including the `TrialBalanceRow` derived claim. There is no
//! hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{DerivedClaim, Invariant, PredicateDecl, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "double_entry_ledger",
        include_str!("../../../examples/03_double_entry_ledger/ledger.morph"),
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

pub fn journal_entry_has_lines() -> Invariant {
    crate::invariant(&PROGRAM, "journal_entry_has_lines")
}

pub fn post_simple_entry() -> Transformation {
    crate::transformation(&PROGRAM, "post_simple_entry")
}

pub fn post_split_entry() -> Transformation {
    crate::transformation(&PROGRAM, "post_split_entry")
}

pub fn close_period() -> Transformation {
    crate::transformation(&PROGRAM, "close_period")
}

pub fn restate_entry() -> Transformation {
    crate::transformation(&PROGRAM, "restate_entry")
}

pub fn trial_balance_row() -> DerivedClaim {
    crate::derived(&PROGRAM, "TrialBalanceRow")
}
