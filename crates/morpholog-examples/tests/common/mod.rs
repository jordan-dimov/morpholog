//! Re-export of the shared test helpers in `morpholog-test-support`.
//!
//! Test files import `use common::{subj, dec, ...};`, so helpers can move
//! between here and the support crate without touching them.

#![allow(
    dead_code,
    unused_imports,
    reason = "each test binary uses a different subset of these helpers"
)]

pub(crate) use morpholog_test_support::*;

/// Every worked-example programme, for the cross-example tests. A new
/// `.morph` is covered as soon as it is added.
pub(crate) fn all_programs() -> Vec<morpholog_core::Program> {
    morpholog_examples::all_programs()
}
