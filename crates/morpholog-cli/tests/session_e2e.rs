//! End-to-end tests for `morpholog session`, the resident stdio process.
//! The conversation is pinned as a golden transcript
//! (`tests/golden/session/transcript.ndjson`): the ready line, then
//! alternating requests and responses, with the values that change per
//! run or per machine normalised. The generated Python client's tests read the same
//! transcript, so both sides answer to one conversation.
//!
//! Regenerate after a deliberate protocol change:
//! `UPDATE_GOLDENS=1 cargo test -p morpholog-cli --test session_e2e`

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// A programme of its own, so the transcript does not change when the
/// worked examples do.
const FIXTURE: &str = "\
program session_fixture

predicate Account(account: Subject, opened_on: Date)
    unique by (account)
predicate Balance(account: Subject, figure: Decimal)
predicate BookTotal(account: Subject, total: Decimal)

invariant balances_name_an_account:
    Balance(a, _) implies Account(a, _)

derived BookTotal(account):
    over Balance(account, _)
    value total = sum(f | Balance(account, f))

transformation open_account(account, opened_on):
    admit Account(account, opened_on)

transformation post_balance(account, figure):
    admit Balance(account, figure)
";

/// The requests of the pinned conversation, in order: the exact bytes the
/// generated client must emit.
const REQUESTS: &[&str] = &[
    r#"{"actor":"teller","args_named":{"account":"acct_1","opened_on":"2026-01-15"},"op":"propose","transformation":"open_account"}"#,
    r#"{"actor":"teller","args_named":{"account":"acct_1","figure":"100"},"op":"propose","transformation":"post_balance"}"#,
    r#"{"actor":"teller","args_named":{"account":"ghost","figure":"5"},"explain_on_reject":true,"op":"propose","transformation":"post_balance"}"#,
    r#"{"named":true,"op":"claims","predicates":["Account"]}"#,
    r#"{"named":true,"op":"claims","predicates":["Balance"],"where":{"figure":"100"}}"#,
    r#"{"name":"BookTotal","named":true,"op":"derived"}"#,
    r#"{"actor":"teller","args_named":{},"op":"propose","transformation":"no_such_act"}"#,
    r#"{"as_of":"not-a-coordinate","op":"claims"}"#,
    // Reads between two proposals: the resident loop's shape.
    r#"{"named":true,"op":"audit"}"#,
    r#"{"limit":1,"op":"audit"}"#,
    r#"{"limit":100,"op":"rejections"}"#,
    r#"{"op":"ping"}"#,
    r#"{"after":"00000000-0000-0000-0000-000000000000","op":"audit"}"#,
    r#"{"actor":"teller","args_named":{"account":"acct_2","opened_on":"2026-02-01"},"op":"propose","transformation":"open_account"}"#,
];

fn spawn_session(file: &std::path::Path) -> std::process::Child {
    Command::new(common::bin())
        .args(["session"])
        .arg(file)
        .args(["--database-url", &database_url()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn morpholog session")
}

/// The fields that change per run, per machine or per release, wherever
/// they sit: ids, instants, the login that proposed, and the version.
fn stable(key: &str) -> Option<serde_json::Value> {
    match key {
        "transition_id" | "rejection_id" => {
            Some(serde_json::json!("00000000-0000-0000-0000-000000000000"))
        }
        "committed_at" | "rejected_at" => Some(serde_json::json!("2000-01-01T00:00:00Z")),
        "authenticated_by" => Some(serde_json::json!("login")),
        "authenticated_by_oid" => Some(serde_json::json!(0)),
        "morpholog_version" => Some(serde_json::json!("0.0.0")),
        _ => None,
    }
}

fn normalised(line: &str) -> String {
    fn walk(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(obj) => {
                for (key, v) in obj.iter_mut() {
                    match stable(key) {
                        Some(replacement) => *v = replacement,
                        None => walk(v),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    walk(&mut value);
    serde_json::to_string(&value).unwrap()
}

fn transcript_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/session")
}

#[tokio::test]
async fn the_pinned_transcript_is_the_conversation() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();
    for request in REQUESTS {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin); // EOF: the clean shutdown.
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "session should exit 0 on EOF; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let responses: Vec<&str> = stdout.lines().collect();
    // One ready line, then exactly one response per request, in order.
    assert_eq!(responses.len(), REQUESTS.len() + 1, "{stdout}");

    let mut conversation = String::new();
    conversation.push_str(&normalised(responses[0]));
    conversation.push('\n');
    for (request, response) in REQUESTS.iter().zip(&responses[1..]) {
        conversation.push_str(request);
        conversation.push('\n');
        conversation.push_str(&normalised(response));
        conversation.push('\n');
    }
    // Error prose may name the programme's temp path, which changes each
    // run.
    let conversation = conversation.replace(&fixture.path.display().to_string(), "<fixture>");

    let path = transcript_path().join("transcript.ndjson");
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(transcript_path()).unwrap();
        std::fs::write(&path, &conversation).unwrap();
        return;
    }
    let pinned = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing transcript golden ({e}); run with UPDATE_GOLDENS=1"));
    assert_eq!(
        pinned, conversation,
        "the session conversation drifted from the pinned transcript; if the \
         protocol change is deliberate, regenerate with UPDATE_GOLDENS=1 and \
         update the Python session tests to match"
    );
}

#[tokio::test]
async fn the_ready_line_arrives_before_any_request_and_matches_hash() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    // Read the ready line before writing anything: it must arrive
    // unprompted and flushed, or a lockstep client would deadlock.
    let stdout = child.stdout.take().unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let _ = send.send(line);
    });
    let ready_line = recv
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the ready line must arrive unprompted");
    let ready: serde_json::Value = serde_json::from_str(&ready_line).unwrap();
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["protocol"], 1);
    assert_eq!(ready["program"], "session_fixture");

    // The staleness token is the same hash `morpholog hash` reports.
    let hash_out = Command::new(common::bin())
        .args(["hash"])
        .arg(&fixture.path)
        .output()
        .unwrap();
    let hash: serde_json::Value =
        serde_json::from_str(&String::from_utf8(hash_out.stdout).unwrap()).unwrap();
    assert_eq!(ready["model_hash"], hash["hash"]);

    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[tokio::test]
async fn blank_lines_are_skipped_and_the_session_keeps_answering() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();
    // Blank, malformed, then a valid request. The blank gets no receipt
    // but uses a line number; the malformed one gets an error receipt; the
    // session still answers afterwards.
    writeln!(stdin).unwrap();
    writeln!(stdin, "this is not json").unwrap();
    writeln!(stdin, r#"{{"op":"nonsense"}}"#).unwrap();
    writeln!(
        stdin,
        r#"{{"actor":"teller","args_named":{{"account":"a1","opened_on":"2026-01-15"}},"op":"propose","transformation":"open_account"}}"#
    )
    .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4, "{stdout}");
    assert_eq!(lines[0]["status"], "ready");
    assert_eq!(lines[1]["status"], "error");
    assert_eq!(lines[1]["code"], "invalid_request");
    assert_eq!(lines[1]["row"], 2, "the blank line consumed row 1");
    assert_eq!(lines[2]["status"], "error");
    assert_eq!(lines[2]["code"], "unknown_operation");
    assert_eq!(lines[3]["status"], "committed");
    assert_eq!(lines[3]["row"], 4);
}

#[tokio::test]
async fn requests_are_answered_lockstep_not_only_at_eof() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let _ = send.send(line.unwrap());
        }
    });
    let timeout = std::time::Duration::from_secs(30);
    let ready = recv.recv_timeout(timeout).expect("ready line");
    assert!(ready.contains("\"ready\""));
    // Write one request with stdin still open: the response must arrive
    // anyway, or a lockstep client deadlocks.
    writeln!(
        stdin,
        r#"{{"actor":"teller","args_named":{{"account":"a1","opened_on":"2026-01-15"}},"op":"propose","transformation":"open_account"}}"#
    )
    .unwrap();
    let first = recv
        .recv_timeout(timeout)
        .expect("a response before EOF: every line is flushed");
    assert!(first.contains("\"committed\""), "{first}");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[tokio::test]
async fn operational_failure_aborts_with_a_nonzero_exit_not_a_receipt() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let _ = send.send(line.unwrap());
        }
    });
    let timeout = std::time::Duration::from_secs(30);
    let _ready = recv.recv_timeout(timeout).expect("ready line");

    // Break the database mid-session, then restore it before any
    // assertion can fail, since the suite shares it.
    let pool = morpholog_postgres::PgPool::connect(&database_url())
        .await
        .expect("second connection");
    sqlx::query("ALTER TABLE morpholog.claims RENAME TO claims_hidden")
        .execute(&pool)
        .await
        .expect("hide the claims table");
    writeln!(stdin, r#"{{"op":"claims"}}"#).unwrap();
    let status = child.wait();
    let leftover: Vec<String> = recv.try_iter().collect();
    sqlx::query("ALTER TABLE morpholog.claims_hidden RENAME TO claims")
        .execute(&pool)
        .await
        .expect("restore the claims table");

    let status = status.expect("session exits");
    assert!(
        !status.success(),
        "an operational failure must abort with a non-zero exit"
    );
    assert!(
        leftover.is_empty(),
        "no receipt for an operational failure; got {leftover:?}"
    );
}

/// A programme whose actor policy allows one login only. The grant is a
/// claim compared with `session_user` as text, so naming some other login
/// makes this connection unauthorised, with no role switching.
const POLICY_FIXTURE: &str = "program policy_demo

predicate ActorAssertionRestricted(actor: Subject)
predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)
predicate Noted(id: Subject)

transformation arm(person, login_role):
    admit ActorAssertionRestricted(person)
    admit ActorAssertionAuthority(person, login_role)

transformation note(id):
    admit Noted(id)
";

fn request(op: &str, transformation: &str, actor: &str, args: serde_json::Value) -> String {
    serde_json::json!({
        "op": op,
        "transformation": transformation,
        "actor": actor,
        "args_named": args,
    })
    .to_string()
}

#[tokio::test]
async fn an_unauthorised_actor_is_a_coded_receipt_and_the_session_stays_usable() {
    reset_db().await;
    let fixture = common::write_fixture("session_policy", POLICY_FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();

    // Arm `restricted_actor` for a login this connection is not.
    writeln!(
        stdin,
        "{}",
        request(
            "propose",
            "arm",
            "bootstrap",
            serde_json::json!({"person": "restricted_actor", "login_role": "somebody_else"}),
        )
    )
    .unwrap();
    // Refused: well-formed request, unauthorised assertion.
    writeln!(
        stdin,
        "{}",
        request(
            "propose",
            "note",
            "restricted_actor",
            serde_json::json!({"id": "n1"}),
        )
    )
    .unwrap();
    // The session stays healthy: the refusal is about authority, not the
    // conversation.
    writeln!(
        stdin,
        "{}",
        request(
            "propose",
            "note",
            "anyone_else",
            serde_json::json!({"id": "n2"})
        ),
    )
    .unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4, "ready plus one response each: {stdout}");

    let armed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(armed["status"], "committed", "{}", lines[1]);

    let refused: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(refused["status"], "error", "{}", lines[2]);
    assert_eq!(
        refused["code"], "actor_assertion_unauthorised",
        "the code is the stable surface a caller branches on: {}",
        lines[2]
    );

    let after: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
    assert_eq!(
        after["status"], "committed",
        "the session must survive an unauthorised request: {}",
        lines[3]
    );
    assert!(output.status.success(), "session should exit 0 on EOF");
}

/// A session `where` resolves as the one-shot read does: without the named
/// read there is nothing to resolve field names against. The refusal is a
/// coded receipt, and the session carries on.
#[tokio::test]
async fn a_where_clause_without_the_named_read_is_a_coded_receipt() {
    reset_db().await;
    let fixture = common::write_fixture("session_where", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"named":false,"op":"claims","predicates":["Balance"],"where":{{"figure":"100"}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"named":true,"op":"claims","predicates":["Balance"],"where":{{"figure":"100"}}}}"#
    )
    .unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "ready plus one response each: {stdout}");

    let refused: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(refused["status"], "error", "{}", lines[1]);
    assert_eq!(refused["code"], "invalid_arguments", "{}", lines[1]);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("needs the named read"),
        "{}",
        lines[1]
    );
    let after: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert!(after.is_array(), "the named read answers: {}", lines[2]);
    assert!(output.status.success(), "session should exit 0 on EOF");
}

/// The session's `transact`: one decision with this request's row, and the
/// session still in step after a refusal and after an empty batch.
#[tokio::test]
async fn transact_answers_with_the_one_decision_and_the_session_stays_in_step() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();

    let act = |transformation: &str, args: serde_json::Value| serde_json::json!({"transformation": transformation, "actor": "teller", "args_named": args});
    // Refused at act 2: the balance names no account.
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"op": "transact", "acts": [
            act("open_account", serde_json::json!({"account": "acct_1", "opened_on": "2026-01-15"})),
            act("post_balance", serde_json::json!({"account": "ghost", "figure": "5"})),
        ]})
    )
    .unwrap();
    // Empty: one invalid request.
    writeln!(stdin, r#"{{"op":"transact","acts":[]}}"#).unwrap();
    // Committed: both acts, in order.
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"op": "transact", "acts": [
            act("open_account", serde_json::json!({"account": "acct_1", "opened_on": "2026-01-15"})),
            act("post_balance", serde_json::json!({"account": "acct_1", "figure": "100"})),
        ]})
    )
    .unwrap();
    writeln!(stdin, r#"{{"op":"claims","predicates":["Balance"]}}"#).unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 5, "ready plus four answers: {lines:?}");
    assert_eq!(lines[1]["status"], "rejected", "{}", lines[1]);
    assert_eq!(lines[1]["act"], 2);
    assert_eq!(lines[1]["row"], 1);
    assert_eq!(lines[2]["code"], "invalid_request", "{}", lines[2]);
    assert_eq!(lines[2]["row"], 2);
    assert_eq!(lines[3]["status"], "committed", "{}", lines[3]);
    assert_eq!(lines[3]["row"], 3);
    assert_eq!(lines[3]["acts"].as_array().unwrap().len(), 2);
    assert_eq!(
        lines[4].as_array().unwrap().len(),
        1,
        "the balance is on the record"
    );
}

/// A commit through the resident session names the programme the session
/// loaded: the hash on its ready line.
#[tokio::test]
async fn a_session_commit_names_the_programme_the_session_loaded() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"actor":"teller","args_named":{{"account":"a1","opened_on":"2026-01-15"}},"op":"propose","transformation":"open_account"}}"#
    )
    .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut lines = stdout.lines();
    let ready: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let receipt: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(receipt["status"], "committed", "{receipt}");

    let tail = Command::new(common::bin())
        .args(["inspect", "audit", "--database-url", &database_url()])
        .output()
        .unwrap();
    let row: serde_json::Value = serde_json::from_str(
        String::from_utf8(tail.stdout)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(row["model_hash"], ready["model_hash"]);
}

/// Lines from a running session, read on a thread so a test can wait
/// with a deadline.
fn lines_of(child: &mut std::process::Child) -> std::sync::mpsc::Receiver<String> {
    let stdout = child.stdout.take().unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let _ = send.send(line.unwrap());
        }
    });
    recv
}

fn answer(
    stdin: &mut std::process::ChildStdin,
    recv: &std::sync::mpsc::Receiver<String>,
    request: &serde_json::Value,
) -> serde_json::Value {
    writeln!(stdin, "{request}").unwrap();
    let line = recv
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("no answer to {request}: {e}"));
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn an_audit_read_resumes_after_its_last_row_and_bad_arguments_are_receipts() {
    reset_db().await;
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = spawn_session(&fixture.path);
    let mut stdin = child.stdin.take().unwrap();
    let recv = lines_of(&mut child);
    let ready: serde_json::Value = serde_json::from_str(
        &recv
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap(),
    )
    .unwrap();
    for account in ["a1", "a2", "a3"] {
        let receipt = answer(
            &mut stdin,
            &recv,
            &serde_json::json!({"op": "propose", "transformation": "open_account",
                "actor": "teller", "args_named": {"account": account, "opened_on": "2026-01-15"}}),
        );
        assert_eq!(receipt["status"], "committed", "{receipt}");
    }

    let first = answer(
        &mut stdin,
        &recv,
        &serde_json::json!({"op": "audit", "limit": 2}),
    );
    let first = first.as_array().unwrap();
    assert_eq!(first.len(), 2, "the limit caps the answer");
    let cursor = first[1]["transition_id"].as_str().unwrap();
    let rest = answer(
        &mut stdin,
        &recv,
        &serde_json::json!({"op": "audit", "after": cursor}),
    );
    let rest = rest.as_array().unwrap();
    assert_eq!(rest.len(), 1, "the next read resumes after the last row");
    assert_eq!(rest[0]["arguments"][0]["value"], "a3");
    let end = rest[0]["transition_id"].as_str().unwrap();
    let none = answer(
        &mut stdin,
        &recv,
        &serde_json::json!({"op": "audit", "after": end}),
    );
    assert_eq!(none, serde_json::json!([]));
    let mentioning = answer(
        &mut stdin,
        &recv,
        &serde_json::json!({"op": "audit", "mentions": "a2"}),
    );
    assert_eq!(mentioning.as_array().unwrap().len(), 1, "{mentioning}");

    // Each of these the caller can correct, so each is a coded receipt
    // and the session keeps answering.
    for bad in [
        serde_json::json!({"op": "audit", "after": "not-an-id"}),
        serde_json::json!({"op": "audit", "after": "00000000-0000-0000-0000-000000000000"}),
        serde_json::json!({"op": "audit", "mentions": ""}),
        serde_json::json!({"op": "audit", "writer_roles": []}),
        serde_json::json!({"op": "audit", "writer_roles": ["morpholog_ci_no_such_role"]}),
        serde_json::json!({"op": "audit", "limit": 0}),
        serde_json::json!({"op": "rejections", "mentions": ""}),
    ] {
        let receipt = answer(&mut stdin, &recv, &bad);
        assert_eq!(receipt["code"], "invalid_arguments", "{bad}: {receipt}");
    }
    for misspelt in [
        serde_json::json!({"op": "audit", "afer": "x"}),
        serde_json::json!({"op": "ping", "deep": true}),
    ] {
        let receipt = answer(&mut stdin, &recv, &misspelt);
        assert_eq!(receipt["code"], "invalid_request", "{misspelt}: {receipt}");
    }
    let pong = answer(&mut stdin, &recv, &serde_json::json!({"op": "ping"}));
    assert_eq!(pong, ready, "a ping answers with the ready line");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

/// A database of the test's own, with the schema, so a test can break it
/// without touching the shared one. Dropped by [`OwnDatabase::drop_it`].
struct OwnDatabase {
    admin: morpholog_postgres::PgPool,
    name: String,
    url: String,
}

impl OwnDatabase {
    async fn new(tag: &str) -> Self {
        let base = database_url();
        let name = format!("morpholog_cli_{tag}_{}", std::process::id());
        let admin = morpholog_postgres::PgPool::connect(&common::with_database(&base, "postgres"))
            .await
            .unwrap();
        let db = OwnDatabase {
            url: common::with_database(&base, &name),
            admin,
            name,
        };
        db.run(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", db.name))
            .await;
        db.run(&format!("CREATE DATABASE {}", db.name)).await;
        let init = Command::new(common::bin())
            .args(["init", "--database-url", &db.url])
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        db
    }

    async fn run(&self, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&self.admin)
            .await
            .unwrap();
    }

    async fn drop_it(self) {
        self.run(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ))
        .await;
    }
}

/// A session on `url` that has answered one ping, with its ready line.
fn pinged_session(
    url: &str,
) -> (
    std::process::Child,
    std::process::ChildStdin,
    std::sync::mpsc::Receiver<String>,
) {
    let fixture = common::write_fixture("session_fixture", FIXTURE);
    let mut child = Command::new(common::bin())
        .arg("session")
        .arg(&fixture.path)
        .args(["--database-url", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let recv = lines_of(&mut child);
    let ready: serde_json::Value = serde_json::from_str(
        &recv
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap(),
    )
    .unwrap();
    let pong = answer(&mut stdin, &recv, &serde_json::json!({"op": "ping"}));
    assert_eq!(pong, ready, "a ping answers with the ready line");
    (child, stdin, recv)
}

/// Ping, then wait for the session to end without answering. An answer
/// or a hang is the error.
fn ping_expecting_abort(
    mut child: std::process::Child,
    mut stdin: std::process::ChildStdin,
    recv: &std::sync::mpsc::Receiver<String>,
) -> Result<(), String> {
    writeln!(stdin, r#"{{"op":"ping"}}"#).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if let Ok(line) = recv.try_recv() {
            child.kill().unwrap();
            return Err(format!(
                "the session answered a ping it could not make: {line}"
            ));
        }
        if let Some(status) = child.try_wait().unwrap() {
            if status.success() {
                return Err("the session exited 0 on a failed ping".to_string());
            }
            let leftover: Vec<String> = recv.try_iter().collect();
            return match leftover.is_empty() {
                true => Ok(()),
                false => Err(format!("no answer to the ping; got {leftover:?}")),
            };
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            return Err("the session hung on a failed ping".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A ping reaches the database, so a session whose database stops
/// accepting connections aborts on it rather than answering. Ending the
/// session's connection alone is not enough, because the pool reconnects;
/// the database is also closed to new ones.
#[tokio::test]
async fn a_ping_aborts_the_session_when_the_database_is_unreachable() {
    let db = OwnDatabase::new("ping_closed").await;
    let (child, stdin, recv) = pinged_session(&db.url);
    db.run(&format!(
        "ALTER DATABASE {} ALLOW_CONNECTIONS false",
        db.name
    ))
    .await;
    db.run(&format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}'",
        db.name
    ))
    .await;
    let outcome = ping_expecting_abort(child, stdin, &recv);
    db.drop_it().await;
    outcome.unwrap_or_else(|e| panic!("{e}"));
}

/// A ping also checks the schema, so a database migrated past this binary
/// under a running session aborts it on the next ping.
#[tokio::test]
async fn a_ping_aborts_the_session_when_the_database_moved_past_its_binary() {
    let db = OwnDatabase::new("ping_ahead").await;
    let (child, stdin, recv) = pinged_session(&db.url);
    let pool = morpholog_postgres::PgPool::connect(&db.url).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO morpholog.schema_migrations (version, name) \
         VALUES (9999, 'from_a_later_binary')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let outcome = ping_expecting_abort(child, stdin, &recv);
    db.drop_it().await;
    outcome.unwrap_or_else(|e| panic!("{e}"));
}
