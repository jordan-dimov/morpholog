//! A refusal by a named gate names the act's parameters the gate reads,
//! the same on every route that prints a refusal: a one-shot proposal,
//! with `--trace` and with `--explain-on-reject`, a `--batch` row, and a
//! `transact` act, and an explanation's gate verdict: `explain`, the
//! session's, and the one nested in `--explain-on-reject`. An invariant's
//! refusal names none, even when the invariant shares the gate's name.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;
use sqlx::PgPool;

const PROGRAMME: &str = "\
program approvals

predicate ApprovalLimit(doc: Subject, limit: Decimal)
predicate Approved(doc: Subject, amount: Decimal)
predicate Limit(book: Subject, cap: Decimal)

invariant within_limit:
    Approved(d, a) and Approved(d, b) implies a = b

transformation set_limit(doc, limit):
    admit ApprovalLimit(doc, limit)

transformation approve(doc, amount, memo: Subject):
    require within_limit: ApprovalLimit(doc, limit) and amount <= limit
    admit Approved(doc, amount)

transformation revise(doc, amount, memo: Subject):
    bind limit_on_file: ApprovalLimit(doc, limit)
    admit Approved(doc, amount)

transformation reopen(doc: Subject, memo: Subject):
    require none_approved: sum(1 | Approved(d, a)) = 0

transformation reopen_unnamed(doc: Subject, memo: Subject):
    require sum(1 | Approved(d, a)) = 0

transformation set_cap(book, cap: Decimal):
    admit Limit(book, cap)

transformation trade(book, before: Decimal, after: Decimal, memo: Subject):
    require shrinks: (forall cap in Limit(book: book, cap: cap): before <= cap or after < before)
";

fn run(args: &[&str], stdin: Option<&str>) -> Value {
    let mut child = Command::new(common::bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn morpholog binary");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    // One object, pretty-printed; a batch prints one receipt per line.
    serde_json::from_str(&stdout)
        .or_else(|_| serde_json::from_str(stdout.lines().next().unwrap_or("null")))
        .unwrap_or_else(|e| panic!("{e}: {stdout}\n{}", String::from_utf8_lossy(&output.stderr)))
}

#[tokio::test(flavor = "current_thread")]
async fn every_route_names_the_parameters_a_refusing_gate_reads() {
    let base = morpholog_postgres::with_connection_defaults(&common::database_url());
    let name = format!("morpholog_cli_related_{}", std::process::id());
    let admin = PgPool::connect(&common::with_database(&base, "postgres"))
        .await
        .unwrap();
    for sql in [
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        format!("CREATE DATABASE {name}"),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&admin)
            .await
            .unwrap();
    }
    let url = common::with_database(&base, &name);
    let fixture = common::write_fixture("approvals", PROGRAMME);
    let file = fixture.to_str().unwrap();
    let db = ["--database-url", url.as_str()];
    run(&[&["init"][..], &db].concat(), None);
    let propose = |transformation: &str, args: &str, extra: &[&str]| {
        let mut argv = vec![
            "propose",
            file,
            transformation,
            "--actor",
            "clerk",
            "--args-named",
            args,
        ];
        argv.extend_from_slice(&db);
        argv.extend_from_slice(extra);
        run(&argv, None)
    };
    let explain = |transformation: &str, args: &str| {
        let mut argv = vec![
            "explain",
            file,
            transformation,
            "--actor",
            "clerk",
            "--args-named",
            args,
            "--json",
        ];
        argv.extend_from_slice(&db);
        run(&argv, None)
    };
    propose("set_limit", r#"{"doc":"d1","limit":"100"}"#, &[]);
    propose("approve", r#"{"doc":"d1","amount":"50","memo":"ok"}"#, &[]);
    propose("set_cap", r#"{"book":"b1","cap":"10"}"#, &[]);

    let too_much = r#"{"doc":"d1","amount":"500","memo":"big"}"#;
    let plain = propose("approve", too_much, &[]);
    let explained = propose("approve", too_much, &["--explain-on-reject"]);
    let traced = propose("approve", too_much, &["--trace"]);
    let row = format!(r#"{{"transformation":"approve","actor":"clerk","args_named":{too_much}}}"#);
    let batch = run(
        &[&["propose", file, "--batch", "-"][..], &db].concat(),
        Some(&format!("{row}\n")),
    );
    let acts_dir = tempfile::tempdir().unwrap();
    let acts = acts_dir.path().join("acts.json");
    std::fs::write(
        &acts,
        format!(
            "{{\"transformation\":\"set_limit\",\"actor\":\"clerk\",\"args_named\":{{\"doc\":\"d2\",\"limit\":\"10\"}}}}\n{row}\n"
        ),
    )
    .unwrap();
    let atomic = run(
        &[
            &["transact", file, "--acts", acts.to_str().unwrap()][..],
            &db,
        ]
        .concat(),
        None,
    );
    // The session's answers, a proposal and an atomic batch, line by line
    // after its ready line.
    let set_limit =
        r#"{"transformation":"set_limit","actor":"clerk","args_named":{"doc":"d3","limit":"10"}}"#;
    let session_input = format!(
        "{}\n{}\n{}\n",
        row.replacen('{', r#"{"op":"propose","#, 1),
        format_args!(r#"{{"op":"transact","acts":[{set_limit},{row}]}}"#),
        row.replacen('{', r#"{"op":"explain","#, 1),
    );
    let mut session = Command::new(common::bin())
        .args(["session", file, "--database-url", url.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn morpholog session");
    session
        .stdin
        .take()
        .unwrap()
        .write_all(session_input.as_bytes())
        .unwrap();
    let answers = String::from_utf8(session.wait_with_output().unwrap().stdout).unwrap();
    let answers: Vec<Value> = answers
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let again = r#"{"doc":"d1","amount":"60","memo":"again"}"#;
    let invariant = propose("approve", again, &[]);
    let explained_gate = explain("approve", too_much);
    let explained_invariant = explain("approve", again);
    let explained_bind = explain("revise", r#"{"doc":"d9","amount":"5","memo":"m"}"#);
    let state_only = explain("reopen", r#"{"doc":"d1","memo":"m"}"#);
    let state_only_unnamed = explain("reopen_unnamed", r#"{"doc":"d1","memo":"m"}"#);
    let shrinks = explain(
        "trade",
        r#"{"book":"b1","before":"20","after":"30","memo":"m"}"#,
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();

    let named = serde_json::json!(["doc", "amount"]);
    for (route, envelope) in [
        ("propose", &plain),
        ("--explain-on-reject", &explained),
        ("--trace", &traced["result"]),
        ("--batch", &batch),
        ("transact", &atomic),
        ("session propose", &answers[0]),
        ("session transact", &answers[1]),
    ] {
        assert_eq!(envelope["status"], "rejected", "{route}: {envelope}");
        assert_eq!(envelope["rule"], "within_limit", "{route}: {envelope}");
        assert_eq!(envelope["related_parameters"], named, "{route}: {envelope}");
    }
    assert_eq!(atomic["act"], 2, "{atomic}");
    // The same `rule` as the gate's, from the invariant: no parameters.
    assert_eq!(invariant["rule"], "within_limit", "{invariant}");
    assert!(
        invariant["reason"].as_str().unwrap().contains("invariant"),
        "{invariant}"
    );
    assert!(invariant.get("related_parameters").is_none(), "{invariant}");

    // An explanation's gate verdict names what the refusal names.
    for (route, explanation) in [
        ("explain", &explained_gate),
        ("session explain", &answers[2]),
        ("--explain-on-reject", &explained["explanation"]),
    ] {
        let verdict = &explanation["verdict"]["rejected"];
        assert_eq!(verdict["kind"], "gate", "{route}: {explanation}");
        assert_eq!(verdict["rule"], "within_limit", "{route}: {explanation}");
        assert_eq!(
            verdict["related_parameters"], named,
            "{route}: {explanation}"
        );
    }
    // The invariant that shares the name: its verdict has no such field.
    let verdict = &explained_invariant["verdict"]["rejected"];
    assert_eq!(verdict["kind"], "invariant", "{explained_invariant}");
    assert!(verdict.get("related_parameters").is_none(), "{verdict}");
    // A failed named `bind` names what its pattern reads.
    let verdict = &explained_bind["verdict"]["rejected"];
    assert_eq!(verdict["statement_kind"], "bind_one", "{explained_bind}");
    assert_eq!(verdict["related_parameters"], serde_json::json!(["doc"]));
    // A named gate over the state alone reads none; unnamed, it has none
    // to name.
    let verdict = &state_only["verdict"]["rejected"];
    assert_eq!(
        verdict["related_parameters"],
        serde_json::json!([]),
        "{state_only}"
    );
    let verdict = &state_only_unnamed["verdict"]["rejected"];
    assert_eq!(verdict["kind"], "gate", "{state_only_unnamed}");
    assert!(verdict.get("related_parameters").is_none(), "{verdict}");
    // The issue's own gate: every parameter it reads, not the memo.
    assert_eq!(
        shrinks["verdict"]["rejected"]["related_parameters"],
        serde_json::json!(["book", "before", "after"]),
        "{shrinks}"
    );
}
