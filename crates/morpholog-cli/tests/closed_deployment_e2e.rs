//! Closing a deployment through the documented commands, on a database of
//! its own: a plain `init` leaves it open and says so, `init --root-actor`
//! closes it, the root's login may then propose as the root and nobody may
//! propose as an actor no grant names, and the session's ready line and
//! `check --json` say it is closed. Closing a deployment with history
//! first lists the recorded actors no grant names.
//!
//! The root login is this test's own login, so it makes no roles; the
//! adapter suite `closed_deployment.rs` holds other logins to the rule.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use sqlx::PgPool;

const PROGRAMME: &str = "\
program closed_e2e

predicate Note(text: Subject, author: Subject)

transformation note(text):
    admit Note(text, actor)
";

fn run(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(common::bin())
        .args(args)
        .output()
        .expect("spawn morpholog binary");
    (
        output.status,
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn note(url: &str, file: &str, actor: &str) -> (std::process::ExitStatus, String, String) {
    run(&[
        "propose",
        "--database-url",
        url,
        file,
        "note",
        "--actor",
        actor,
        "--args-named",
        r#"{"text":"hello"}"#,
    ])
}

/// The first line a session prints.
fn session_ready(url: &str, file: &str) -> serde_json::Value {
    let mut child = Command::new(common::bin())
        .args(["session", file, "--database-url", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn morpholog session");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    drop(child.stdin.take());
    child.wait().unwrap();
    serde_json::from_str(&line).unwrap()
}

/// A fresh database for `tag`, dropped by the caller.
async fn scratch(tag: &str) -> (PgPool, String, String) {
    let base = morpholog_postgres::with_connection_defaults(&common::database_url());
    let name = format!("morpholog_cli_idc_{tag}_{}", std::process::id());
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
    (admin, name, url)
}

async fn drop_scratch(admin: &PgPool, name: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(admin)
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn init_closes_a_new_deployment_under_its_root() {
    let (admin, name, url) = scratch("new").await;
    let me = common::session_user(&url).await;
    let fixture = common::write_fixture("closed_e2e", PROGRAMME);
    let file = fixture.to_str().unwrap();

    let (status, _, open_stderr) = run(&["init", "--database-url", &url]);
    assert!(status.success(), "{open_stderr}");
    let (_, _, check_open) = run(&["check", file, "--database-url", &url, "--json"]);
    let (status, _, closing) = run(&[
        "init",
        "--skip-if-exists",
        "--database-url",
        &url,
        "--root-actor",
        "root_admin",
        "--root-login",
        &me,
    ]);
    let as_root = note(&url, file, "root_admin");
    let as_other = note(&url, file, "lapse_sweep");
    let ready = session_ready(&url, file);
    let (_, check_json, check_closed) = run(&["check", file, "--database-url", &url, "--json"]);
    drop_scratch(&admin, &name).await;

    assert!(
        open_stderr.contains("this deployment is open"),
        "a plain init says it is open: {open_stderr}"
    );
    assert!(
        check_open.contains("this deployment is open"),
        "{check_open}"
    );
    assert!(status.success(), "{closing}");
    assert!(!closing.contains("this deployment is open"), "{closing}");
    assert!(
        as_root.0.success(),
        "the root's login proposes as the root: {}",
        as_root.2
    );
    assert_eq!(as_other.0.code(), Some(1), "{}", as_other.2);
    let refusal: serde_json::Value = serde_json::from_str(&as_other.1).unwrap();
    assert_eq!(refusal["code"], "actor_assertion_unauthorised", "{refusal}");
    assert_eq!(ready["identity"], "closed", "{ready}");
    let report: serde_json::Value = serde_json::from_str(&check_json).unwrap();
    assert_eq!(report["identity"], "closed", "{report}");
    assert!(
        !check_closed.contains("this deployment is open"),
        "{check_closed}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn closing_a_deployment_with_history_lists_the_actors_no_grant_names() {
    let (admin, name, url) = scratch("history").await;
    let me = common::session_user(&url).await;
    let fixture = common::write_fixture("closed_e2e", PROGRAMME);
    let file = fixture.to_str().unwrap();
    let close = |confirm: bool| {
        let mut args = vec![
            "init",
            "--skip-if-exists",
            "--database-url",
            &url,
            "--root-actor",
            "root_admin",
            "--root-login",
            &me,
        ];
        if confirm {
            args.push("--confirm");
        }
        run(&args)
    };

    let (status, _, stderr) = run(&["init", "--database-url", &url]);
    assert!(status.success(), "{stderr}");
    assert!(note(&url, file, "alice").0.success());
    assert!(note(&url, file, "root_admin").0.success());
    let refused = close(false);
    let still_open = session_ready(&url, file);
    let confirmed = close(true);
    let closed = session_ready(&url, file);
    let alice = note(&url, file, "alice");
    drop_scratch(&admin, &name).await;

    assert_eq!(refused.0.code(), Some(1), "{}", refused.2);
    assert!(
        refused.2.contains("alice") && !refused.2.contains("root_admin"),
        "the recorded actors without a grant, the root aside: {}",
        refused.2
    );
    assert_eq!(
        still_open["identity"], "open",
        "a refused close changes nothing"
    );
    assert!(confirmed.0.success(), "{}", confirmed.2);
    assert_eq!(closed["identity"], "closed");
    assert_eq!(
        alice.0.code(),
        Some(1),
        "alice has no grant now: {}",
        alice.2
    );
}
