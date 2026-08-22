//! End-to-end test of `morpholog check --ir` (the IR debugging view)
//! against real `.morph` fixture files. Spawns the built binary, asserts
//! on stdout/stderr/exit code.
//!
//! Distinct from the in-crate argument-parsing tests (which only
//! verify clap accepts the right shape): this test catches
//! regressions in the CLI wiring itself - file reading, JSON
//! emission, diagnostic rendering, exit codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::bin;

use std::process::Command;

#[test]
fn check_ir_happy_path_emits_json_and_exits_zero() {
    let path = common::write_fixture(
        "happy",
        "program demo\npredicate Foo(a: Subject)\npredicate Bar(b: Decimal)\n",
    );
    let out = Command::new(bin())
        .args(["check", path.to_str().unwrap(), "--ir"])
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
fn check_ir_projects_invariants_transformations_and_derived_claims() {
    // The happy-path test above exercises only the predicate
    // projection; the bulk of the parse command is the three rendered
    // projections (invariant bodies, transformation bodies, derived
    // claims), which need a programme that actually has them.
    let path = common::write_fixture(
        "rich",
        "program rich\n\
         predicate Balance(acct: Subject, amount: Decimal)\n\
         predicate Row(acct: Subject, total: Decimal)\n\
         intent Posted(acct: Subject)\n\
         invariant non_negative: Balance(a, x) implies 0 <= x\n\
         transformation post(acct, amount):\n    \
             require not Balance(acct, _)\n    \
             admit Balance(acct, amount)\n    \
             emit Posted(acct)\n\
         derived Row(acct):\n    \
             over Balance(acct, _)\n    \
             value total = sum(x | Balance(acct, x))\n",
    );
    let out = Command::new(bin())
        .args(["check", path.to_str().unwrap(), "--ir"])
        .output()
        .expect("run morpholog parse");
    assert!(
        out.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).expect("stdout is JSON");

    let inv = &parsed["invariants"][0];
    assert_eq!(inv["name"], "non_negative");
    assert_eq!(inv["version"], 1);
    assert!(
        inv["body"].as_str().unwrap().contains("implies"),
        "invariant body renders inline: {inv}"
    );

    let t = &parsed["transformations"][0];
    assert_eq!(t["name"], "post");
    assert_eq!(t["parameters"][0], "acct");
    let body: Vec<&str> = t["body"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(
        body.iter().any(|l| l.contains("require")) && body.iter().any(|l| l.contains("emit")),
        "transformation body renders statement lines: {body:?}"
    );

    let d = &parsed["derived_claims"][0];
    assert_eq!(d["predicate"], "Row");
    assert_eq!(d["keys"][0], "acct");
    assert_eq!(d["values"][0]["name"], "total");
    assert!(
        d["values"][0]["expr"].as_str().unwrap().contains("sum"),
        "derived value expression renders inline: {d}"
    );
}

#[test]
fn check_ir_parse_error_emits_diagnostic_on_stderr_and_exits_nonzero() {
    let path = common::write_fixture("bad", "program demo\npredicate Foo(amount: Money)\n");
    let out = Command::new(bin())
        .args(["check", path.to_str().unwrap(), "--ir"])
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
fn check_ir_missing_file_errors_via_anyhow() {
    let out = Command::new(bin())
        .args(["check", "/nonexistent/path/to/file.morph", "--ir"])
        .output()
        .expect("run morpholog parse");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("read source file") || stderr.contains("No such file"),
        "stderr should explain the read failure: {stderr}"
    );
}

#[test]
fn check_ir_renders_a_non_first_hole_lookup_in_its_named_form() {
    // The carrier example's asset register reads a figure past an
    // elided coordinate. Positional text would reparse with the wrong
    // hole, so the IR view must emit the named spelling - the same
    // faithfulness rule the formatter and the canonical hash follow.
    let path = common::repo_root().join("examples/11_borrowing_base/borrowing_base.morph");
    let out = Command::new(bin())
        .args(["check", path.to_str().unwrap(), "--ir"])
        .output()
        .expect("run morpholog check --ir");
    assert!(
        out.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).expect("stdout is JSON");
    let derived = parsed["derived_claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["predicate"] == "AssetValue")
        .expect("the asset register is in the IR view");
    let expr = derived["values"][0]["expr"].as_str().unwrap();
    assert_eq!(
        expr, "value EligibleCollateral(asset: asset, collateral_value: _, ..)",
        "the named spelling is the only faithful rendering"
    );
}
