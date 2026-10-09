//! `migrate` warns when other logins are connected to a database with a
//! migration pending: a process on the old binary keeps writing until it
//! is restarted. A warning, never a refusal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::Command;

use sqlx::PgPool;

fn with_database(url: &str, name: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let swapped = match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{name}"),
        None => base.to_string(),
    };
    match query {
        Some(q) => format!("{swapped}?{q}"),
        None => swapped,
    }
}

fn migrate(url: &str) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(common::bin())
        .args(["migrate", "--database-url", url])
        .output()
        .expect("spawn morpholog binary");
    (
        output.status,
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn migrate_names_the_other_sessions_while_a_migration_is_pending() {
    let base = morpholog_postgres::with_default_user(&common::database_url());
    let name = format!("morpholog_cli_sessions_{}", std::process::id());
    let admin = PgPool::connect(&with_database(&base, "postgres"))
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
    let url = with_database(&base, &name);
    let fixture = std::fs::read_to_string(
        common::repo_root().join("crates/morpholog-postgres/tests/fixtures/schema_v0.0.14.sql"),
    )
    .unwrap();
    let other = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(fixture))
        .execute(&other)
        .await
        .expect("the v0.0.14 schema");
    // `other` keeps one session open on the old schema while the upgrade
    // runs, as a worker not yet restarted would.
    let role: String = sqlx::query_scalar("SELECT session_user::text")
        .fetch_one(&other)
        .await
        .unwrap();

    let (status, stdout, stderr) = migrate(&url);
    other.close().await;
    let (again_status, _, again_stderr) = migrate(&url);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();

    assert!(status.success(), "a warning never refuses: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["recorded_version_before"], 23, "{stdout}");
    assert_eq!(
        report["recorded_version_after"],
        morpholog_postgres::head_version(),
        "{stdout}"
    );
    assert!(
        stderr.contains("warning: other sessions are connected") && stderr.contains(&role),
        "the warning names the login: {stderr}"
    );
    assert!(again_status.success());
    assert!(
        !again_stderr.contains("other sessions"),
        "nothing pending, nobody else connected: {again_stderr}"
    );
}
