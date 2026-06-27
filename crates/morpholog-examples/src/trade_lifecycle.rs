//! Trade lifecycle example.
//!
//! Authored as surface source at
//! `examples/10_trade_lifecycle/trade_lifecycle.morph`; this module
//! parses it and exposes the registered program plus the by-name
//! accessors the tests use. There is no hand-built IR: the `.morph`
//! file is the source of truth.

use std::sync::LazyLock;

use morpholog_core::{Definition, DerivedClaim, Invariant, Program, Transformation};

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "trade_lifecycle",
        include_str!("../../../examples/10_trade_lifecycle/trade_lifecycle.morph"),
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

pub fn capture_trade() -> Transformation {
    crate::transformation(&PROGRAM, "capture_trade")
}

pub fn amend_trade_terms() -> Transformation {
    crate::transformation(&PROGRAM, "amend_trade_terms")
}

pub fn grant_confirm_authority() -> Transformation {
    crate::transformation(&PROGRAM, "grant_confirm_authority")
}

pub fn confirm_trade() -> Transformation {
    crate::transformation(&PROGRAM, "confirm_trade")
}

pub fn correct_official_price() -> Transformation {
    crate::transformation(&PROGRAM, "correct_official_price")
}

pub fn settle_trade() -> Transformation {
    crate::transformation(&PROGRAM, "settle_trade")
}

pub fn set_position_limit() -> Transformation {
    crate::transformation(&PROGRAM, "set_position_limit")
}

pub fn terms_timeline() -> DerivedClaim {
    crate::derived(&PROGRAM, "TermsTimeline")
}
