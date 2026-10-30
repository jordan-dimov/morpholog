//! `migrate --prepare` does everything except the step that pauses
//! writers, and `--check` reports where that preparation stands and what
//! the cutover would cost, so an operator schedules the pause knowing it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::Command;

use serde_json::Value;
use sqlx::PgPool;

fn migrate(url: &str, extra: &[&str]) -> (bool, Value, String) {
    let output = Command::new(common::bin())
        .args(["migrate", "--database-url", url])
        .args(extra)
        .output()
        .expect("spawn morpholog binary");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report = serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("json: {stdout}"));
    (
        output.status.success(),
        report,
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_stops_before_the_cutover_and_check_reports_it() {
    let base = morpholog_postgres::with_default_user(&common::database_url());
    let name = format!("morpholog_cli_prepare_{}", std::process::id());
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
    let fixture = std::fs::read_to_string(
        common::repo_root().join("crates/morpholog-postgres/tests/fixtures/schema_v0.0.14.sql"),
    )
    .unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(fixture))
        .execute(&pool)
        .await
        .expect("the v0.0.14 schema");

    let (ok, pending, _) = migrate(&url, &["--check"]);
    assert!(!ok, "behind: {pending}");
    assert_eq!(
        pending["activation_preflight"]["state"], "pending",
        "{pending}"
    );
    assert_eq!(
        pending["activation_preflight"]["refusals"],
        Value::Array(vec![])
    );
    assert_eq!(pending["activation_preflight"]["legacy_rows"], 0);
    assert_eq!(pending["activation_preflight"]["legacy_rows_prepared"], 0);

    let (ok, prepared, stderr) = migrate(&url, &["--prepare"]);
    assert!(ok, "{stderr}");
    assert_eq!(prepared["recorded_version_after"], 26, "{prepared}");
    assert_eq!(prepared["pending"][0]["version"], 27, "{prepared}");
    assert_eq!(
        prepared["activation_preflight"]["state"], "ready",
        "{prepared}"
    );
    assert_eq!(
        prepared["activation_preflight"]["steps_done"],
        serde_json::json!(["columns", "constraints", "indexes", "objects", "frontier"])
    );

    let (ok, ready, _) = migrate(&url, &["--check"]);
    assert!(!ok, "still behind by the cutover: {ready}");
    assert_eq!(ready["activation_preflight"]["state"], "ready", "{ready}");

    let (ok, done, stderr) = migrate(&url, &[]);
    assert!(ok, "{stderr}");
    assert_eq!(done["applied"][0]["version"], 27, "{done}");
    assert!(done.get("activation_preflight").is_none(), "{done}");

    let (ok, current, _) = migrate(&url, &["--check"]);
    assert!(ok, "{current}");
    assert!(current.get("activation_preflight").is_none(), "{current}");
    let activations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(activations, 1);

    let conflict = Command::new(common::bin())
        .args(["migrate", "--database-url", &url, "--check", "--prepare"])
        .output()
        .unwrap();
    assert!(
        !conflict.status.success(),
        "the two flags exclude each other"
    );

    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
}
