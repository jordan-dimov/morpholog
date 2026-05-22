//! Round-trip property test: every worked example formats to canonical
//! `.morph` text and parses back to the equivalent `Program` IR.
//!
//! The contract being tested:
//!
//! ```text
//! parse_program(format_program(p)) == Ok(p)   for every p in all_programs()
//! ```
//!
//! This is the closing property test for the parser arc. It ties
//! the formatter and the parser to each other: any drift between
//! the two sides surfaces as a structural inequality. The kernel's
//! `Program` derives `PartialEq`, so the comparison is exact.
//!
//! The test runs across every worked example registered in
//! `morpholog_examples::all_programs()`. Adding a new example
//! automatically extends the property's coverage; no per-example
//! test is needed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::format_program;
use morpholog_examples::all_programs;
use morpholog_surface::parse_program;

#[test]
fn every_worked_example_round_trips() {
    let mut failures: Vec<String> = Vec::new();

    for program in all_programs() {
        let name = program.name.clone();
        let formatted = format_program(&program);
        match parse_program(&formatted) {
            Ok(reparsed) => {
                if reparsed != program {
                    failures.push(format!(
                        "{name}: round-trip changed the IR\n--- formatted ---\n{formatted}\n--- expected ---\n{:#?}\n--- got ---\n{:#?}",
                        program, reparsed,
                    ));
                }
            }
            Err(diagnostics) => {
                let msgs: Vec<String> = diagnostics.iter().map(|d| d.message.clone()).collect();
                failures.push(format!(
                    "{name}: parse failed on formatted output\n--- formatted ---\n{formatted}\n--- diagnostics ---\n{}",
                    msgs.join("\n"),
                ));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "round-trip failed for {} worked example(s):\n\n{}",
            failures.len(),
            failures.join("\n\n=====\n\n"),
        );
    }
}
