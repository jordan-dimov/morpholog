//! Round-trip property test: every worked-example `.morph` source parses,
//! formats back to canonical `.morph` text, and re-parses to the same IR.
//!
//! The contract being tested:
//!
//! ```text
//! let p = parse_program(src).unwrap();   // the source must parse first
//! parse_program(&format_program(&p)) == Ok(p)
//! ```
//!
//! Any drift between formatter and parser shows up as an exact `Program` inequality. The
//! examples are read from disk, so a new `.morph` example is covered automatically.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use morpholog_core::format::format_program;
use morpholog_surface::parse_program;

/// Collect every `examples/*/*.morph` source, relative to this crate.
fn example_sources() -> Vec<(PathBuf, String)> {
    let examples_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut sources = Vec::new();
    for entry in fs::read_dir(&examples_dir).expect("examples/ directory must exist") {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for file in fs::read_dir(&dir).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|e| e == "morph") {
                let src = fs::read_to_string(&path).unwrap();
                sources.push((path, src));
            }
        }
    }
    sources
}

#[test]
fn every_worked_example_round_trips() {
    let sources = example_sources();
    assert!(
        !sources.is_empty(),
        "expected to find worked-example .morph sources under examples/"
    );

    let mut failures: Vec<String> = Vec::new();

    for (path, src) in &sources {
        let name = path.display();
        let program = match parse_program(src) {
            Ok(p) => p,
            Err(diagnostics) => {
                let msgs: Vec<String> = diagnostics.iter().map(|d| d.message.clone()).collect();
                failures.push(format!("{name}: source did not parse\n{}", msgs.join("\n")));
                continue;
            }
        };
        let formatted = format_program(&program);
        match parse_program(&formatted) {
            Ok(reparsed) => {
                if reparsed != program {
                    failures.push(format!(
                        "{name}: round-trip changed the IR\n--- formatted ---\n{formatted}\n--- expected ---\n{program:#?}\n--- got ---\n{reparsed:#?}",
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
