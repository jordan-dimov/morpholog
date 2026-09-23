//! Re-export of the shared test helpers in `morpholog-test-support`.
//!
//! Test files import `use common::{subj, dec, ...};`, so helpers can move
//! between here and the support crate without touching them.

#![allow(unused_imports)]

pub use morpholog_test_support::*;

/// Every worked-example programme, for the cross-example tests. A new
/// `.morph` is covered as soon as it is added.
///
/// `dead_code` is allowed because most test binaries use `common` only for
/// the other helpers.
#[allow(dead_code)]
pub fn all_programs() -> Vec<morpholog_core::Program> {
    morpholog_examples::all_programs()
}
