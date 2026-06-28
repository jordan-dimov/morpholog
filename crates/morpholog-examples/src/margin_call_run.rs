//! Daily margin call run, governed for completeness (example 14).
//!
//! Authored as surface source at
//! `examples/14_margin_call_run/margin_call_run.morph`; this module parses
//! it and exposes the registered program plus the by-name accessors the
//! tests use. There is no hand-built IR: the `.morph` file is the source
//! of truth. This is the gallery's first *set-valued proposal* - the risk
//! engine submits the whole batch of called accounts as one all-or-nothing
//! decision, and the run is admitted only if it is complete and exact.

use std::sync::LazyLock;

use morpholog_core::{Invariant, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "margin_call_run",
        include_str!("../../../examples/14_margin_call_run/margin_call_run.morph"),
    )
});

pub fn program() -> Program {
    PROGRAM.clone()
}

pub fn all_invariants() -> Vec<Invariant> {
    PROGRAM.invariants.clone()
}

pub fn issue_margin_run() -> Transformation {
    crate::transformation(&PROGRAM, "issue_margin_run")
}
