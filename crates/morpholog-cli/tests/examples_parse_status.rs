//! Status-locking integration tests for the worked-example
//! `.morph` files. Every example parses end-to-end via
//! `morpholog parse`; this file is the steady-state smoke
//! harness that fails loudly if any example regresses.

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

fn assert_parses(rel: &str) {
    let path = repo_root().join(rel);
    let out = Command::new(bin())
        .arg("parse")
        .arg(&path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run morpholog parse {}: {e}", path.display()));
    assert!(
        out.status.success(),
        "expected full parse for {rel}; got stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn settlement_netting_parses_fully() {
    assert_parses("examples/01_settlement_netting/netting.morph");
}

#[test]
fn verified_revenue_parses_fully() {
    assert_parses("examples/02_verified_revenue/verified_revenue.morph");
}

#[test]
fn double_entry_ledger_parses_fully() {
    assert_parses("examples/03_double_entry_ledger/ledger.morph");
}

#[test]
fn approval_controls_parses_fully() {
    assert_parses("examples/04_approval_controls/approval_controls.morph");
}

#[test]
fn insurance_claim_settlement_parses_fully() {
    assert_parses("examples/05_insurance_claim_settlement/insurance_claim_settlement.morph");
}

#[test]
fn clinical_trial_enrolment_parses_fully() {
    assert_parses("examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph");
}
