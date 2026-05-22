//! Integration tests for the `morpholog check` subcommand.

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

#[test]
fn check_clean_program_exits_zero_with_no_output() {
    let out = Command::new(bin())
        .arg("check")
        .arg(repo_root().join("examples/01_settlement_netting/netting.morph"))
        .output()
        .expect("morpholog check should run");
    assert!(
        out.status.success(),
        "expected exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "clean check should be silent; got stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn check_undeclared_predicate_reports_validation_error() {
    let tmp = std::env::temp_dir().join("morpholog_check_test_undeclared.morph");
    std::fs::write(
        &tmp,
        "program demo\n\
         predicate Foo(x: Subject)\n\
         invariant test: UndeclaredPred(x)\n",
    )
    .unwrap();

    let out = Command::new(bin())
        .arg("check")
        .arg(&tmp)
        .output()
        .expect("morpholog check should run");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("undeclared predicate") && stderr.contains("UndeclaredPred"),
        "expected validation diagnostic; got:\n{stderr}"
    );

    std::fs::remove_file(&tmp).ok();
}

#[test]
fn check_arity_mismatch_reports_validation_error() {
    let tmp = std::env::temp_dir().join("morpholog_check_test_arity.morph");
    std::fs::write(
        &tmp,
        "program demo\n\
         predicate Foo(x: Subject, y: Subject)\n\
         invariant test: Foo(x)\n",
    )
    .unwrap();

    let out = Command::new(bin()).arg("check").arg(&tmp).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("declared with arity 2") && stderr.contains("Foo"),
        "expected arity-mismatch diagnostic; got:\n{stderr}"
    );

    std::fs::remove_file(&tmp).ok();
}

#[test]
fn check_parse_failure_renders_ariadne_diagnostic() {
    let tmp = std::env::temp_dir().join("morpholog_check_test_parse.morph");
    std::fs::write(&tmp, "predicate Foo(x: Subject)\n").unwrap();

    let out = Command::new(bin()).arg("check").arg(&tmp).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("program") || stderr.contains("Error"),
        "expected parse-error rendering; got:\n{stderr}"
    );

    std::fs::remove_file(&tmp).ok();
}

#[test]
fn check_all_worked_examples_are_well_formed() {
    // Every worked example .morph must parse and validate cleanly.
    // Any future change that breaks one would fail here loudly.
    for rel in [
        "examples/01_settlement_netting/netting.morph",
        "examples/02_verified_revenue/verified_revenue.morph",
        "examples/03_double_entry_ledger/ledger.morph",
        "examples/04_approval_controls/approval_controls.morph",
        "examples/05_insurance_claim_settlement/insurance_claim_settlement.morph",
        "examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph",
    ] {
        let path = repo_root().join(rel);
        let out = Command::new(bin())
            .arg("check")
            .arg(&path)
            .output()
            .expect("morpholog check should run");
        assert!(
            out.status.success(),
            "{rel} failed check; stderr:\n{}",
            String::from_utf8_lossy(&out.stderr),
        );
    }
}
