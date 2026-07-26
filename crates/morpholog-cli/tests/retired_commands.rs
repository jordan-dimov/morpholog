//! No message may tell a reader to run a command that does not exist.
//!
//! The `audit` rename swept the CLI crate and missed a runtime error in
//! another crate that told the reader to run checkpoint, unqualified. A
//! user who followed it got clap's exit 2, and smoking the published
//! release is what found it - the rename's own tests all passed.
//!
//! The vocabulary here is DERIVED from the binary's own `--help` tree,
//! not from a list of retired spellings. Two earlier cuts kept such a
//! list, and widening it found real sites both times: first the
//! morpholog-prefixed spelling, then a sentence-leading "Run" the
//! case-sensitive match skipped. A list cannot be complete, because the
//! thing it must know - every way prose might name a dead command - is
//! open-ended. The valid command tree is closed, so the check reads that
//! instead and asks whether each reference is in it.
//!
//! What stays heuristic is the DETECTOR: which backticked spans count as
//! command references. That is bounded by the two shapes prose actually
//! uses, and a shape nobody anticipated still escapes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_morpholog");

/// Command paths the `audit` rename retired. Distinct contract from the
/// prose check: these must stay rejected, so a re-added alias fails here.
const RETIRED_PATHS: &[&[&str]] = &[
    &["evidence"],
    &["evidence", "export"],
    &["evidence", "verify"],
    &["checkpoint"],
    &["keygen"],
    &["verify"],
];

#[test]
fn the_cli_rejects_every_retired_command_path() {
    for path in RETIRED_PATHS {
        let out = Command::new(BIN)
            .args(*path)
            .arg("--help")
            .output()
            .expect("the binary runs");
        // Exit 2 is clap refusing the path. Not 127 (no such file) and
        // not 0 - a smoke check of mine read 127 as a rejection once,
        // which proved only that it had the wrong path to the binary.
        assert_eq!(
            out.status.code(),
            Some(2),
            "`morpholog {}` must be rejected by clap, got {:?}",
            path.join(" "),
            out.status.code()
        );
    }
}

#[test]
fn every_command_a_message_names_exists() {
    let valid = derive_command_tree();
    // The tree is the source of truth, so a broken derivation must not
    // read as "nothing to check": these are paths the binary certainly
    // has, and the audit group is what the rename created.
    for expected in [
        "check",
        "propose",
        "audit",
        "audit verify",
        "audit verify-pack",
    ] {
        assert!(
            valid.contains(expected),
            "derivation is broken: `{expected}` missing from the command tree"
        );
    }

    // Every word the command tree uses at any depth, so the "run x"
    // shape can tell a command from an ordinary identifier.
    let known_words: BTreeSet<String> = valid
        .iter()
        .flat_map(|path| path.split(' ').map(str::to_string))
        .collect();

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();
    visit_rust_files(&root.join("crates"), &mut |path| {
        let text = std::fs::read_to_string(path).expect("source readable");
        for (number, line) in text.lines().enumerate() {
            for reference in command_references(line, &known_words) {
                if !valid.contains(&reference) {
                    offenders.push(format!(
                        "{}:{} names `morpholog {reference}`, which the CLI does not have",
                        path.display(),
                        number + 1
                    ));
                }
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "these messages name commands that do not exist:\n{}",
        offenders.join("\n")
    );
}

/// The two shapes prose uses to name a command: a backticked span
/// starting with the binary name, and one introduced by the word "run".
/// Returns each as a normalised path.
fn command_references(line: &str, known_words: &BTreeSet<String>) -> Vec<String> {
    let mut found = Vec::new();
    let lowered = line.to_lowercase();
    // Carry the text that preceded the opening backtick. Searching for
    // the span's position instead put the backtick itself at the end of
    // the prefix, so the "run" shape never matched - the very shape the
    // original defect took.
    let mut preceding = "";
    for (index, span) in lowered.split('`').enumerate() {
        if index % 2 == 0 {
            preceding = span;
            continue;
        }
        if let Some(rest) = span.strip_prefix("morpholog") {
            if let Some(path) = command_path(rest) {
                found.push(path);
            }
        } else if preceding.trim_end().ends_with("run") {
            // "run x" means "run morpholog x" only when x is a word the
            // CLI actually uses. Without that, the shape also reads a
            // doc comment about running a closure against a body.
            if let Some(path) = command_path(span) {
                let first = path.split(' ').next().unwrap_or_default();
                if known_words.contains(first) {
                    found.push(path);
                }
            }
        }
    }
    found
}

/// Keep the leading command words, dropping the first argument-looking
/// token - flags, placeholders and file names are not part of the path.
fn command_path(span: &str) -> Option<String> {
    let words: Vec<&str> = span
        .split_whitespace()
        .take_while(|word| {
            !word.starts_with('-')
                && !word.starts_with('<')
                && !word.contains('.')
                && !word.contains('/')
                && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        })
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(words.join(" "))
}

/// Every command path the binary accepts, read out of its own `--help`.
fn derive_command_tree() -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    collect_subcommands(&[], &mut paths);
    paths
}

fn collect_subcommands(prefix: &[String], into: &mut BTreeSet<String>) {
    // Three levels is the deepest the CLI goes; the bound also stops a
    // parsing mistake from recursing forever.
    if prefix.len() >= 3 {
        return;
    }
    let out = Command::new(BIN)
        .args(prefix)
        .arg("--help")
        .output()
        .expect("the binary runs");
    let help = String::from_utf8_lossy(&out.stdout);
    for name in parse_commands_section(&help) {
        let mut child = prefix.to_vec();
        let is_help = name == "help";
        child.push(name);
        into.insert(child.join(" "));
        // `help` is a real path a message may name, but descending into
        // it just re-prints the parent.
        if !is_help {
            collect_subcommands(&child, into);
        }
    }
}

/// Names from the `Commands:` block. A name sits at exactly two spaces of
/// indentation, which is what separates it from a wrapped description.
fn parse_commands_section(help: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    for line in help.lines() {
        if line.trim_end() == "Commands:" {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if line.trim().is_empty() {
            break;
        }
        let Some(rest) = line.strip_prefix("  ") else {
            break;
        };
        if rest.starts_with(' ') {
            continue;
        }
        let name = rest.split_whitespace().next().unwrap_or_default();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            names.push(name.to_string());
        }
    }
    names
}

fn visit_rust_files(dir: &Path, each: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            visit_rust_files(&path, each);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            each(&path);
        }
    }
}
