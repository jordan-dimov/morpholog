//! Status-locking integration tests for the worked-example
//! `.morph` files.
//!
//! For each file in `examples/*/*.morph`, this test runs the
//! `morpholog` binary and pins the current parse outcome. After
//! P3b2 (state-mutating statements + iteration), the situation is:
//!
//! - Four examples parse fully end-to-end.
//! - Two examples stop at `derived` (the only remaining P3c
//!   keyword that hasn't landed).
//!
//! When P3c lands, the `derived`-stopping tests get updated to
//! expect full success and this file becomes a steady-state
//! "all examples parse" smoke harness.
//!
//! The point of the test is not to assert specific stop tokens
//! forever; it is to make sure the parser doesn't *regress* a
//! formerly-passing portion of an example silently. If a future
//! change made one of these examples fail earlier than recorded
//! here, this test would fail loudly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn parse_file(rel: &str) -> (bool, String) {
    let path = repo_root().join(rel);
    let out = Command::new(bin())
        .arg("parse")
        .arg(&path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run morpholog parse {}: {e}", path.display()));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stderr)
}

// ============================================================
// Examples that parse end-to-end post-P3b2.
// ============================================================

#[test]
fn settlement_netting_parses_fully() {
    let (ok, stderr) = parse_file("examples/01_settlement_netting/netting.morph");
    assert!(ok, "expected full parse; stderr:\n{stderr}");
}

#[test]
fn verified_revenue_parses_fully() {
    let (ok, stderr) = parse_file("examples/02_verified_revenue/verified_revenue.morph");
    assert!(ok, "expected full parse; stderr:\n{stderr}");
}

#[test]
fn approval_controls_parses_fully() {
    let (ok, stderr) = parse_file("examples/04_approval_controls/approval_controls.morph");
    assert!(ok, "expected full parse; stderr:\n{stderr}");
}

#[test]
fn clinical_trial_enrolment_parses_fully() {
    let (ok, stderr) =
        parse_file("examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph");
    assert!(ok, "expected full parse; stderr:\n{stderr}");
}

// ============================================================
// Examples that still stop at `derived` (P3c territory).
// ============================================================

#[test]
fn double_entry_ledger_stops_at_derived() {
    let (ok, stderr) = parse_file("examples/03_double_entry_ledger/ledger.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    assert!(
        stderr.contains("`derived`"),
        "expected `derived` keyword to be the stopping point; got stderr:\n{stderr}"
    );
}

#[test]
fn insurance_claim_settlement_stops_at_derived() {
    let (ok, stderr) =
        parse_file("examples/05_insurance_claim_settlement/insurance_claim_settlement.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    assert!(
        stderr.contains("`derived`"),
        "expected `derived` keyword to be the stopping point; got stderr:\n{stderr}"
    );
}
