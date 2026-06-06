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
fn check_verbose_clean_program_prints_summary() {
    // A self-contained fixture so the asserted counts are intrinsic
    // to the test, not coupled to a worked example that may grow.
    let tmp = temp_morph(
        "program demo\n\
         predicate Foo(x: Subject)\n\
         intent Bar(x: Subject)\n\
         invariant always: Foo(x) implies Foo(x)\n\
         transformation t(x):\n    admit Foo(x)\n    emit Bar(x)\n",
    );

    let out = Command::new(bin())
        .arg("check")
        .arg("--verbose")
        .arg(tmp.path())
        .output()
        .expect("morpholog check should run");
    assert!(
        out.status.success(),
        "expected exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = format!(
        "ok: {}\nprogram: demo\n  predicates: 1\n  invariants: 1\n  transformations: 1\n  intents: 1\n  derived claims: 0\n",
        tmp.path().display()
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        expected,
        "verbose summary shape is part of the contract"
    );
    assert!(
        out.stderr.is_empty(),
        "verbose clean check should not write to stderr; got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_verbose_on_invalid_program_prints_no_summary() {
    // The summary is a success artifact: a failing check must keep
    // stdout empty even under --verbose, so scripts piping stdout
    // never see a half-summary for a programme that did not validate.
    let tmp = temp_morph(
        "program demo\n\
         predicate Foo(x: Subject)\n\
         invariant test: UndeclaredPred(x)\n",
    );

    let out = Command::new(bin())
        .arg("check")
        .arg("-v")
        .arg(tmp.path())
        .output()
        .expect("morpholog check should run");
    assert!(!out.status.success(), "expected non-zero exit");
    assert!(
        out.stdout.is_empty(),
        "failed check must not print a summary; got:\n{}",
        String::from_utf8_lossy(&out.stdout)
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
fn check_kind_mismatch_reports_predicate_arg_kind_diagnostic() {
    // A decimal literal in a Subject slot is the canonical
    // kind-checker catch. Surfaces an ArgKindMismatch
    // diagnostic with the expected vs actual kinds named.
    let tmp = temp_morph(
        "program demo\n\
         predicate Owner(id: Subject)\n\
         invariant bad: Owner(100)\n",
    );

    let out = Command::new(bin())
        .arg("check")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Owner") && stderr.contains("Subject") && stderr.contains("Decimal"),
        "expected kind-mismatch diagnostic naming Owner/Subject/Decimal; got:\n{stderr}"
    );
}

#[test]
fn check_date_le_with_decimal_literal_reports_operand_kind_diagnostic() {
    // `on_or_before` is the date comparator; a decimal literal
    // on either side is the wrong-kind mistake. Diagnostic
    // should name the operator and the two kinds.
    let tmp = temp_morph(
        "program demo\n\
         predicate Limit(amount: Decimal)\n\
         invariant bad: 100 on_or_before 200\n",
    );

    let out = Command::new(bin())
        .arg("check")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("on_or_before") && stderr.contains("Date") && stderr.contains("Decimal"),
        "expected operand-kind diagnostic naming on_or_before/Date/Decimal; got:\n{stderr}"
    );
}

#[test]
fn developer_intro_complete_program_checks() {
    // The developer introduction embeds a complete `revenue.morph` and
    // promises every shown artefact is real. Extract that block
    // verbatim and check it, so a tutorial edit cannot silently break
    // the programme readers are told to paste.
    let doc = std::fs::read_to_string(repo_root().join("docs/developer-intro.md"))
        .expect("read developer intro");
    let section = doc
        .split("### The complete `revenue.morph`")
        .nth(1)
        .expect("complete-file section present");
    let fence = "```morph\n";
    let start = section.find(fence).expect("morph fence opens") + fence.len();
    let end = section[start..].find("```").expect("morph fence closes") + start;
    let tmp = temp_morph(&section[start..end]);

    let out = Command::new(bin())
        .arg("check")
        .arg("-v")
        .arg(tmp.path())
        .output()
        .expect("morpholog check should run");
    assert!(
        out.status.success(),
        "tutorial's complete revenue.morph failed check; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The doc tells the reader this summary reports one derived claim;
    // pin the promise, not just well-formedness.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("derived claims: 1"),
        "tutorial promises `derived claims: 1`; got:\n{stdout}"
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
        "examples/07_chess_transition_invariants/chess.morph",
        "examples/08_kyc_sanctions_screening/kyc.morph",
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
