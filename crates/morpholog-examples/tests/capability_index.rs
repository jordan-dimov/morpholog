//! `examples/README.md` is the capability index, and it has to stay true.
//!
//! Why it exists: an embedder spent a week designing around a limitation that
//! did not exist, because the gallery is indexed by domain and the example
//! demonstrating what they wanted was named after a different business. Three
//! of their first four capability requests were features they could not find.
//!
//! Why it is tested: an index is a promise that a reader can get from a
//! question to an example, and that promise decays silently. Every example
//! added without an entry makes the index quietly less true, and nothing about
//! writing a `.morph` reminds anyone to touch a table in a different file. So
//! the gate is the reminder.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn index_text() -> String {
    std::fs::read_to_string(examples_dir().join("README.md")).expect("examples/README.md")
}

/// Every directory under `examples/`, which is what a browsing reader sees.
fn example_directories() -> BTreeSet<String> {
    std::fs::read_dir(examples_dir())
        .expect("examples directory")
        .filter_map(|e| {
            let path = e.expect("dir entry").path();
            if !path.is_dir() {
                return None;
            }
            Some(path.file_name()?.to_string_lossy().to_string())
        })
        .collect()
}

/// Directories the index links to, read out of its markdown links.
fn linked_directories(index: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (at, _) in index.match_indices("](") {
        // `match_indices` yields the MATCH, not what follows it - the first
        // version of this read "](" every time and parsed nothing, which the
        // anti-vacuity assertion caught.
        let target = index[at + 2..].split(')').next().unwrap_or_default();
        // Only links into the gallery: `14_margin_call_run/`, not `../README.md`.
        if let Some(dir) = target.strip_suffix('/')
            && !dir.contains("..")
            && !dir.contains('/')
        {
            found.insert(dir.to_string());
        }
    }
    found
}

/// Every example is reachable from the index.
///
/// The failure this prevents is precise: a capability lands in a new example,
/// the index does not mention it, and a reader with exactly that question
/// concludes the language cannot do it. That has already happened three times
/// to one reader, before the index existed at all.
#[test]
fn every_example_appears_in_the_capability_index() {
    let index = index_text();
    let linked = linked_directories(&index);
    let dirs = example_directories();

    assert!(
        dirs.len() > 10,
        "anti-vacuity: found only {} example directories, so this is not reading the gallery",
        dirs.len()
    );
    assert!(
        linked.len() > 10,
        "anti-vacuity: parsed only {} links out of the index",
        linked.len()
    );

    let missing: Vec<&String> = dirs.difference(&linked).collect();
    assert!(
        missing.is_empty(),
        "these examples are in the gallery and not in examples/README.md, so a reader \
         with the question they answer cannot find them: {missing:?}. Add a row saying \
         what each one shows you how to do."
    );
}

/// The index links nothing that does not exist.
#[test]
fn the_capability_index_links_only_real_examples() {
    let index = index_text();
    let dirs = example_directories();
    let dangling: Vec<String> = linked_directories(&index)
        .into_iter()
        .filter(|d| !dirs.contains(d))
        .collect();
    assert!(
        dangling.is_empty(),
        "examples/README.md links directories that are not there: {dangling:?}"
    );
}

/// Each row names a construct, and every construct it names is one the
/// language actually has.
///
/// Checked against the surface-to-IR table in `docs/runtime-semantics.md`,
/// which is the canonical list. An index that advertises a spelling the parser
/// does not accept is worse than no index: it sends a reader to write
/// something that will not compile.
#[test]
fn the_constructs_the_index_names_are_real() {
    let index = index_text();
    let semantics = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/runtime-semantics.md"),
    )
    .expect("docs/runtime-semantics.md");

    // The surface spellings the index claims, taken from its own third column.
    let claimed = [
        "effective by",
        "total over",
        "current pointer by",
        "superseded via",
        "append only",
        "require",
        "define",
        "derived",
        "emit",
        "round(",
        "const",
        "abs(",
        "pre(",
        "forall",
        "min(",
        "max(",
        "sum(",
        "on_or_before",
        "at_or_before",
        "no_longer_than",
    ];
    let mut absent = Vec::new();
    for construct in claimed {
        if !index.contains(construct) {
            // The index no longer mentions it; nothing to verify.
            continue;
        }
        if !semantics.contains(construct) {
            absent.push(construct);
        }
    }
    assert!(
        absent.is_empty(),
        "examples/README.md names constructs the surface-to-IR table does not: {absent:?}. \
         Either the spelling is wrong or the table is out of date - both matter, because \
         one sends a reader to write something that will not parse."
    );
}
