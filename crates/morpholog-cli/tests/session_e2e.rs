//! End-to-end tests for `morpholog session`: the resident stdio
//! process. The conversation itself is pinned as a golden transcript
//! (`tests/golden/session/transcript.ndjson`): the ready line, then
//! request and response lines alternating, with the volatile fields
//! (transition ids, the binary version) normalised. The generated
//! Python client's session tests consume the same transcript - the
//! requests it must emit byte-identically, the responses it must
//! parse - so the two implementations answer to one conversation.
//!
//! Regenerate after a deliberate protocol change:
//! `UPDATE_GOLDENS=1 cargo test -p morpholog-cli --test session_e2e`

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// A small register of its own, so the transcript's `model_hash` and
/// witnesses do not rot when the worked-example gallery changes.
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

/// The request half of the pinned conversation, in order. These exact
/// bytes are what the generated client must emit; the golden carries
/// them interleaved with the responses each one earned.
const REQUESTS: &[&str] = &[
    r#"{"actor":"teller","args_named":{"account":"acct_1","opened_on":"2026-01-15"},"op":"propose","transformation":"open_account"}"#,
    r#"{"actor":"teller","args_named":{"account":"acct_1","figure":"100"},"op":"propose","transformation":"post_balance"}"#,
    r#"{"actor":"teller","args_named":{"account":"ghost","figure":"5"},"explain_on_reject":true,"op":"propose","transformation":"post_balance"}"#,
    r#"{"named":true,"op":"claims","predicates":["Account"]}"#,
    r#"{"named":true,"op":"claims","predicates":["Balance"],"where":{"figure":"100"}}"#,
    r#"{"name":"BookTotal","named":true,"op":"derived"}"#,
    r#"{"actor":"teller","args_named":{},"op":"propose","transformation":"no_such_act"}"#,
    r#"{"as_of":"not-a-coordinate","op":"claims"}"#,
];

fn database_url() -> String {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-cli integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    morpholog_postgres::with_default_user(&url)
}

async fn reset_db() {
    let pool = morpholog_postgres::PgPool::connect(&database_url())
        .await
        .expect("connect to test DB");
    sqlx::query(morpholog_postgres::testing::RESET_SQL)
        .execute(&pool)
        .await
        .expect("truncate");
}

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

/// Normalise the volatile fields so the transcript pins the
/// conversation, not the run: transition ids are fresh UUIDv7s every
/// commit, and the version rots on every release.
fn normalised(line: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    if let Some(obj) = value.as_object_mut() {
        if obj.contains_key("transition_id") {
            obj.insert(
                "transition_id".to_string(),
                serde_json::json!("00000000-0000-0000-0000-000000000000"),
            );
        }
        if obj.contains_key("morpholog_version") {
            obj.insert("morpholog_version".to_string(), serde_json::json!("0.0.0"));
        }
    }
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
    // Error prose may name the programme file, which lives in a fresh
    // temp directory every run; the placeholder keeps the transcript
    // about the conversation.
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
    // Read the ready line WITHOUT writing anything: it must be
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
    // Blank, malformed, then a lawful request: the blank earns no
    // receipt but consumes a line number, the malformed earns an
    // error receipt, and the session still answers afterwards.
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
    // Write one request while stdin stays OPEN: the response must
    // arrive anyway, or a lockstep client deadlocks on an unflushed
    // buffer.
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

    // Break the substrate mid-session, then restore it before any
    // assertion can bail out - the suite shares this database.
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
