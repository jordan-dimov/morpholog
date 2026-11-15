//! A refusal by a named gate names the act's parameters the gate reads,
//! the same on every route that prints a refusal: a one-shot proposal,
//! with `--trace` and with `--explain-on-reject`, a `--batch` row, and a
//! `transact` act. An invariant's refusal names none.

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

invariant one_approval_per_doc:
    Approved(d, a) and Approved(d, b) implies a = b

transformation set_limit(doc, limit):
    admit ApprovalLimit(doc, limit)

transformation approve(doc, amount, memo: Subject):
    require within_limit: ApprovalLimit(doc, limit) and amount <= limit
    admit Approved(doc, amount)
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
    propose("set_limit", r#"{"doc":"d1","limit":"100"}"#, &[]);
    propose("approve", r#"{"doc":"d1","amount":"50","memo":"ok"}"#, &[]);

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
        "{}\n{}\n",
        row.replacen('{', r#"{"op":"propose","#, 1),
        format_args!(r#"{{"op":"transact","acts":[{set_limit},{row}]}}"#),
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
    let invariant = propose(
        "approve",
        r#"{"doc":"d1","amount":"60","memo":"again"}"#,
        &[],
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
    assert_eq!(invariant["rule"], "one_approval_per_doc", "{invariant}");
    assert!(invariant.get("related_parameters").is_none(), "{invariant}");
}
