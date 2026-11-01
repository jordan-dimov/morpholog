//! Migration 27 prepares in steps before the one transaction that pauses
//! writers. The steps are idempotent and repair what they own, the brief
//! ACCESS EXCLUSIVE lock they need is bounded, and the census the cutover
//! rests on fails closed: a session this login cannot see, or a prepared
//! transaction, refuses it, whether the transaction was prepared before
//! the census or during it. An object of another shape under a name a
//! step owns is refused by name, never changed. The legacy Merkle
//! frontier built during preparation equals a scan from scratch, a
//! writer that commits late lands at its place in it, a saved frontier
//! that is malformed or whose rows changed is refused, and a second
//! runner is refused while one holds the lock.
//!
//! Schema-changing tests work in a database they create and drop.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{recreate_roles, reset_db, session_is_superuser, test_pool};
use morpholog_postgres::{
    LockPolicy, MIGRATION_LOCK_KEY, MigrateMode, PgError, activation_preflight,
    apply_migrations_with, census, census_paused, frontier_pass_with, legacy_root_full_scan,
    migration_status,
};
use sqlx::PgPool;
use uuid::Uuid;

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
        "SELECT step, detail FROM morpholog.migration_progress
         WHERE version = 27 AND step <> 'frontier_state' ORDER BY step",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A row as a binary before the canonical order wrote it; `committed_at`
/// defaults to the writer's transaction start, as it did.
async fn legacy_row<'e, E>(executor: E, k: &str, committed_at: Option<&str>) -> Uuid
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor, invariant_epoch,
            invariants_checked, asserted_claims, retracted_claims, emitted_intents,
            committed_at, attestation, parameters, model_hash, semantics_version,
            drawn_subjects)
         VALUES ($1, 'admit_x', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)),
            '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]',
            jsonb_build_array(jsonb_build_object('predicate', 'X', 'args',
                jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)))),
            '[]', '[]', coalesce($3::timestamptz, now()),
            '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[\"k\"]',
            'sha256:' || repeat('d', 64), 1, '[]')",
    )
    .bind(id)
    .bind(k)
    .bind(committed_at)
    .execute(executor)
    .await
    .unwrap();
    id
}

async fn frontier_detail(pool: &PgPool) -> Option<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT detail FROM morpholog.migration_progress WHERE version = 27 AND step = 'frontier_state'",
    )
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// The activation record's `(legacy_tree_size, legacy_root)`.
async fn recorded_legacy(pool: &PgPool) -> (u64, String) {
    let arguments: serde_json::Value = sqlx::query_scalar(
        "SELECT arguments FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (
        arguments[0]["value"].as_str().unwrap().parse().unwrap(),
        arguments[1]["value"].as_str().unwrap().to_string(),
    )
}

async fn prepare(pool: &PgPool) -> morpholog_postgres::MigrationReport {
    apply_migrations_with(pool, MigrateMode::Prepare, &LockPolicy::default())
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
        (27..=morpholog_postgres::head_version()).collect::<Vec<_>>()
    );
    let preflight = prepared.activation_preflight.clone().expect("a preflight");
    assert_eq!(preflight.state, morpholog_postgres::PreparationState::Ready);
    assert_eq!(
        preflight.steps_done,
        ["columns", "constraints", "indexes", "objects", "frontier"]
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
        "DROP FUNCTION morpholog.audit_watermark(), morpholog.audit_horizon()".to_string(),
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
    assert_eq!(recorded.len(), 5, "{recorded:?}");
    let indexes = &recorded.iter().find(|(s, _)| s == "indexes").unwrap().1;
    assert_eq!(indexes["rebuilt"], serde_json::json!(["audit_canonical"]));
    let functions: bool = sqlx::query_scalar(
        "SELECT to_regprocedure('morpholog.audit_watermark()') IS NOT NULL
            AND to_regprocedure('morpholog.audit_horizon()') IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(functions, "the dropped functions are back");

    // The cutover records the migration, writes the activation record and
    // forgets the steps.
    let done = apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
        .await
        .unwrap();
    assert_eq!(
        done.applied.iter().map(|m| m.version).collect::<Vec<_>>(),
        (27..=morpholog_postgres::head_version()).collect::<Vec<_>>()
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
async fn an_object_of_another_shape_under_a_migration_name_is_refused() {
    let Some((pool, admin, name)) = baseline_database("shape").await else {
        return;
    };
    // Someone's column, constraint or index under one of the migration's
    // names: the right kind of object with another definition. Each
    // refuses naming the object and what was found, the step that met it
    // is not recorded, and removing it lets the run go on.
    let intruders = [
        (
            "columns",
            "morpholog.audit.audit_order",
            "ALTER TABLE morpholog.audit ADD COLUMN audit_order bigint NOT NULL DEFAULT 7",
            "ALTER TABLE morpholog.audit DROP COLUMN audit_order",
            "DEFAULT 7",
        ),
        (
            "columns",
            "morpholog.audit.operations",
            "ALTER TABLE morpholog.audit ADD COLUMN operations text",
            "ALTER TABLE morpholog.audit DROP COLUMN operations",
            "`text`",
        ),
        (
            "constraints",
            "constraint audit_order_all_or_none",
            "ALTER TABLE morpholog.audit ADD CONSTRAINT audit_order_all_or_none CHECK (true)",
            "ALTER TABLE morpholog.audit DROP CONSTRAINT audit_order_all_or_none",
            "CHECK (true)",
        ),
        (
            "indexes",
            "index morpholog.audit_one_activation",
            "CREATE INDEX audit_one_activation ON morpholog.audit (transformation_name) \
             WHERE transformation_name = 'morpholog.activate'",
            "DROP INDEX morpholog.audit_one_activation",
            "ON morpholog.audit btree",
        ),
        (
            "indexes",
            "index morpholog.audit_canonical",
            "CREATE INDEX audit_canonical ON morpholog.audit (committed_at)",
            "DROP INDEX morpholog.audit_canonical",
            "btree (committed_at)",
        ),
    ];
    for (step, object, plant, remove, found) in intruders {
        ddl(&pool, plant.to_string()).await;
        let err = apply_migrations_with(&pool, MigrateMode::Prepare, &LockPolicy::default())
            .await
            .expect_err(object);
        match &err {
            PgError::ObjectDiffers { object: named, .. } => assert_eq!(named, object, "{err}"),
            other => panic!("{object}: {other}"),
        }
        assert!(err.to_string().contains(found), "{err}");
        assert!(
            !steps_recorded(&pool).await.iter().any(|(s, _)| s == step),
            "{step} is not recorded while it refuses"
        );
        ddl(&pool, remove.to_string()).await;
    }
    let prepared = apply_migrations_with(&pool, MigrateMode::Prepare, &LockPolicy::default())
        .await
        .unwrap();
    let preflight = prepared.activation_preflight.expect("a preflight");
    assert_eq!(preflight.state, morpholog_postgres::PreparationState::Ready);
    assert!(preflight.refusals.is_empty(), "{preflight:?}");
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn the_frontier_equals_a_full_scan_and_resumes_from_what_it_saved() {
    let Some((pool, admin, name)) = baseline_database("frontier").await else {
        return;
    };
    prepare(&pool).await;
    // A history written out of order, so the pages exercise the keyset.
    let mut order: Vec<i64> = (0..100).collect();
    for i in 0..100usize {
        order.swap(i, (i * 37) % 100);
    }
    for i in order {
        legacy_row(
            &pool,
            &format!("k{i}"),
            Some(&format!(
                "2026-01-01T00:00:{:02}.{:03}Z",
                i / 10,
                (i % 10) * 100
            )),
        )
        .await;
    }

    // A pass that dies after hashing its second page saved only its first;
    // the saved page is resumable, and the step is not done.
    ddl(
        &pool,
        "DELETE FROM morpholog.migration_progress WHERE step = 'frontier'".to_string(),
    )
    .await;
    let interrupted = frontier_pass_with(&pool, 32, Some(2)).await.unwrap();
    assert_eq!(interrupted.legacy_rows_prepared, 32, "{interrupted:?}");
    let partial = activation_preflight(&pool).await.unwrap();
    assert_eq!(
        (partial.state, partial.legacy_rows_prepared),
        (morpholog_postgres::PreparationState::Preparing, 32),
        "{partial:?}"
    );
    let resumed = frontier_pass_with(&pool, 32, None).await.unwrap();
    assert_eq!(resumed.legacy_rows_prepared, 100);
    assert_eq!(
        resumed.rows_added, 68,
        "the page that was lost is read again"
    );
    let (rows, root) = legacy_root_full_scan(&pool).await.unwrap();
    assert_eq!((rows, root.as_str()), (100, resumed.legacy_root.as_str()));

    let preflight = activation_preflight(&pool).await.unwrap();
    assert_eq!(preflight.legacy_rows, 100);
    assert_eq!(preflight.legacy_rows_prepared, 100);
    assert_eq!(
        preflight.estimated_writer_pause_ms, 0,
        "nothing legacy is left for the pause: {preflight:?}"
    );

    // From zero, the same root.
    ddl(
        &pool,
        "DELETE FROM morpholog.migration_progress WHERE step = 'frontier_state'".to_string(),
    )
    .await;
    let again = frontier_pass_with(&pool, 1024, None).await.unwrap();
    assert_eq!(again.rows_added, 100);
    assert_eq!(again.legacy_root, root);

    apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
        .await
        .unwrap();
    assert_eq!(recorded_legacy(&pool).await, (100, root));
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn a_writer_that_commits_late_lands_at_its_place() {
    let Some((pool, admin, name)) = baseline_database("late").await else {
        return;
    };
    prepare(&pool).await;
    for i in 0..10 {
        legacy_row(&pool, &format!("early{i}"), None).await;
    }
    // A writer on the old binary starts, writes, and holds: its row's
    // `committed_at` is its start, below everything written after it.
    let mut writer = pool.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN").execute(&mut *writer).await.unwrap();
    legacy_row(&mut *writer, "late", None).await;

    let first = frontier_pass_with(&pool, 1024, None).await.unwrap();
    assert_eq!(first.legacy_rows_prepared, 10, "{first:?}");
    for i in 0..5 {
        legacy_row(&pool, &format!("after{i}"), None).await;
    }
    let held = frontier_pass_with(&pool, 1024, None).await.unwrap();
    assert_eq!(
        held.rows_added, 0,
        "nothing above the writer's start is final while it holds: {held:?}"
    );
    sqlx::raw_sql("COMMIT").execute(&mut *writer).await.unwrap();
    drop(writer);
    let released = frontier_pass_with(&pool, 1024, None).await.unwrap();
    assert_eq!(released.legacy_rows_prepared, 16, "{released:?}");
    let (rows, root) = legacy_root_full_scan(&pool).await.unwrap();
    assert_eq!((rows, root.as_str()), (16, released.legacy_root.as_str()));

    apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
        .await
        .unwrap();
    assert_eq!(recorded_legacy(&pool).await, (16, root));
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn the_cutover_checks_the_rows_under_the_prepared_frontier() {
    let Some((pool, admin, name)) = baseline_database("prefix").await else {
        return;
    };
    prepare(&pool).await;
    for i in 0..50 {
        legacy_row(&pool, &format!("k{i}"), None).await;
    }
    frontier_pass_with(&pool, 1024, None).await.unwrap();
    // Someone with the privilege removes a row the frontier already holds.
    ddl(
        &pool,
        "DELETE FROM morpholog.audit WHERE transition_id = (
            SELECT transition_id FROM morpholog.audit ORDER BY committed_at, transition_id LIMIT 1)"
            .to_string(),
    )
    .await;
    let err = apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
        .await
        .unwrap_err();
    match &err {
        PgError::CutoverRefused { reasons } => {
            assert!(
                reasons
                    .iter()
                    .any(|r| r.contains("prepared frontier holds 50")),
                "{reasons:?}"
            );
        }
        other => panic!("{other}"),
    }
    assert!(!migration_status(&pool).await.unwrap().is_current());
    assert!(frontier_detail(&pool).await.is_some(), "refused, not reset");

    ddl(
        &pool,
        "DELETE FROM morpholog.migration_progress WHERE step = 'frontier_state'".to_string(),
    )
    .await;
    apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
        .await
        .unwrap();
    assert_eq!(recorded_legacy(&pool).await.0, 49);
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn a_malformed_saved_frontier_is_refused_not_reset() {
    let Some((pool, admin, name)) = baseline_database("malformed").await else {
        return;
    };
    prepare(&pool).await;
    for i in 0..5 {
        legacy_row(&pool, &format!("k{i}"), None).await;
    }
    frontier_pass_with(&pool, 1024, None).await.unwrap();
    ddl(
        &pool,
        "UPDATE morpholog.migration_progress SET detail = jsonb_set(detail, '{subtrees}', '[]')
         WHERE step = 'frontier_state'"
            .to_string(),
    )
    .await;
    let detail = frontier_detail(&pool).await;
    for result in [
        apply_migrations_with(&pool, MigrateMode::Prepare, &LockPolicy::default())
            .await
            .map(|_| ()),
        apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
            .await
            .map(|_| ()),
        activation_preflight(&pool).await.map(|_| ()),
    ] {
        match result.unwrap_err() {
            PgError::InvalidState(message) => {
                assert!(message.contains("saved frontier"), "{message}");
            }
            other => panic!("{other}"),
        }
    }
    assert_eq!(frontier_detail(&pool).await, detail, "untouched");
    assert!(!migration_status(&pool).await.unwrap().is_current());
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn a_runner_looks_again_once_it_holds_the_lock() {
    let Some((pool, admin, name)) = baseline_database("reread").await else {
        return;
    };
    legacy_row(&pool, "k", None).await;
    // Another runner completes the migration between this one's first
    // look and its lock. This one applies nothing and writes no second
    // record.
    let other = pool.clone();
    let report = morpholog_postgres::apply_migrations_paused(
        &pool,
        MigrateMode::Complete,
        &LockPolicy::default(),
        async move {
            apply_migrations_with(&other, MigrateMode::Complete, &LockPolicy::default())
                .await
                .unwrap();
        },
    )
    .await
    .unwrap();
    assert!(report.applied.is_empty(), "{report:?}");
    assert!(report.is_current(), "{report:?}");
    assert_eq!(recorded_legacy(&pool).await.0, 1);
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn a_second_runner_is_refused_while_one_holds_the_lock() {
    let Some((pool, admin, name)) = baseline_database("runner").await else {
        return;
    };
    let mut holder = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    let err = apply_migrations_with(&pool, MigrateMode::Prepare, &LockPolicy::default())
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::MigrationRunning), "{err}");
    let progress: bool =
        sqlx::query_scalar("SELECT to_regclass('morpholog.migration_progress') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!progress, "the refused runner changed nothing");
    // A session lock outlives the pooled connection; release it by hand.
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);
    assert_eq!(
        prepare(&pool).await.activation_preflight.unwrap().state,
        morpholog_postgres::PreparationState::Ready
    );
    drop_database(pool, admin, &name).await;
}

#[tokio::test]
async fn a_prepared_transaction_refuses_a_frontier_pass() {
    let Some((pool, admin, name)) = baseline_database("pass2pc").await else {
        return;
    };
    if !prepared_transactions_allowed(&pool).await {
        drop_database(pool, admin, &name).await;
        return;
    }
    prepare(&pool).await;
    legacy_row(&pool, "k", None).await;
    let detail = frontier_detail(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN; SELECT 1; PREPARE TRANSACTION 'mtest027_pass'")
        .execute(&mut *conn)
        .await
        .unwrap();
    let err = frontier_pass_with(&pool, 1024, None).await.unwrap_err();
    assert!(
        matches!(&err, PgError::CutoverRefused { reasons } if reasons.iter().any(|r| r.contains("prepared transaction"))),
        "{err}"
    );
    assert_eq!(frontier_detail(&pool).await, detail, "unchanged");
    sqlx::raw_sql("ROLLBACK PREPARED 'mtest027_pass'")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(
        frontier_pass_with(&pool, 1024, None)
            .await
            .unwrap()
            .legacy_rows_prepared,
        1
    );
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
async fn the_preflight_estimates_the_baseline_bytes() {
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
    let preflight = activation_preflight(&pool).await.unwrap();
    assert_eq!(preflight.live_claims, 2);
    assert!(preflight.baseline_bytes > 0);
    assert!(preflight.refusals.is_empty(), "{preflight:?}");
}

/// A database prepared by the build before the baseline chunks holds the
/// attestation constraint over the record alone. Preparation replaces
/// that one known shape and records it; any other shape under the name
/// is still refused.
#[tokio::test]
async fn the_constraint_of_the_build_before_the_chunks_is_replaced() {
    let Some((pool, admin, name)) = baseline_database("superseded").await else {
        return;
    };
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit ADD CONSTRAINT audit_activation_attested CHECK \
         ((transformation_name = 'morpholog.activate') = (attestation ->> 'mode' = 'migration'))"
            .to_string(),
    )
    .await;
    let prepared = prepare(&pool).await;
    let preflight = prepared.activation_preflight.expect("a preflight");
    assert_eq!(preflight.state, morpholog_postgres::PreparationState::Ready);
    let shape: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conrelid = 'morpholog.audit'::regclass AND conname = 'audit_activation_attested'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(shape.contains("'morpholog.baseline'::text"), "{shape}");
    let steps = steps_recorded(&pool).await;
    let (_, detail) = steps
        .iter()
        .find(|(step, _)| step == "constraints")
        .expect("the constraints step is recorded");
    assert_eq!(
        detail["replaced"],
        serde_json::json!(["audit_activation_attested"]),
        "{detail}"
    );
    drop_database(pool, admin, &name).await;
}

/// A legacy row stamped ahead of the server clock. The cutover moves the
/// ordering clock up to the latest legacy start first, so the activation
/// is placed at or after every legacy row whatever the clock says, and
/// the chronology the verifiers hold is met from the first ordered row.
#[tokio::test]
async fn the_cutover_places_the_activation_after_every_legacy_row() {
    let Some((pool, admin, name)) = baseline_database("floor").await else {
        return;
    };
    let ahead = jiff::Timestamp::now() + jiff::SignedDuration::from_hours(24);
    legacy_row(&pool, "a", Some(&ahead.to_string())).await;
    ddl(
        &pool,
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'X', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', 'a')),
                transition_id FROM morpholog.audit"
            .to_string(),
    )
    .await;
    apply_migrations_with(&pool, MigrateMode::Complete, &LockPolicy::default())
        .await
        .unwrap();
    let placed: Vec<String> = sqlx::query_scalar(
        "SELECT ordered_at::text FROM morpholog.audit
          WHERE transformation_name IN ('morpholog.baseline', 'morpholog.activate')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(placed.len(), 2, "{placed:?}");
    for at in &placed {
        let at: jiff::Timestamp = at.replace(' ', "T").parse().unwrap();
        assert!(
            at >= ahead,
            "the activation at {at} is before the legacy row at {ahead}"
        );
    }
    assert!(matches!(
        morpholog_postgres::verify_replay(&pool, &mut |_| {})
            .await
            .unwrap(),
        morpholog_postgres::VerifyOutcome::Consistent { .. }
    ));
    drop_database(pool, admin, &name).await;
}
