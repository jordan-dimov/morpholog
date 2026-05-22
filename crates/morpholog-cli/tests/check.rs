//! Integration tests for the `morpholog check` subcommand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;
use tempfile::NamedTempFile;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

/// Write `source` to a uniquely-named temp file and return the
/// handle. The file is auto-deleted when the handle drops; tests
/// keep it alive for the duration of the `morpholog` subprocess.
/// Tempfile-per-test avoids cross-test collisions under parallel
/// or repeated local runs.
fn temp_morph(source: &str) -> NamedTempFile {
    let f = NamedTempFile::new().expect("create temp .morph file");
    std::fs::write(f.path(), source).expect("write temp .morph file");
    f
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
    // The contract for `check` on a clean programme is that it is
    // silent on both streams. Asserting both keeps accidental
    // warnings or stdout writes from sneaking in unnoticed.
    assert!(
        out.stdout.is_empty(),
        "clean check should be silent on stdout; got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "clean check should be silent on stderr; got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_undeclared_predicate_reports_validation_error() {
    let tmp = temp_morph(
        "program demo\n\
         predicate Foo(x: Subject)\n\
         invariant test: UndeclaredPred(x)\n",
    );

    let out = Command::new(bin())
        .arg("check")
        .arg(tmp.path())
        .output()
        .expect("morpholog check should run");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("undeclared predicate") && stderr.contains("UndeclaredPred"),
        "expected validation diagnostic; got:\n{stderr}"
    );
}

#[test]
fn check_arity_mismatch_reports_validation_error() {
    let tmp = temp_morph(
        "program demo\n\
         predicate Foo(x: Subject, y: Subject)\n\
         invariant test: Foo(x)\n",
    );

    let out = Command::new(bin())
        .arg("check")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("declared with arity 2") && stderr.contains("Foo"),
        "expected arity-mismatch diagnostic; got:\n{stderr}"
    );
}

#[test]
fn check_parse_failure_renders_ariadne_diagnostic() {
    let tmp = temp_morph("predicate Foo(x: Subject)\n");

    let out = Command::new(bin())
        .arg("check")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("program") || stderr.contains("Error"),
        "expected parse-error rendering; got:\n{stderr}"
    );
    // Diagnostics go to stderr; stdout must stay empty so that
    // scripts piping `check`'s stdout don't get diagnostic text
    // mixed into their data stream.
    assert!(
        out.stdout.is_empty(),
        "parse failure should not write to stdout; got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
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
