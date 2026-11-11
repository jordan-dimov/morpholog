//! A value of another kind than its field declares, at the binary.
//! Attacker capability: a caller who sends tagged arguments (`--args`, a
//! batch row's `args`, a session request) under a programme it cannot
//! change. Every path answers with the same coded error, `kernel_error`
//! with the kind `claim_kind_mismatch`, and writes nothing. A ledger
//! written under one declaration, rehearsed under a declaration it no
//! longer fits, shows each row as `would_error` with that kind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const AS_SUBJECT: &str = "\
program billing

predicate Manifest(id: Subject, source_name: Subject)
predicate Line(id: Subject, qty: Decimal, rate: Decimal, amount: Decimal)

transformation add(id, source_name):
    admit Manifest(id, source_name)

transformation price(id, qty, rate):
    let amount = qty * rate
    admit Line(id, qty, rate, amount)
";

const AS_DATE: &str = "\
program billing

predicate Manifest(id: Subject, source_name: Date)
predicate Line(id: Subject, qty: Decimal, rate: Decimal, amount: Decimal)

transformation add(id, source_name):
    admit Manifest(id, source_name)

transformation price(id, qty, rate):
    let amount = qty * rate
    admit Line(id, qty, rate, amount)
";

const SUBJECT_ARGS: &str = r#"[{"type":"subject","value":"m1"},{"type":"subject","value":"s1"}]"#;

fn bin(args: &[&str], stdin: Option<&str>) -> (i32, String) {
    let mut child = Command::new(common::bin())
        .args(args)
        .args(["--database-url", &database_url()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).unwrap(),
    )
}

/// The binary with no database: for the commands that take none.
fn plain(args: &[&str]) -> (i32, String) {
    let output = Command::new(common::bin()).args(args).output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// The `nth` JSON value on stdout, printed pretty or one to a line.
fn json_line(text: &str, nth: usize) -> Value {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .nth(nth)
        .unwrap_or_else(|| panic!("no value {nth} in {text}"))
        .unwrap()
}

fn claims_written() -> usize {
    let (code, out) = bin(&["inspect", "claims"], None);
    assert_eq!(code, 0, "{out}");
    serde_json::from_str::<Vec<Value>>(&out).unwrap().len()
}

fn assert_misfit(what: &str, error: &Value) {
    assert_eq!(error["code"], "kernel_error", "{what}: {error}");
    assert_eq!(error["kind"], "claim_kind_mismatch", "{what}: {error}");
}

#[tokio::test(flavor = "current_thread")]
async fn every_path_answers_a_misfit_with_the_same_coded_error() {
    reset_db().await;
    let file = common::write_fixture("as_date", AS_DATE);
    let path = file.to_str().unwrap();
    let propose = [
        "propose",
        path,
        "add",
        "--actor",
        "desk",
        "--args",
        SUBJECT_ARGS,
    ];

    let (code, out) = bin(&propose, None);
    assert_eq!(code, 1, "{out}");
    assert_misfit("one-shot propose", &json_line(&out, 0));

    let mut traced = propose.to_vec();
    traced.push("--trace");
    let (code, out) = bin(&traced, None);
    assert_eq!(code, 1, "{out}");
    let envelope: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(envelope["result"]["status"], "errored", "{envelope}");
    assert_misfit("traced propose", &envelope["result"]);

    let row = json!({"transformation": "add", "actor": "desk", "args": serde_json::from_str::<Value>(SUBJECT_ARGS).unwrap()});
    let (_, out) = bin(
        &["propose", path, "--batch", "-"],
        Some(&format!("{row}\n")),
    );
    let receipt = json_line(&out, 0);
    assert_eq!(receipt["status"], "error", "{receipt}");
    assert_misfit("batch row", &receipt);

    let (code, out) = bin(
        &["transact", path, "--acts", "-"],
        Some(&format!("{row}\n")),
    );
    assert_eq!(code, 1, "{out}");
    assert_misfit("transact", &json_line(&out, 0));

    let mut request = row.clone();
    request["op"] = json!("propose");
    let (_, out) = bin(&["session", path], Some(&format!("{request}\n")));
    let receipt = json_line(&out, 1);
    assert_eq!(receipt["status"], "error", "{receipt}");
    assert_misfit("session", &receipt);

    assert_eq!(claims_written(), 0, "a misfit wrote a claim");

    // Another kernel error keeps its own kind; a refusal or a commit has
    // none.
    let (code, out) = bin(
        &[
            "propose",
            path,
            "price",
            "--actor",
            "desk",
            "--args-named",
            r#"{"id":"l1","qty":"7922816251426433759354395033","rate":"1.23456"}"#,
        ],
        None,
    );
    assert_eq!(code, 1, "{out}");
    let error = json_line(&out, 0);
    assert_eq!(error["kind"], "arith_out_of_range", "{error}");
    assert!(
        error["error"].as_str().unwrap().contains(" * 1.23456 "),
        "the operator as written: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_ledger_rehearsed_under_a_declaration_it_does_not_fit_errors_row_by_row() {
    reset_db().await;
    let before = common::write_fixture("as_subject", AS_SUBJECT);
    let after = common::write_fixture("as_date", AS_DATE);
    for i in 1..=3 {
        let args = format!(r#"{{"id":"m{i}","source_name":"s{i}"}}"#);
        let (code, out) = bin(
            &[
                "propose",
                before.to_str().unwrap(),
                "add",
                "--actor",
                "desk",
                "--args-named",
                &args,
            ],
            None,
        );
        assert_eq!(code, 0, "{out}");
    }
    let report = rehearse(&after);
    assert_eq!(report["totals"]["would_error"], 3, "{report}");
    assert_eq!(report["totals"]["same"], 0, "{report}");
    for row in report["differences"].as_array().unwrap() {
        assert_eq!(row["class"], "would_error", "{row}");
        assert_eq!(row["kind"], "claim_kind_mismatch", "{row}");
    }
    // Admission checks what a proposal introduces; the stored claims are
    // the fit check's question, and it still refuses them.
    let (code, out) = bin(&["check", after.to_str().unwrap(), "--json"], None);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("declared Date, but 3 stored claims hold Subject"),
        "{out}"
    );
}

fn rehearse(file: &Path) -> Value {
    let (code, out) = bin(&["rehearse", file.to_str().unwrap()], None);
    assert_eq!(code, 0, "{out}");
    serde_json::from_str(&out).unwrap()
}

const DECLARED_FLAG: &str = "\
program closing
predicate Order(id: Subject)
predicate Closed(id: Subject)
transformation open(id):
    admit Order(id)
transformation close(id, force: Bool):
    require known: Order(id)
    require force = true or Closed(id)
    admit Closed(id)
";

/// A flag tested only by equality has a kind because its parameter
/// declares one: the schema types it, the generated client takes the
/// programme, the named codec reads it, and an argument of another kind
/// is the same coded error on the binary, writing nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_declared_parameter_kind_is_the_contract_at_the_binary() {
    reset_db().await;
    let file = common::write_fixture("declared_flag", DECLARED_FLAG);
    let path = file.to_str().unwrap();

    let (code, out) = plain(&["schema", path, "close"]);
    assert_eq!(code, 0, "{out}");
    let schema: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        schema["properties"]["force"]["type"], "boolean",
        "the declared kind types the flag: {schema}"
    );

    let out_dir = file.parent().unwrap().join("declared_flag_client");
    let (code, out) = plain(&[
        "generate",
        "python-client",
        path,
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "the generator takes the declared flag: {out}");
    let undeclared = common::write_fixture(
        "undeclared_flag",
        &DECLARED_FLAG.replace("close(id, force: Bool)", "close(id, force)"),
    );
    let (code, out) = plain(&[
        "generate",
        "python-client",
        undeclared.to_str().unwrap(),
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert_ne!(code, 0, "the undeclared flag has no kind: {out}");
    assert!(out.contains("force"), "the refusal names the flag: {out}");

    let (code, out) = bin(
        &[
            "propose",
            path,
            "open",
            "--actor",
            "desk",
            "--args-named",
            r#"{"id":"o1"}"#,
        ],
        None,
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = bin(
        &[
            "propose",
            path,
            "close",
            "--actor",
            "desk",
            "--args",
            r#"[{"type":"subject","value":"o1"},{"type":"decimal","value":"1"}]"#,
        ],
        None,
    );
    assert_eq!(code, 1, "{out}");
    let error = json_line(&out, 0);
    assert_eq!(error["code"], "kernel_error", "{error}");
    assert_eq!(error["kind"], "argument_kind_mismatch", "{error}");
    assert_eq!(claims_written(), 1, "the misfit wrote nothing");
    let (code, out) = bin(
        &[
            "propose",
            path,
            "close",
            "--actor",
            "desk",
            "--args-named",
            r#"{"id":"o1","force":true}"#,
        ],
        None,
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(json_line(&out, 0)["status"], "committed");
}
