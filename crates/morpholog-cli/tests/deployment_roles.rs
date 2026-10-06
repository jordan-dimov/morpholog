//! Deployment roles through the binary: `init` names a deployment's roles
//! and refuses roles another database holds, and `migrate` warns when a
//! deployment's recorded roles reach another database, also with nothing
//! to migrate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::Command;

use sqlx::PgPool;

fn cli(args: &[&str]) -> (std::process::ExitStatus, String, String) {
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

/// The same connection URL, pointing at another database.
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

async fn run(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

const DATABASES: [&str; 3] = [
    "morpholog_ci_cli_roles_a",
    "morpholog_ci_cli_roles_b",
    "morpholog_ci_cli_roles_c",
];
const ROLES: [&str; 2] = [
    "morpholog_ci_cli_roles_writer",
    "morpholog_ci_cli_roles_reader",
];

async fn clean(admin: &PgPool) {
    for db in DATABASES {
        run(admin, &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
    }
    for role in ROLES {
        run(admin, &format!("DROP ROLE IF EXISTS {role}")).await;
    }
}

#[test]
fn a_role_prefix_needs_the_floor_and_a_lawful_name() {
    let (status, _, stderr) = cli(&[
        "init",
        "--role-prefix",
        "acme_",
        "--database-url",
        "postgres:///nowhere",
    ]);
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--least-privilege"), "{stderr}");

    // Refused before connecting: the database named does not exist.
    let (status, _, stderr) = cli(&[
        "init",
        "--least-privilege",
        "--role-prefix",
        "pg_acme_",
        "--database-url",
        "postgres:///nowhere",
    ]);
    assert!(!status.success());
    assert!(stderr.contains("is not a role prefix"), "{stderr}");
}

#[tokio::test]
async fn init_refuses_held_roles_and_migrate_warns_about_shared_ones() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let base = morpholog_postgres::with_default_user(&base);
    let admin = PgPool::connect(&with_database(&base, "postgres"))
        .await
        .unwrap();
    clean(&admin).await;
    for db in DATABASES {
        run(&admin, &format!("CREATE DATABASE {db}")).await;
    }
    let [a, b, c] = DATABASES.map(|db| with_database(&base, db));
    let prefix = "morpholog_ci_cli_roles_";

    let (status, stdout, stderr) = cli(&[
        "init",
        "--least-privilege",
        "--role-prefix",
        prefix,
        "--database-url",
        &a,
    ]);
    assert!(status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["least_privilege"]["writer_role"], ROLES[0]);
    assert_eq!(report["least_privilege"]["reader_role"], ROLES[1]);
    assert!(!stderr.contains("warning:"), "{stderr}");

    // A second deployment cannot take the first's roles.
    let (status, _, stderr) = cli(&[
        "init",
        "--least-privilege",
        "--role-prefix",
        prefix,
        "--database-url",
        &b,
    ]);
    assert!(!status.success());
    assert!(
        stderr.contains("already exists") && stderr.contains(DATABASES[0]),
        "{stderr}"
    );

    // A deployment already sharing them, as one recorded before the check
    // can be: migrate warns, though it has nothing to apply.
    let (status, _, stderr) = cli(&["init", "--database-url", &c]);
    assert!(status.success(), "{stderr}");
    let pool_c = PgPool::connect(&c).await.unwrap();
    run(
        &pool_c,
        &format!(
            "INSERT INTO morpholog.deployment_roles (writer_role, reader_role) \
             VALUES ('{}', '{}'); \
             GRANT SELECT, INSERT ON morpholog.audit TO {}",
            ROLES[0], ROLES[1], ROLES[0]
        ),
    )
    .await;
    pool_c.close().await;
    let (status, stdout, stderr) = cli(&["migrate", "--database-url", &c]);
    assert!(status.success(), "{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["applied"], serde_json::json!([]));
    assert!(
        stderr.contains("warning:") && stderr.contains(DATABASES[0]),
        "{stderr}"
    );
    let (status, _, stderr) = cli(&["migrate", "--database-url", &a]);
    assert!(status.success(), "{stderr}");
    assert!(
        stderr.contains("warning:") && stderr.contains(DATABASES[2]),
        "A's migrate sees the sharing too: {stderr}"
    );

    clean(&admin).await;
}

/// `init --reset --least-privilege` binds the roles the database recorded
/// before the reset again. Another prefix is refused before anything is
/// dropped, and a database that recorded nothing still adopts nothing.
#[tokio::test]
async fn a_reset_binds_again_only_the_roles_the_database_recorded() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let base = morpholog_postgres::with_default_user(&base);
    let admin = PgPool::connect(&with_database(&base, "postgres"))
        .await
        .unwrap();
    let (kept, bare) = ("morpholog_ci_cli_reset_a", "morpholog_ci_cli_reset_b");
    let roles = [
        "morpholog_ci_cli_reset_writer",
        "morpholog_ci_cli_reset_reader",
        "morpholog_ci_cli_loose_writer",
    ];
    let clean = || async {
        for db in [kept, bare] {
            run(
                &admin,
                &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
            )
            .await;
        }
        for role in roles {
            run(&admin, &format!("DROP ROLE IF EXISTS {role}")).await;
        }
    };
    clean().await;
    for db in [kept, bare] {
        run(&admin, &format!("CREATE DATABASE {db}")).await;
    }
    let (kept_url, bare_url) = (with_database(&base, kept), with_database(&base, bare));
    let reset = |url: &str, prefix: &str| {
        cli(&[
            "init",
            "--reset",
            "--i-know-this-deletes-data",
            "--least-privilege",
            "--role-prefix",
            prefix,
            "--database-url",
            url,
        ])
    };
    let recorded = |url: String| async move {
        let pool = PgPool::connect(&url).await.unwrap();
        let row =
            sqlx::query_scalar::<_, String>("SELECT writer_role FROM morpholog.deployment_roles")
                .fetch_optional(&pool)
                .await
                .unwrap();
        let can_append = sqlx::query_scalar::<_, bool>(
            "SELECT has_table_privilege('morpholog_ci_cli_reset_writer', 'morpholog.audit', 'INSERT')",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or(false);
        pool.close().await;
        (row, can_append)
    };

    let (status, _, stderr) = cli(&[
        "init",
        "--least-privilege",
        "--role-prefix",
        "morpholog_ci_cli_reset_",
        "--database-url",
        &kept_url,
    ]);
    assert!(status.success(), "{stderr}");

    let (status, stdout, stderr) = reset(&kept_url, "morpholog_ci_cli_reset_");
    assert!(status.success(), "the same prefix binds again: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["least_privilege"]["writer_role"], roles[0]);
    assert_eq!(
        recorded(kept_url.clone()).await,
        (Some(roles[0].to_string()), true)
    );

    let (status, _, stderr) = reset(&kept_url, "morpholog_ci_cli_other_");
    assert!(!status.success());
    assert!(stderr.contains("Nothing was dropped"), "{stderr}");
    assert_eq!(
        recorded(kept_url.clone()).await,
        (Some(roles[0].to_string()), true),
        "the refused reset left the schema and its floor in place"
    );

    // A database that recorded nothing adopts nothing on a reset either.
    let (status, _, stderr) = cli(&["init", "--database-url", &bare_url]);
    assert!(status.success(), "{stderr}");
    run(&admin, &format!("CREATE ROLE {} NOLOGIN", roles[2])).await;
    let (status, _, stderr) = reset(&bare_url, "morpholog_ci_cli_loose_");
    assert!(!status.success());
    assert!(stderr.contains("already exists"), "{stderr}");

    clean().await;
}

/// A reset never recreates a recorded role that is gone: it refuses while
/// the record still names it, and drops nothing.
#[tokio::test]
async fn a_reset_never_recreates_a_missing_recorded_role() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let base = morpholog_postgres::with_default_user(&base);
    let admin = PgPool::connect(&with_database(&base, "postgres"))
        .await
        .unwrap();
    let db = "morpholog_ci_cli_gone";
    let (writer, reader) = (
        "morpholog_ci_cli_gone_writer",
        "morpholog_ci_cli_gone_reader",
    );
    let clean = || async {
        run(
            &admin,
            &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
        )
        .await;
        run(&admin, &format!("DROP ROLE IF EXISTS {writer}, {reader}")).await;
    };
    clean().await;
    run(&admin, &format!("CREATE DATABASE {db}")).await;
    let url = with_database(&base, db);
    let (status, _, stderr) = cli(&[
        "init",
        "--least-privilege",
        "--role-prefix",
        "morpholog_ci_cli_gone_",
        "--database-url",
        &url,
    ]);
    assert!(status.success(), "{stderr}");
    let pool = PgPool::connect(&url).await.unwrap();
    run(&pool, &format!("DROP OWNED BY {writer}")).await;
    run(&admin, &format!("DROP ROLE {writer}")).await;

    let (status, _, stderr) = cli(&[
        "init",
        "--reset",
        "--i-know-this-deletes-data",
        "--least-privilege",
        "--role-prefix",
        "morpholog_ci_cli_gone_",
        "--database-url",
        &url,
    ]);
    assert!(!status.success());
    assert!(
        stderr.contains("nothing was dropped") && stderr.contains("does not exist"),
        "{stderr}"
    );
    let still_recorded =
        sqlx::query_scalar::<_, String>("SELECT writer_role FROM morpholog.deployment_roles")
            .fetch_one(&pool)
            .await
            .expect("the schema and its record are still there");
    assert_eq!(still_recorded, writer);
    let recreated =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(writer)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!recreated);

    pool.close().await;
    clean().await;
}
