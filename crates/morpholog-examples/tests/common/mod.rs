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
