//! Re-export of the shared test helpers in `morpholog-test-support`.
//!
//! Each per-example integration test declares `mod common;` to bring
//! these helpers into scope under a single `common::` path. Keeping
//! this thin re-export avoids touching every test file when helpers
//! move - the test file imports stay `use common::{subj, dec, ...};`
//! regardless of whether the helpers live here or in the support
//! crate.

#![allow(unused_imports)]

pub use morpholog_test_support::*;

/// The worked-example programmes, enumerated for the cross-example
/// property tests (round-trip, format-smoke, validation, guarantees).
/// Delegates to the registry `build.rs` auto-discovers from `examples/`,
/// so a new example is covered the moment its `.morph` is added - no list
/// to forget.
///
/// `dead_code`-allowed because `common` is compiled into every per-example
/// test binary, most of which include it only for the `subj`/`dec`/...
/// helpers and never call this.
#[allow(dead_code)]
pub fn all_programs() -> Vec<morpholog_core::Program> {
    morpholog_examples::all_programs()
}
