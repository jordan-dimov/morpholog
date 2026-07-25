//! Integration tests for the `morpholog check` subcommand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{bin, repo_root};

use std::process::Command;
use tempfile::NamedTempFile;

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

/// Drop ANSI CSI sequences (`ESC [` parameters, closed by a final
/// byte in `@`..=`~`) so assertions can read the rendered diagnostic
/// as plain text (ariadne colours the quoted source line per
/// character). Restricted to CSI rather than skip-to-`m` so a
/// non-SGR escape can never swallow unrelated output; a lone ESC is
/// dropped.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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
        "ok: {}\nprogram: demo\n  predicates: 1\n  definitions: 0\n  invariants: 1\n  transformations: 1\n  intents: 1\n  derived claims: 0\n",
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
    // Every worked example .morph must parse and validate cleanly -
    // discovered by walking examples/, so a new example is covered the
    // day it lands (a hardcoded list here once silently stopped at 12).
    let mut checked = 0usize;
    let mut example_dirs = 0usize;
    for entry in std::fs::read_dir(repo_root().join("examples")).expect("examples dir") {
        let dir = entry.expect("dir entry").path();
        if !dir.is_dir() {
            continue;
        }
        // Only the numbered gallery dirs are examples with a .morph;
        // the worked embedder's dir carries a Python package instead.
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
        {
            example_dirs += 1;
        }
        for file in std::fs::read_dir(&dir).expect("example dir") {
            let path = file.expect("file entry").path();
            if path.extension().is_some_and(|e| e == "morph") {
                let out = Command::new(bin())
                    .arg("check")
                    .arg(&path)
                    .output()
                    .expect("morpholog check should run");
                assert!(
                    out.status.success(),
                    "{} failed check; stderr:\n{}",
                    path.display(),
                    String::from_utf8_lossy(&out.stderr),
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked >= example_dirs && example_dirs > 0,
        "every example dir carries a checked .morph ({checked} checked, {example_dirs} dirs)"
    );
}

const LINT_TRIP: &str = r#"
program trip

predicate Decision(decision_id: Subject, doc: Subject)
    append only
predicate CurrentMandate(doc: Subject, mandate_id: Subject)
    current pointer by (doc)

invariant decisions_need_live_mandate:
    Decision(d, doc) implies CurrentMandate(doc, _)
"#;

// A lint finding is advisory by default: hint on stderr, exit 0, and
// stdout stays silent so the empty-stdout script contract holds. The
// hint carets the invariant it concerns - LINT_TRIP's sits at 9:1.
#[test]
fn check_prints_a_located_hint_and_passes_without_strict() {
    let f = temp_morph(LINT_TRIP);
    let out = Command::new(bin())
        .arg("check")
        .arg(f.path())
        .output()
        .expect("spawn morpholog");
    assert!(out.status.success(), "lints alone must not fail the check");
    assert!(out.stdout.is_empty(), "stdout stays silent");
    let stderr = strip_ansi(&String::from_utf8(out.stderr).unwrap());
    assert!(
        stderr.contains("hint:") && stderr.contains("decisions_need_live_mandate"),
        "got: {stderr}"
    );
    assert!(
        stderr.contains(":9:1") && stderr.contains("invariant decisions_need_live_mandate:"),
        "the hint carets the invariant's source line; got: {stderr}"
    );
}

// --strict promotes the same finding to an error and a failing exit.
#[test]
fn check_strict_promotes_the_hint_to_an_error() {
    let f = temp_morph(LINT_TRIP);
    let out = Command::new(bin())
        .arg("check")
        .arg(f.path())
        .arg("--strict")
        .output()
        .expect("spawn morpholog");
    assert!(!out.status.success(), "--strict fails on a finding");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("error:") && stderr.contains("belongs in the admitting"),
        "got: {stderr}"
    );
}

// A validation error carets the declaration that contains it.
#[test]
fn check_validation_error_carets_the_declaration() {
    let tmp = temp_morph(
        "program demo\n\
         predicate Foo(x: Subject)\n\
         invariant test: UndeclaredPred(x)\n",
    );
    let out = Command::new(bin())
        .arg("check")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = strip_ansi(&String::from_utf8(out.stderr).unwrap());
    assert!(
        stderr.contains(":3:1") && stderr.contains("invariant test: UndeclaredPred(x)"),
        "the error carets the invariant's source line; got: {stderr}"
    );
}

/// Run `check --json` and parse the stdout object.
fn check_json(path: &std::path::Path, strict: bool) -> (serde_json::Value, bool) {
    let mut cmd = Command::new(bin());
    cmd.arg("check").arg("--json").arg(path);
    if strict {
        cmd.arg("--strict");
    }
    let out = cmd.output().expect("spawn morpholog");
    let payload: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    (payload, out.status.success())
}

#[test]
fn check_json_clean_program_reports_no_diagnostics() {
    let (payload, ok) = check_json(
        &repo_root().join("examples/01_settlement_netting/netting.morph"),
        false,
    );
    assert!(ok);
    assert_eq!(payload["diagnostics"], serde_json::json!([]));
    assert!(
        payload["file"].as_str().unwrap().ends_with("netting.morph"),
        "got: {payload}"
    );
}

#[test]
fn check_json_validation_error_carries_line_and_column() {
    let tmp = temp_morph(
        "program demo\n\
         predicate Foo(x: Subject)\n\
         invariant test: UndeclaredPred(x)\n",
    );
    let (payload, ok) = check_json(tmp.path(), false);
    assert!(!ok, "validation failure exits non-zero under --json too");
    let d = &payload["diagnostics"][0];
    assert_eq!(d["severity"], "error");
    assert_eq!(d["line"], 3);
    assert_eq!(d["column"], 1);
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .contains("undeclared predicate"),
        "got: {payload}"
    );
    let (start, end) = (
        d["start"].as_u64().unwrap() as usize,
        d["end"].as_u64().unwrap() as usize,
    );
    assert!(start < end, "byte span is well-formed: {payload}");
}

#[test]
fn check_json_lint_is_a_hint_and_strict_promotes_it() {
    let f = temp_morph(LINT_TRIP);
    let (payload, ok) = check_json(f.path(), false);
    assert!(ok, "a hint alone passes");
    let d = &payload["diagnostics"][0];
    assert_eq!(d["severity"], "hint");
    assert_eq!(d["line"], 9);

    let (payload, ok) = check_json(f.path(), true);
    assert!(!ok, "--strict fails on the same finding");
    assert_eq!(payload["diagnostics"][0]["severity"], "error");
}

#[test]
fn check_json_parse_error_is_reported_in_band() {
    let tmp = temp_morph("predicate Foo(x: Subject)\n");
    let (payload, ok) = check_json(tmp.path(), false);
    assert!(!ok);
    let diags = payload["diagnostics"].as_array().unwrap();
    assert!(!diags.is_empty(), "parse errors appear in the JSON");
    assert_eq!(diags[0]["severity"], "error");
}

// A lint-clean programme is identical under --strict.
#[test]
fn check_strict_on_a_clean_program_exits_zero() {
    let out = Command::new(bin())
        .arg("check")
        .arg(repo_root().join("examples/02_verified_revenue/verified_revenue.morph"))
        .arg("--strict")
        .output()
        .expect("spawn morpholog");
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}
