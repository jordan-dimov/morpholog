//! End-to-end test of the `morpholog parse` subcommand against
//! real `.morph` fixture files. Spawns the built binary, asserts
//! on stdout/stderr/exit code.
//!
//! Distinct from the in-crate argument-parsing tests (which only
//! verify clap accepts the right shape): this test catches
//! regressions in the CLI wiring itself - file reading, JSON
//! emission, diagnostic rendering, exit codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

fn write_fixture(name: &str, content: &str) -> tempfile::TempPath {
    let mut f = tempfile::Builder::new()
        .prefix(name)
        .suffix(".morph")
        .tempfile()
        .expect("create tempfile");
    f.write_all(content.as_bytes()).expect("write fixture");
    f.into_temp_path()
}

#[test]
fn parse_happy_path_emits_json_and_exits_zero() {
    let path = write_fixture(
        "happy",
        "program demo\npredicate Foo(a: Subject)\npredicate Bar(b: Decimal)\n",
    );
    let out = Command::new(bin())
        .args(["parse", path.to_str().unwrap()])
        .output()
        .expect("run morpholog parse");
    assert!(
        out.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(parsed["name"], "demo");
    let preds = parsed["predicates"].as_array().expect("predicates array");
    assert_eq!(preds.len(), 2);
    assert_eq!(preds[0]["name"], "Foo");
    assert_eq!(preds[1]["name"], "Bar");
}

#[test]
fn parse_error_emits_diagnostic_on_stderr_and_exits_nonzero() {
    let path = write_fixture("bad", "program demo\npredicate Foo(amount: Money)\n");
    let out = Command::new(bin())
        .args(["parse", path.to_str().unwrap()])
        .output()
        .expect("run morpholog parse");
    assert!(!out.status.success(), "expected non-zero exit");
    assert_eq!(out.status.code(), Some(1), "expected exit code 1");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    // stderr should contain SOME diagnostic; the exact rendering is
    // ariadne's, but the source-file name should appear.
    assert!(
        stderr.contains(path.file_name().unwrap().to_str().unwrap()),
        "stderr should reference the source file: {stderr}"
    );
    // stdout should be empty on the failure path.
    assert!(
        out.stdout.is_empty(),
        "stdout should be empty on parse failure"
    );
}

#[test]
fn parse_missing_file_errors_via_anyhow() {
    let out = Command::new(bin())
        .args(["parse", "/nonexistent/path/to/file.morph"])
        .output()
        .expect("run morpholog parse");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("read source file") || stderr.contains("No such file"),
        "stderr should explain the read failure: {stderr}"
    );
}
