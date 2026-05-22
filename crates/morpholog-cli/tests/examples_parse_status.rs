//! Status-locking integration tests for the worked-example
//! `.morph` files.
//!
//! For each file in `examples/*/*.morph`, this test runs the
//! `morpholog` binary and pins the current parse outcome. The
//! current state of the parser arc is mid-stream: every example
//! parses through its predicates and invariants; five of six stop
//! expectedly at the first `admit` keyword (P3b2 territory); the
//! double-entry ledger stops at `derived` (P3c territory).
//!
//! When P3b2 lands, the tests for `admit`-stopping examples will
//! need to be updated to either expect a deeper failure point
//! (e.g. `for`) or full success. When P3c lands, the same goes
//! for the ledger example.
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
    // `CARGO_MANIFEST_DIR` is `crates/morpholog-cli`; the repo
    // root is two `..` up.
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

/// All six examples either parse fully or fail expectedly at a
/// known P3b2/P3c keyword. None should fail at an earlier point
/// (e.g. a predicate declaration or an invariant body).
#[test]
fn settlement_netting_stops_at_admit() {
    let (ok, stderr) = parse_file("examples/01_settlement_netting/netting.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    assert!(
        stderr.contains("`admit`"),
        "expected `admit` keyword to be the stopping point; got stderr:\n{stderr}"
    );
}

#[test]
fn verified_revenue_stops_at_admit() {
    let (ok, stderr) = parse_file("examples/02_verified_revenue/verified_revenue.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    assert!(
        stderr.contains("`admit`"),
        "expected `admit` keyword to be the stopping point; got stderr:\n{stderr}"
    );
}

#[test]
fn double_entry_ledger_stops_at_admit_or_derived() {
    let (ok, stderr) = parse_file("examples/03_double_entry_ledger/ledger.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    // The ledger uses `admit` in its transformations AND `derived`
    // at the bottom of the file. The parser's recovery walks past
    // the first failure looking for the next top-level keyword;
    // either stopping point is acceptable for this status lock.
    assert!(
        stderr.contains("`admit`") || stderr.contains("`derived`"),
        "expected stop at `admit` or `derived`; got stderr:\n{stderr}"
    );
}

#[test]
fn approval_controls_stops_at_admit() {
    let (ok, stderr) = parse_file("examples/04_approval_controls/approval_controls.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    assert!(
        stderr.contains("`admit`"),
        "expected `admit` keyword to be the stopping point; got stderr:\n{stderr}"
    );
}

#[test]
fn insurance_claim_settlement_stops_at_admit() {
    let (ok, stderr) =
        parse_file("examples/05_insurance_claim_settlement/insurance_claim_settlement.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    assert!(
        stderr.contains("`admit`"),
        "expected `admit` keyword to be the stopping point; got stderr:\n{stderr}"
    );
}

#[test]
fn clinical_trial_enrolment_stops_at_admit() {
    let (ok, stderr) =
        parse_file("examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph");
    assert!(!ok, "expected partial-parse failure; got success");
    assert!(
        stderr.contains("`admit`"),
        "expected `admit` keyword to be the stopping point; got stderr:\n{stderr}"
    );
}
