//! End-to-end tests for `morpholog serve`, the resident runtime behind a
//! Unix socket.
//!
//! The attacker modelled here is a process on the same host that can
//! connect to the socket and send any bytes it likes, but holds no
//! database login and cannot change the service's arguments or files. It
//! may present under an unmapped uid, ask for operations it was not
//! granted, name a programme of its own, open many connections, and send
//! lines of any length. Every test runs as one OS user, so a second uid is
//! modelled by mapping only a uid that is not the test's own; the
//! several-users deployment is the auth proof recorded on #475.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "\
program serve_fixture

predicate Account(account: Subject, opened_on: Date)
    unique by (account)

transformation open_account(account, opened_on):
    admit Account(account, opened_on)
";

const OPEN: &str = r#"{"actor":"teller","args_named":{"account":"acct_1","opened_on":"2026-01-15"},"op":"propose","transformation":"open_account"}"#;

/// The uid this test runs as, read from a file it just created.
fn own_uid() -> u32 {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::metadata(file.path()).unwrap().uid()
}

fn model_hash(file: &Path) -> String {
    let out = Command::new(common::bin())
        .arg("hash")
        .arg(file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    value["hash"].as_str().unwrap().to_string()
}

/// A running service, killed when dropped.
struct Served {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
    _fixture: common::Fixture,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Served {
    fn connect(&self) -> (BufReader<UnixStream>, UnixStream) {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        (BufReader::new(stream.try_clone().unwrap()), stream)
    }

    /// Connect and read the ready line.
    fn open(&self) -> (BufReader<UnixStream>, UnixStream) {
        let (mut read, write) = self.connect();
        let ready = read_json(&mut read).expect("a ready line");
        assert_eq!(ready["status"], "ready", "{ready}");
        (read, write)
    }

    /// SIGTERM, then the exit status, the service's stderr, and whether
    /// the socket outlived it (read before the temp directory goes).
    fn stop(mut self) -> (std::process::ExitStatus, String, bool) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let status = self.child.wait().unwrap();
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        (status, stderr, self.socket.exists())
    }
}

fn command(fixture: &Path, hash: &str, socket: &Path, extra: &[String], url: &str) -> Command {
    let mut command = Command::new(common::bin());
    command
        .arg("serve")
        .arg(fixture)
        .args(["--expect-hash", hash])
        .arg("--socket")
        .arg(socket)
        .args(extra)
        .args(["--database-url", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

/// Start the service and wait for its socket.
fn serve(extra: &[String]) -> Served {
    let fixture = common::write_fixture("serve_fixture", FIXTURE);
    let hash = model_hash(&fixture);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve.sock");
    let mut child = command(&fixture, &hash, &socket, extra, &raw_url())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !socket.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("serve exited with {status} before listening: {stderr}");
        }
        assert!(Instant::now() < deadline, "serve did not listen in time");
        std::thread::sleep(Duration::from_millis(20));
    }
    Served {
        child,
        socket,
        _dir: dir,
        _fixture: fixture,
    }
}

/// Run the service expecting it to refuse to start: it must exit non-zero
/// within the deadline. One still running then has started, so it is
/// killed and the test fails, rather than waiting on it forever.
fn refused_start(fixture: &Path, hash: &str, socket: &Path, extra: &[String], url: &str) -> String {
    let mut child = command(fixture, hash, socket, extra, url).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve started when it should not have");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success(), "serve exited zero instead of refusing");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    stderr
}

/// `DATABASE_URL` as given: with no user in it, serve sets each
/// presenter's login itself.
fn raw_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
}

async fn login() -> String {
    common::session_user(&database_url()).await
}

fn presenter(uid: u32, login: &str) -> Vec<String> {
    vec!["--presenter".into(), format!("{uid}={login}")]
}

fn read_json(read: &mut impl BufRead) -> Option<serde_json::Value> {
    let mut line = String::new();
    if read.read_line(&mut line).unwrap() == 0 {
        return None;
    }
    Some(serde_json::from_str(&line).unwrap())
}

fn ask(conn: &mut (BufReader<UnixStream>, UnixStream), request: &str) -> serde_json::Value {
    writeln!(conn.1, "{request}").unwrap();
    read_json(&mut conn.0).expect("a response line")
}

/// True when the service closed the connection without writing a line. A
/// close with unread input can reach this side as a reset.
fn closed_unanswered(read: &mut BufReader<UnixStream>) -> bool {
    let mut line = String::new();
    match read.read_line(&mut line) {
        Ok(n) => n == 0,
        Err(e) => e.kind() == std::io::ErrorKind::ConnectionReset,
    }
}

fn without_transition_id(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(id) = value.get_mut("transition_id") {
        *id = serde_json::json!("<id>");
    }
    value
}

#[tokio::test]
async fn a_presenter_proposes_and_the_audit_names_its_login() {
    let login = login().await;

    reset_db().await;
    let fixture = common::write_fixture("serve_fixture", FIXTURE);
    let mut session = Command::new(common::bin())
        .arg("session")
        .arg(&fixture.path)
        .args(["--database-url", &database_url()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(session.stdout.take().unwrap());
    let session_ready = read_json(&mut out).unwrap();
    writeln!(session.stdin.as_mut().unwrap(), "{OPEN}").unwrap();
    let session_receipt = read_json(&mut out).unwrap();
    drop(session.stdin.take());
    session.wait().unwrap();

    reset_db().await;
    let served = serve(&presenter(own_uid(), &login));
    let (mut read, write) = served.connect();
    let ready = read_json(&mut read).unwrap();
    assert_eq!(
        ready, session_ready,
        "the same programme, the same ready line"
    );
    let mut conn = (read, write);
    let receipt = ask(&mut conn, OPEN);
    assert_eq!(receipt["status"], "committed", "{receipt}");
    assert_eq!(
        without_transition_id(receipt),
        without_transition_id(session_receipt),
        "serve answers a proposal exactly as session does"
    );

    let pool = morpholog_postgres::PgPool::connect(&database_url())
        .await
        .unwrap();
    let (by,): (String,) =
        sqlx::query_as("SELECT attestation->>'authenticated_by' FROM morpholog.audit")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by, login, "the audit row names the presenter's login");
}

#[tokio::test]
async fn an_unmapped_uid_is_closed_unanswered() {
    let login = login().await;
    let served = serve(&presenter(own_uid().wrapping_add(1), &login));
    let (mut read, _write) = served.connect();
    assert!(
        closed_unanswered(&mut read),
        "no ready line for an unmapped uid"
    );
    let (_, stderr, _) = served.stop();
    assert!(stderr.contains("not a presenter"), "{stderr}");
}

#[tokio::test]
async fn reads_and_explanations_need_a_grant() {
    let login = login().await;
    let uid = own_uid();
    let served = serve(&presenter(uid, &login));
    let mut conn = served.open();
    for op in ["explain", "claims", "derived", "audit", "rejections"] {
        let answer = ask(&mut conn, &format!(r#"{{"op":"{op}"}}"#));
        assert_eq!(answer["code"], "unknown_operation", "{op}: {answer}");
        assert!(
            answer["error"].as_str().unwrap().contains("not granted"),
            "{answer}"
        );
    }
    let with_explanation = OPEN.replace(r#""op""#, r#""explain_on_reject":true,"op""#);
    let answer = ask(&mut conn, &with_explanation);
    assert_eq!(answer["code"], "invalid_request", "{answer}");
    assert_eq!(ask(&mut conn, r#"{"op":"ping"}"#)["status"], "ready");
    drop(conn);
    drop(served);

    let mut extra = presenter(uid, &login);
    extra.extend(["--grant".into(), format!("{uid}=claims,explain")]);
    let served = serve(&extra);
    let mut conn = served.open();
    assert!(ask(&mut conn, r#"{"op":"claims"}"#).is_array());
    let explanation = ask(
        &mut conn,
        &OPEN.replace(r#""op":"propose""#, r#""op":"explain""#),
    );
    assert!(explanation.get("code").is_none(), "{explanation}");
    let answer = ask(&mut conn, r#"{"op":"audit"}"#);
    assert_eq!(answer["code"], "unknown_operation", "audit was not granted");
}

#[tokio::test]
async fn a_request_cannot_choose_the_programme() {
    let login = login().await;
    let served = serve(&presenter(own_uid(), &login));
    let mut conn = served.open();
    for field in [
        r#""program":"/tmp/weaker.morph""#,
        r#""model_hash":"sha256:00""#,
    ] {
        let request = OPEN.replace(r#""op""#, &format!(r#"{field},"op""#));
        let answer = ask(&mut conn, &request);
        assert_eq!(answer["code"], "invalid_request", "{field}: {answer}");
    }
}

#[tokio::test]
async fn a_wrong_hash_refuses_before_the_socket_exists() {
    let login = login().await;
    let fixture = common::write_fixture("serve_fixture", FIXTURE);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve.sock");
    let stderr = refused_start(
        &fixture,
        "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        &socket,
        &presenter(own_uid(), &login),
        &raw_url(),
    );
    assert!(stderr.contains("hashes to"), "{stderr}");
    assert!(!socket.exists(), "nothing listened");
}

#[tokio::test]
async fn an_existing_socket_path_refuses_to_start() {
    let login = login().await;
    let fixture = common::write_fixture("serve_fixture", FIXTURE);
    let hash = model_hash(&fixture);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve.sock");
    std::fs::write(&socket, "").unwrap();
    let stderr = refused_start(
        &fixture,
        &hash,
        &socket,
        &presenter(own_uid(), &login),
        &raw_url(),
    );
    assert!(stderr.contains("binding"), "{stderr}");
}

#[tokio::test]
async fn a_login_the_connection_did_not_get_refuses_to_start() {
    let fixture = common::write_fixture("serve_fixture", FIXTURE);
    let hash = model_hash(&fixture);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve.sock");
    // The URL names the test's own login, so the connection logs in as
    // that and not as the mapped one.
    let stderr = refused_start(
        &fixture,
        &hash,
        &socket,
        &presenter(own_uid(), "someone_else"),
        &database_url(),
    );
    assert!(stderr.contains("logged in as"), "{stderr}");
    assert!(!socket.exists(), "nothing listened");
}

#[tokio::test]
async fn a_uid_or_a_login_mapped_twice_refuses_to_start() {
    let login = login().await;
    let fixture = common::write_fixture("serve_fixture", FIXTURE);
    let hash = model_hash(&fixture);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve.sock");
    let uid = own_uid();

    let mut twice = presenter(uid, &login);
    twice.extend(presenter(uid, "another_login"));
    let stderr = refused_start(&fixture, &hash, &socket, &twice, &raw_url());
    assert!(stderr.contains("mapped twice"), "{stderr}");

    let mut shared = presenter(uid, &login);
    shared.extend(presenter(uid.wrapping_add(1), &login));
    let stderr = refused_start(&fixture, &hash, &socket, &shared, &raw_url());
    assert!(stderr.contains("two uids"), "{stderr}");
}

#[tokio::test]
async fn a_presenter_holds_a_bounded_number_of_connections() {
    let login = login().await;
    let served = serve(&presenter(own_uid(), &login));
    let held: Vec<_> = (0..4).map(|_| served.open()).collect();
    let (mut read, _write) = served.connect();
    assert!(closed_unanswered(&mut read), "a fifth connection is closed");
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (mut read, _write) = served.connect();
        if read_json(&mut read).is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closed connections free their slots"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn an_overlong_line_closes_only_its_connection() {
    let login = login().await;
    let served = serve(&presenter(own_uid(), &login));
    let (mut read, mut write) = served.open();
    let _ = write.write_all(&vec![b'a'; 1024 * 1024 + 1]);
    assert!(
        closed_unanswered(&mut read),
        "the long line closed its connection"
    );
    let mut conn = served.open();
    assert_eq!(ask(&mut conn, r#"{"op":"ping"}"#)["status"], "ready");
}

#[tokio::test]
async fn sigterm_removes_the_socket() {
    let login = login().await;
    let served = serve(&presenter(own_uid(), &login));
    let (status, _, socket_left) = served.stop();
    assert!(status.success(), "{status}");
    assert!(!socket_left, "the socket outlived the service");
}
