//! Integration tests for the `morpholog check` subcommand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{bin, repo_root};

use std::process::Command;
use tempfile::NamedTempFile;

/// Write `source` to a uniquely named temp file, deleted when the handle
/// drops. Keep the handle alive while the subprocess runs.
fn temp_morph(source: &str) -> NamedTempFile {
    let f = NamedTempFile::new().expect("create temp .morph file");
    std::fs::write(f.path(), source).expect("write temp .morph file");
    f
}

/// Drop ANSI CSI sequences (`ESC [`, ended by a byte in `@`..=`~`) so
/// assertions read the diagnostic as plain text; ariadne colours each
/// character. Only CSI, so another escape cannot swallow real output. A
/// lone ESC is dropped.
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
    // A clean programme is silent on both streams.
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
    // Its own fixture, so the counts do not change when an example grows.
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
        "ok: {}\nprogram: demo\n  predicates: 1\n  definitions: 0\n  invariants: 1\n  transformations: 1\n  intents: 1\n  derived claims: 0\n  invariant checks: compiled\n",
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

/// A programme with an invariant that cannot compile to SQL runs
/// interpreted, and the summary names that invariant and why.
#[test]
fn check_verbose_names_the_invariant_that_keeps_a_programme_interpreted() {
    let tmp = temp_morph(
        "program demo\n\
         predicate Foo(x: Subject)\n\
         invariant sticky: pre(Foo(x)) implies Foo(x)\n\
         transformation t(x):\n    admit Foo(x)\n",
    );
    let out = Command::new(bin())
        .arg("check")
        .arg("--verbose")
        .arg(tmp.path())
        .output()
        .expect("morpholog check should run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("  invariant checks: interpreted\n    sticky: "),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("outside the compiled fragment"),
        "got: {stdout}"
    );
}

#[test]
fn check_verbose_on_invalid_program_prints_no_summary() {
    // The summary is for success only: a failing check keeps stdout empty
    // even under --verbose.
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
    // Diagnostics go to stderr; stdout stays empty for scripts.
    assert!(
        out.stdout.is_empty(),
        "parse failure should not write to stdout; got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn check_kind_mismatch_reports_predicate_arg_kind_diagnostic() {
    // A decimal literal in a Subject slot: the diagnostic names the
    // expected and actual kinds.
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
    // A decimal on either side of the date comparator `on_or_before`: the
    // diagnostic names the operator and both kinds.
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
fn check_le_over_date_variables_refuses_and_names_the_date_comparator() {
    // `<=` does not order dates. Refuse it at check time, not at
    // evaluation, and name the comparator that does.
    let tmp = temp_morph(
        "program demo\n\
         predicate Window(w: Subject, opens_on: Date)\n\
         transformation act(w, asked_on):\n\
         \x20   bind Window(w, opens_on)\n\
         \x20   require asked_on <= opens_on\n\
         \x20   admit Window(w, asked_on)\n",
    );

    let out = Command::new(bin())
        .arg("check")
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("<=") && stderr.contains("Date") && stderr.contains("use `on_or_before`"),
        "expected the decimal comparator to refuse Date operands and suggest on_or_before; got:\n{stderr}"
    );
}

#[test]
fn developer_intro_complete_program_checks() {
    // The developer introduction embeds a complete `revenue.morph` for
    // readers to paste. Check that block as-is, so a doc edit cannot
    // break it silently.
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
    // The doc says the summary reports one derived claim; pin that too.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("derived claims: 1"),
        "tutorial promises `derived claims: 1`; got:\n{stdout}"
    );
}

#[test]
fn check_all_worked_examples_are_well_formed() {
    // Every worked example must parse and validate. The list is the
    // generated registry behind `all_programs()`, so a new example is
    // covered as soon as the build finds it.
    let examples = morpholog_examples::all_examples();
    assert!(!examples.is_empty(), "the registry discovered no examples");
    for example in examples {
        let path = repo_root().join("examples").join(example.rel_path);
        let out = Command::new(bin())
            .arg("check")
            .arg(&path)
            .output()
            .expect("morpholog check should run");
        assert!(
            out.status.success(),
            "{} ({}) failed check; stderr:\n{}",
            example.name,
            path.display(),
            String::from_utf8_lossy(&out.stderr),
        );
    }
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

// A lint is advisory by default: a hint on stderr, exit 0, silent stdout.
// The hint points at its invariant, here at 9:1.
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

/// A reserved actor-policy name declared in a shape the runtime cannot
/// match is an error, not a hint: the restriction would silently never
/// apply, and the programme would look protected while protecting nothing.
const MISSHAPEN_POLICY: &str = "program p
predicate ActorAssertionRestricted(actor: Subject, note: Decimal)
predicate Thing(id: Subject)
transformation add(id):
    admit Thing(id)
";

#[test]
fn a_misshapen_actor_policy_declaration_is_refused_by_check() {
    let f = temp_morph(MISSHAPEN_POLICY);
    let out = Command::new(bin())
        .args(["check", f.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success(), "check must refuse it");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ActorAssertionRestricted") && stderr.contains("reserved"),
        "the refusal must name the predicate and why: {stderr}"
    );
}

#[test]
fn a_misshapen_actor_policy_declaration_is_refused_by_check_json() {
    // `check --json` is a separate path and must refuse too.
    let f = temp_morph(MISSHAPEN_POLICY);
    let out = Command::new(bin())
        .args(["check", "--json", f.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success(), "check --json must refuse it");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let found = report["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .any(|d| {
            d["severity"] == "error"
                && d["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("ActorAssertionRestricted"))
        });
    assert!(found, "expected an error diagnostic naming it: {report}");
}

#[test]
fn a_well_shaped_actor_policy_declaration_passes() {
    // The acceptance side: the contract still admits what it promises.
    let f = temp_morph(
        "program p
predicate ActorAssertionRestricted(actor: Subject)
predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)
predicate Thing(id: Subject)
transformation add(id):
    admit Thing(id)
",
    );
    let out = Command::new(bin())
        .args(["check", f.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the recognised shape must pass: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------
// `--against`: shared writers across programmes.
// ---------------------------------------------------------------

const SECURE_MORPH: &str = r#"program secure
predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)
predicate KeyAdmin(person: Subject)
transformation register(key_id, purpose, public_key):
    require KeyAdmin(actor)
    admit AuditSigningKey(key_id, purpose, public_key)
"#;

const ROGUE_MORPH: &str = r#"program rogue
predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)
transformation bring_own_key(key_id, purpose, public_key):
    admit AuditSigningKey(key_id, purpose, public_key)
"#;

const READER_MORPH: &str = r#"program reader
predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)
predicate Seen(key_id: Subject)
transformation note(key_id):
    require AuditSigningKey(key_id, _, _)
    admit Seen(key_id)
"#;

fn check_against(
    file: &std::path::Path,
    against: &[&std::path::Path],
    extra: &[&str],
) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.arg("check").arg(file);
    for a in against {
        cmd.arg("--against").arg(a);
    }
    for e in extra {
        cmd.arg(e);
    }
    cmd.output().expect("spawn morpholog")
}

#[test]
fn against_reports_a_shared_writer_as_a_hint_on_the_local_transformation() {
    let secure = temp_morph(SECURE_MORPH);
    let rogue = temp_morph(ROGUE_MORPH);
    let out = check_against(secure.path(), &[rogue.path()], &[]);
    assert!(
        out.status.success(),
        "a hint keeps exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty(), "stdout stays script-silent");
    let stderr = strip_ansi(&String::from_utf8(out.stderr).unwrap());
    assert!(
        stderr.contains("hint:")
            && stderr.contains(&format!("against {}", rogue.path().display()))
            && stderr.contains("`bring_own_key`: ungated")
            && stderr.contains("`register` here also writes it and has an admission gate")
            && stderr.contains(":4:1"),
        "the finding names the other file and its ungated writer, anchored on `register`: {stderr}"
    );

    let out = check_against(secure.path(), &[rogue.path()], &["--strict"]);
    assert!(!out.status.success(), "--strict promotes the finding");
    let stderr = strip_ansi(&String::from_utf8(out.stderr).unwrap());
    assert!(stderr.contains("error:"), "{stderr}");
}

#[test]
fn against_json_carries_the_finding_with_the_local_span_and_no_foreign_spans() {
    let secure = temp_morph(SECURE_MORPH);
    let rogue = temp_morph(ROGUE_MORPH);
    let out = check_against(secure.path(), &[rogue.path()], &["--json"]);
    assert!(out.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let diagnostics = payload["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{payload}");
    assert_eq!(diagnostics[0]["severity"], "hint");
    assert_eq!(
        diagnostics[0]["line"], 4,
        "anchored on `register`: {payload}"
    );
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("against {}: ", rogue.path().display())),
        "{payload}"
    );

    // A broken --against file is an error naming the path, with no span:
    // the report has one `file`, and a span into another would mislead.
    let broken = temp_morph("program broken\npredicate P(x: Subject)\ninvariant t: Nope(x)\n");
    let out = check_against(secure.path(), &[broken.path()], &["--json"]);
    assert!(!out.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let d = &payload["diagnostics"].as_array().unwrap()[0];
    assert_eq!(d["severity"], "error");
    assert!(
        d.get("line").is_none() && d.get("start").is_none(),
        "{payload}"
    );
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("against {}: ", broken.path().display())),
        "{payload}"
    );
}

#[test]
fn against_a_broken_file_carets_that_file_in_plain_mode() {
    let secure = temp_morph(SECURE_MORPH);
    let broken = temp_morph("program broken\npredicate P(x: Subject)\ninvariant t: Nope(x)\n");
    let out = check_against(secure.path(), &[broken.path()], &[]);
    assert!(!out.status.success());
    let stderr = strip_ansi(&String::from_utf8(out.stderr).unwrap());
    let broken_path = broken.path().display().to_string();
    assert!(
        stderr.contains(&format!("against {broken_path}"))
            && stderr.contains(&format!("{broken_path}:3:1"))
            && stderr.contains("invariant t: Nope(x)"),
        "the other file's own source and caret, named as the against file: {stderr}"
    );
}

#[test]
fn against_a_reader_is_silent_and_against_itself_is_refused() {
    let secure = temp_morph(SECURE_MORPH);
    let reader = temp_morph(READER_MORPH);
    let out = check_against(secure.path(), &[reader.path()], &["--strict"]);
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "a reader is not a collision"
    );

    let out = check_against(secure.path(), &[secure.path()], &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("cannot collide with itself"), "{stderr}");

    // Under --json the refusal is still one object on stdout.
    let out = check_against(secure.path(), &[secure.path()], &["--json"]);
    assert!(!out.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let d = &payload["diagnostics"].as_array().unwrap()[0];
    assert_eq!(d["severity"], "error");
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .contains("cannot collide with itself")
            && d.get("line").is_none(),
        "{payload}"
    );
}
