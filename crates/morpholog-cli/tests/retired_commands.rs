//! No message may tell a reader to run a command that does not exist.
//!
//! The valid commands are read from the binary's own `--help` tree rather
//! than from a list of retired spellings. A list can never cover every way
//! prose might name a dead command; the command tree is closed.
//!
//! Spotting references is still a heuristic: it knows the two shapes prose
//! uses, and an unexpected shape can slip through.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_morpholog");

/// Retired command paths. They must stay rejected, so a re-added alias
/// fails here.
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
        // Exit 2 is clap refusing the path. 127 would only mean the binary
        // was not found.
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
    // A broken derivation must not pass as "nothing to check": these paths
    // certainly exist.
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
    // Keep the text before the opening backtick, without the backtick,
    // so the "run" shape can match.
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
            // "run x" names a command only when x is a word the CLI uses;
            // otherwise "run a closure" would count.
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
    // Three levels is the CLI's depth; the bound also stops runaway
    // recursion on a parsing mistake.
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
