//! Migration 27 prepares in steps before the one transaction that pauses
//! writers. The steps are idempotent and repair what they own, the brief
//! ACCESS EXCLUSIVE lock they need is bounded, and the census the cutover
//! rests on fails closed: a session this login cannot see, or a prepared
//! transaction, refuses it, whether the transaction was prepared before
//! the census or during it.
//!
//! Schema-changing tests work in a database they create and drop.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{recreate_roles, reset_db, session_is_superuser, test_pool};
use morpholog_postgres::{
    BASELINE_LIMIT_BYTES, LockPolicy, MigrateMode, PgError, activation_preflight_within,
    apply_migrations_with, census, census_paused, migration_status,
};
use sqlx::PgPool;

const BASELINE_SCHEMA: &str = include_str!("fixtures/schema_v0.0.14.sql");

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

async fn ddl(pool: &PgPool, sql: String) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .unwrap();
}

/// A database at the baseline, its own, with its admin pool for the drop.
async fn baseline_database(tag: &str) -> Option<(PgPool, PgPool, String)> {
    let base = std::env::var("DATABASE_URL").ok()?;
    let name = format!("morpholog_prep_{tag}_{}", std::process::id());
    let admin = PgPool::connect(&morpholog_postgres::with_default_user(&with_database(
        &base, "postgres",
    )))
    .await
    .unwrap();
    ddl(
        &admin,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await;
    ddl(&admin, format!("CREATE DATABASE {name}")).await;
    let pool = PgPool::connect(&morpholog_postgres::with_default_user(&with_database(
        &base, &name,
    )))
    .await
    .unwrap();
    sqlx::raw_sql(BASELINE_SCHEMA)
        .execute(&pool)
        .await
        .expect("the baseline schema");
    Some((pool, admin, name))
}

async fn drop_database(pool: PgPool, admin: PgPool, name: &str) {
    pool.close().await;
    ddl(
        &admin,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await;
}

async fn steps_recorded(pool: &PgPool) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as(
        "SELECT step, detail FROM morpholog.migration_progress WHERE version = 27 ORDER BY step",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn index_valid(pool: &PgPool, name: &str) -> Option<bool> {
    sqlx::query_scalar(
        "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE c.relname = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn preparation_is_idempotent_and_repairs_what_it_owns() {
    let Some((pool, admin, name)) = baseline_database("repair").await else {
        return;
    };
    let prepared = apply_migrations_with(&pool, MigrateMode::Prepare, &LockPolicy::default())
        .await
        .unwrap();
    assert_eq!(prepared.recorded_version_after, 26, "{prepared:?}");
    assert_eq!(
        prepared
            .pending
            .iter()
            .map(|m| m.version)
            .collect::<Vec<_>>(),
        vec![27]
    );
    let preflight = prepared.activation_preflight.clone().expect("a preflight");
    assert_eq!(preflight.state, morpholog_postgres::PreparationState::Ready);
    assert_eq!(
        preflight.steps_done,
        ["columns", "constraints", "indexes", "objects"]
    );
    assert!(preflight.refusals.is_empty(), "{preflight:?}");
    assert_eq!(index_valid(&pool, "audit_canonical").await, Some(true));

    // A build that died leaves an invalid index; a hand change drops a
    // function and forgets two steps. Rerunning repairs all of it.
    ddl(
        &pool,
        "UPDATE pg_index SET indisvalid = false
         WHERE indexrelid = 'morpholog.audit_canonical'::regclass"
            .to_string(),
    )
    .await;
    ddl(
        &pool,
        "DROP FUNCTION morpholog.audit_watermark()".to_string(),
    )
    .await;
    ddl(
        &pool,
        "DELETE FROM morpholog.migration_progress WHERE step IN ('indexes', 'objects')".to_string(),
    )
    .await;
    let again = apply_migrations_with(&pool, MigrateMode::Prepare, &LockPolicy::default())
        .await
        .unwrap();
    assert!(
        again.applied.is_empty(),
        "nothing plain left to apply: {again:?}"
    );
    assert_eq!(index_valid(&pool, "audit_canonical").await, Some(true));
    let recorded = steps_recorded(&pool).await;
    assert_eq!(recorded.len(), 4, "{recorded:?}");
    let indexes = &recorded.iter().find(|(s, _)| s == "indexes").unwrap().1;
    assert_eq!(indexes["rebuilt"], serde_json::json!(["audit_canonical"]));
    let watermark: bool =
        sqlx::query_scalar("SELECT to_regprocedure('morpholog.audit_watermark()') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(watermark, "the dropped function is back");

    // The cutover records the migration, writes the activation record and
    // forgets the steps.
    let done = apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
        .await
        .unwrap();
    assert_eq!(
        done.applied.iter().map(|m| m.version).collect::<Vec<_>>(),
        vec![27]
    );
    assert!(done.activation_preflight.is_none());
    assert!(migration_status(&pool).await.unwrap().is_current());
    let activations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(activations, 1);
    assert!(steps_recorded(&pool).await.is_empty());
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn the_brief_lock_is_bounded_and_never_queues() {
    let Some((pool, admin, name)) = baseline_database("lock").await else {
        return;
    };
    let tight = LockPolicy {
        timeout: Duration::from_millis(100),
        attempts: 2,
        backoff: Duration::from_millis(50),
        jitter: Duration::from_millis(1),
        cap: Duration::from_millis(100),
    };
    // A reader holds ACCESS SHARE on `audit` for longer than the policy
    // allows: the run stops, names the statement, and changed nothing.
    let mut holder = pool.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN; SELECT 1 FROM morpholog.audit LIMIT 1;")
        .execute(&mut *holder)
        .await
        .unwrap();
    let refused = apply_migrations_with(&pool, MigrateMode::Prepare, &tight).await;
    match refused {
        Err(PgError::LockNotAcquired {
            statement,
            attempts,
        }) => {
            assert!(
                statement.starts_with("ALTER TABLE morpholog.audit"),
                "{statement}"
            );
            assert_eq!(attempts, 2);
        }
        other => panic!("expected the bounded lock refusal, got {other:?}"),
    }
    let columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_attribute WHERE attrelid = 'morpholog.audit'::regclass
         AND attname IN ('audit_order', 'operations') AND NOT attisdropped",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(columns, 0, "no column was added");
    sqlx::raw_sql("COMMIT").execute(&mut *holder).await.unwrap();

    // A reader that lets go within the policy's attempts: the step waits
    // it out and records how many tries it took.
    let patient = LockPolicy {
        attempts: 20,
        ..tight.clone()
    };
    let short_pool = pool.clone();
    let short = tokio::spawn(async move {
        let mut conn = short_pool.acquire().await.unwrap();
        sqlx::raw_sql(
            "BEGIN; SELECT 1 FROM morpholog.audit LIMIT 1; SELECT pg_sleep(0.5); COMMIT;",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let prepared = apply_migrations_with(&pool, MigrateMode::Prepare, &patient)
        .await
        .unwrap();
    short.await.unwrap();
    assert_eq!(
        prepared.activation_preflight.unwrap().state,
        morpholog_postgres::PreparationState::Ready
    );
    let recorded = steps_recorded(&pool).await;
    let columns = &recorded.iter().find(|(s, _)| s == "columns").unwrap().1;
    assert!(
        columns["attempts"].as_u64().unwrap() >= 2,
        "the first attempt met the lock: {columns}"
    );
    // `close` waits for every connection to come back, the holder's too.
    drop(holder);
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn the_census_refuses_a_session_this_login_cannot_see() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role to SET ROLE");
        return;
    }
    let role = "mtest027_limited";
    let create = format!("CREATE ROLE {role} NOLOGIN");
    recreate_roles(&pool, &[role], &[create.as_str()]).await;
    // The test's own connection, idle, is a session the limited role
    // cannot see into.
    let _other = pool.acquire().await.unwrap();
    let url = std::env::var("DATABASE_URL").unwrap();
    let limited = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE {role}")))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&morpholog_postgres::with_default_user(&url))
        .await
        .unwrap();
    let seen_by_limited = census(&limited).await.unwrap();
    assert!(seen_by_limited.hidden_sessions >= 1, "{seen_by_limited:?}");
    assert!(
        seen_by_limited
            .refusals()
            .iter()
            .any(|r| r.contains("hidden from this login")),
        "{seen_by_limited:?}"
    );
    let seen_by_owner = census(&pool).await.unwrap();
    assert_eq!(seen_by_owner.hidden_sessions, 0, "{seen_by_owner:?}");
    limited.close().await;
    common::drop_roles_if_present(&pool, &[role]).await;
}

async fn prepared_transactions_allowed(pool: &PgPool) -> bool {
    let max: String = sqlx::query_scalar("SHOW max_prepared_transactions")
        .fetch_one(pool)
        .await
        .unwrap();
    if max != "0" {
        return true;
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "CI must allow prepared transactions (max_prepared_transactions > 0) so this test runs"
    );
    eprintln!("skipping: max_prepared_transactions is 0 on this cluster");
    false
}

#[tokio::test]
async fn the_census_refuses_a_prepared_transaction_before_or_during_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !prepared_transactions_allowed(&pool).await {
        return;
    }
    let clean = census(&pool).await.unwrap();
    assert_eq!(clean.prepared_transactions, 0, "{clean:?}");

    // Prepared before the census: absent from the sessions, counted among
    // the prepared transactions.
    let mut other = pool.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN; SELECT 1; PREPARE TRANSACTION 'mtest027_before';")
        .execute(&mut *other)
        .await
        .unwrap();
    let before = census(&pool).await.unwrap();
    assert_eq!(before.prepared_transactions, 1, "{before:?}");
    assert!(
        before
            .refusals()
            .iter()
            .any(|r| r.contains("prepared transaction")),
        "{before:?}"
    );
    sqlx::raw_sql("ROLLBACK PREPARED 'mtest027_before'")
        .execute(&mut *other)
        .await
        .unwrap();
    assert!(census(&pool).await.unwrap().refusals().is_empty());

    // Prepared during the census, between its two statements: the session
    // was in the first, and the transaction is in the second. Either way it
    // cannot slip past.
    let during_pool = pool.clone();
    let during = census_paused(&pool, async move {
        let mut conn = during_pool.acquire().await.unwrap();
        sqlx::raw_sql("BEGIN; SELECT 1; PREPARE TRANSACTION 'mtest027_during';")
            .execute(&mut *conn)
            .await
            .unwrap();
    })
    .await
    .unwrap();
    assert_eq!(during.prepared_transactions, 1, "{during:?}");
    assert!(!during.refusals().is_empty());
    sqlx::raw_sql("ROLLBACK PREPARED 'mtest027_during'")
        .execute(&mut *other)
        .await
        .unwrap();
    assert!(census(&pool).await.unwrap().refusals().is_empty());
}

#[tokio::test]
async fn the_preflight_names_the_baseline_ceiling() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    common::seed_claims(
        &pool,
        &[
            morpholog_core::ClaimInstance {
                predicate: "X".into(),
                args: vec![morpholog_test_support::subj("a")],
            },
            morpholog_core::ClaimInstance {
                predicate: "X".into(),
                args: vec![morpholog_test_support::subj("b")],
            },
        ],
    )
    .await;
    let roomy = activation_preflight_within(&pool, BASELINE_LIMIT_BYTES)
        .await
        .unwrap();
    assert_eq!(roomy.live_claims, 2);
    assert!(roomy.baseline_bytes > 0);
    assert!(
        !roomy.refusals.iter().any(|r| r.contains("byte limit")),
        "{roomy:?}"
    );
    let cramped = activation_preflight_within(&pool, 1).await.unwrap();
    assert!(
        cramped
            .refusals
            .iter()
            .any(|r| r.contains("over the 1 byte limit")),
        "{cramped:?}"
    );
}
