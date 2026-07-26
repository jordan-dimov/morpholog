//! No message may tell a reader to run a command that no longer exists.
//!
//! The `audit` rename swept the CLI crate and missed a runtime error in
//! another crate that said "run `checkpoint` first". A user who followed
//! it got clap's exit 2, and smoking the published release is what found
//! it - the rename's own tests all passed. So the retired spellings are
//! pinned here: one half proves the CLI really rejects them, the other
//! proves no source string still recommends one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_morpholog");

/// Command paths the `audit` rename retired. Each must be rejected by
/// the CLI, and none may appear in a message.
const RETIRED_PATHS: &[&[&str]] = &[
    &["evidence", "export"],
    &["evidence", "verify"],
    &["checkpoint"],
    &["keygen"],
    &["verify"],
];

/// The retired spellings as prose writes them. Multi-word forms are
/// unambiguous; the bare names are matched only in the "run `x`" shape,
/// because `checkpoint` on its own is also an ordinary noun here.
const RETIRED_IN_PROSE: &[&str] = &[
    "`evidence export`",
    "`evidence verify`",
    "run `checkpoint`",
    "run `verify`",
    "run `keygen`",
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
fn no_source_message_recommends_a_retired_command() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();
    visit_rust_files(&root.join("crates"), &mut |path| {
        // This file names the retired spellings on purpose.
        if path.ends_with("retired_commands.rs") {
            return;
        }
        let text = std::fs::read_to_string(path).expect("source readable");
        for (number, line) in text.lines().enumerate() {
            for retired in RETIRED_IN_PROSE {
                if line.contains(retired) {
                    offenders.push(format!("{}:{} names {retired}", path.display(), number + 1));
                }
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "these messages recommend a command the CLI rejects:\n{}",
        offenders.join("\n")
    );
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
