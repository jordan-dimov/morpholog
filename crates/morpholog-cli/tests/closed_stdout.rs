//! A reader that closes standard output ends the command quietly with
//! status 141, as a pipe's writer ends, so `set -o pipefail` sees the
//! truncation; any other failure to write stays an error.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use std::process::{Command, Output};

use common::{bin, repo_root};

/// `args` run with a standard output whose reader is already gone, so
/// the first write fails as `inspect audit | head` fails once `head`
/// has read enough.
fn closed(args: &[&str]) -> Output {
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    Command::new(bin())
        .args(args)
        .stdout(writer)
        .output()
        .expect("cli runs")
}

fn ledger() -> String {
    repo_root()
        .join("examples/03_double_entry_ledger/ledger.morph")
        .to_str()
        .unwrap()
        .to_string()
}

fn assert_quiet_141(output: &Output) {
    assert_eq!(output.status.code(), Some(141), "{output:?}");
    assert!(
        output.stderr.is_empty(),
        "no panic and no error text: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_json_answer_to_a_closed_reader_ends_with_141() {
    assert_quiet_141(&closed(&["schema", &ledger(), "post_simple_entry"]));
}

#[test]
fn a_summary_to_a_closed_reader_ends_with_141() {
    assert_quiet_141(&closed(&["check", "--verbose", &ledger()]));
}

#[test]
fn a_stream_to_a_closed_reader_ends_with_141() {
    let file = repo_root().join("examples/23_delivery_hour_limits/delivery_hour_limits.morph");
    let zoneinfo = repo_root().join("crates/morpholog-cli/tests/fixtures/zoneinfo");
    let output = closed(&[
        "generate",
        "local-days",
        file.to_str().unwrap(),
        "publish_delivery_day",
        "--actor",
        "cal_uk",
        "--arg",
        "calendar=uk",
        "--zone",
        "Europe/London",
        "--zoneinfo",
        zoneinfo.to_str().unwrap(),
        "--from",
        "2026-01-01",
        "--until",
        "2026-12-31",
    ]);
    assert_eq!(output.status.code(), Some(141), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("panicked"),
        "{output:?}"
    );
}

/// The reported case: a reader takes the first line of a long stream and
/// stops, so the writes before succeed and a later one fails. Ten years of
/// days are far more than a pipe holds, so the command is still writing
/// when the reader goes.
#[test]
fn a_stream_its_reader_stops_midway_ends_with_141() {
    use std::io::BufRead;
    let file = repo_root().join("examples/23_delivery_hour_limits/delivery_hour_limits.morph");
    let zoneinfo = repo_root().join("crates/morpholog-cli/tests/fixtures/zoneinfo");
    let mut child = Command::new(bin())
        .args([
            "generate",
            "local-days",
            file.to_str().unwrap(),
            "publish_delivery_day",
            "--actor",
            "cal_uk",
            "--arg",
            "calendar=uk",
            "--zone",
            "Europe/London",
            "--zoneinfo",
            zoneinfo.to_str().unwrap(),
            "--from",
            "2026-01-01",
            "--until",
            "2036-01-01",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cli runs");
    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert!(first.starts_with('{'), "the first row arrived: {first}");
    drop(reader);
    let output = child.wait_with_output().expect("cli ends");
    assert_quiet_141(&output);
}

/// A full disk is not a reader that stopped: the command fails as any
/// failed write does.
#[test]
fn a_full_disk_is_still_an_error() {
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("/dev/full");
    let output = Command::new(bin())
        .args(["schema", &ledger(), "post_simple_entry"])
        .stdout(full)
        .output()
        .expect("cli runs");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("Error:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
