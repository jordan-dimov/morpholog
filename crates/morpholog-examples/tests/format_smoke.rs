//! Smoke test that pins each example's pretty-printer output starts
//! sensibly. Not byte-for-byte; we assert key tokens are present.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::format_program;
use morpholog_examples::all_programs;

#[test]
fn every_program_pretty_prints_with_header_and_recognisable_content() {
    for p in all_programs() {
        let s = format_program(&p);
        assert!(
            s.starts_with(&format!("program {}", p.name)),
            "expected program header for {}; got:\n{}",
            p.name,
            s
        );
        // Each programme has at least one transformation; the
        // rendering must include the `transformation` keyword.
        if !p.transformations.is_empty() {
            assert!(
                s.contains("transformation "),
                "expected `transformation` keyword in render of {}",
                p.name
            );
        }
        // If invariants exist, the keyword appears.
        if !p.invariants.is_empty() {
            assert!(
                s.contains("invariant "),
                "expected `invariant` keyword in render of {}",
                p.name
            );
        }
        // If derived claims exist, the keyword appears.
        if !p.derived_claims.is_empty() {
            assert!(
                s.contains("derived "),
                "expected `derived` keyword in render of {}",
                p.name
            );
        }
    }
}
