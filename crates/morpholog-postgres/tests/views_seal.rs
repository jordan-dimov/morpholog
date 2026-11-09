//! The sealed view surface: the generated script records each view's
//! definition hash as PostgreSQL stores it, and `verify_views` compares
//! a live re-read against that seal - so an in-place redefinition, a
//! dropped view, or a deleted seal row is named, not silent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::canonical_hash;
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{PgPool, ViewsVerification, render_views, verify_views};

mod common;
use common::test_pool;

const SCHEMA: &str = "views_seal_test";

/// Drop and re-apply the generated view surface for the ledger example
/// into the test schema, sealing it in the same transaction.
async fn apply_views(pool: &PgPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"
    )))
    .execute(pool)
    .await
    .unwrap();
    let program = double_entry_ledger::program();
    let rendered = render_views(
        &program.validated().unwrap(),
        SCHEMA,
        &canonical_hash(&program),
        morpholog_postgres::DerivedSource::Cache,
        false,
    )
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(rendered.sql))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_applied_surface_verifies_intact() {
    let pool = test_pool().await;
    apply_views(&pool).await;

    match verify_views(&pool, SCHEMA).await.unwrap() {
        ViewsVerification::Intact { views_checked } => {
            // Every generated view plus the catalogue itself.
            assert!(views_checked >= 2, "got {views_checked}");
        }
        other => panic!("expected Intact, got {other:?}"),
    }
}

#[tokio::test]
async fn an_in_place_redefinition_is_named_mismatched() {
    let pool = test_pool().await;
    apply_views(&pool).await;

    // Same name, same columns, different body: the catalogue inventory
    // and the model hash cannot see this - only the seal can.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE OR REPLACE VIEW {SCHEMA}._morpholog_catalog AS \
         SELECT 'forged'::text AS programme_name, NULL::text AS model_hash, \
                NULL::text AS predicate_name, NULL::text AS view_name, \
                NULL::text AS kind WHERE false"
    )))
    .execute(&pool)
    .await
    .unwrap();

    match verify_views(&pool, SCHEMA).await.unwrap() {
        ViewsVerification::Tampered {
            mismatched,
            missing,
        } => {
            assert_eq!(mismatched, vec!["_morpholog_catalog".to_string()]);
            assert!(missing.is_empty(), "got missing: {missing:?}");
        }
        other => panic!("expected Tampered, got {other:?}"),
    }
}

#[tokio::test]
async fn a_catalogue_redefined_with_different_columns_is_still_named() {
    let pool = test_pool().await;
    apply_views(&pool).await;

    // The tamperer changed the columns too, so the inventory is
    // unreadable. The verdict must still be tampering, not an
    // operational error.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP VIEW {SCHEMA}._morpholog_catalog; \
         CREATE VIEW {SCHEMA}._morpholog_catalog AS SELECT 'forged'::text AS not_view_name"
    )))
    .execute(&pool)
    .await
    .unwrap();

    match verify_views(&pool, SCHEMA).await.unwrap() {
        ViewsVerification::Tampered { mismatched, .. } => {
            assert!(
                mismatched.contains(&"_morpholog_catalog".to_string()),
                "got mismatched: {mismatched:?}"
            );
        }
        other => panic!("expected Tampered, got {other:?}"),
    }
}

#[tokio::test]
async fn a_dropped_view_is_named_missing() {
    let pool = test_pool().await;
    apply_views(&pool).await;

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP VIEW {SCHEMA}.journal_entry"
    )))
    .execute(&pool)
    .await
    .unwrap();

    match verify_views(&pool, SCHEMA).await.unwrap() {
        ViewsVerification::Tampered { missing, .. } => {
            assert!(
                missing.contains(&"journal_entry".to_string()),
                "got missing: {missing:?}"
            );
        }
        other => panic!("expected Tampered, got {other:?}"),
    }
}

#[tokio::test]
async fn a_deleted_seal_row_is_named_missing_not_hidden() {
    let pool = test_pool().await;
    apply_views(&pool).await;

    // Deleting the seal row does not unlist the view: the catalogue
    // still intends it, so the cross-check names it.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {SCHEMA}._morpholog_view_defs WHERE view_name = 'journal_entry'"
    )))
    .execute(&pool)
    .await
    .unwrap();

    match verify_views(&pool, SCHEMA).await.unwrap() {
        ViewsVerification::Tampered { missing, .. } => {
            assert!(
                missing.contains(&"journal_entry".to_string()),
                "got missing: {missing:?}"
            );
        }
        other => panic!("expected Tampered, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unsealed_schema_reports_not_sealed() {
    let pool = test_pool().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {SCHEMA}")))
        .execute(&pool)
        .await
        .unwrap();

    match verify_views(&pool, SCHEMA).await.unwrap() {
        ViewsVerification::NotSealed => {}
        other => panic!("expected NotSealed, got {other:?}"),
    }
}

/// The views the catalogue does not name: the query the guide gives for
/// finding what a regenerated script left behind.
async fn uncatalogued(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT table_name::text FROM information_schema.views
         WHERE table_schema = '{SCHEMA}' AND table_name <> '_morpholog_catalog'
           AND table_name NOT IN (SELECT view_name FROM {SCHEMA}._morpholog_catalog)
         ORDER BY table_name"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A limit, pinned so it is not mistaken for a guarantee. The script
/// never drops a view, so history views applied once outlive a later
/// script generated without `--history`: they still answer queries, the
/// catalogue and the seal no longer name them, and the surface that is
/// named verifies intact. Nothing tracks them from then on, so the
/// operator drops them; the guide's query finds them.
#[tokio::test]
async fn a_history_view_outlives_the_script_that_stops_asking_for_it() {
    let pool = test_pool().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let program = double_entry_ledger::program();
    let script = |history: bool| {
        render_views(
            &program.validated().unwrap(),
            SCHEMA,
            &canonical_hash(&program),
            morpholog_postgres::DerivedSource::Cache,
            history,
        )
        .unwrap()
    };
    let with = script(true);
    let history: Vec<String> = with
        .surface
        .views
        .iter()
        .filter(|v| v.kind == "history")
        .map(|v| v.view.clone())
        .collect();
    assert!(!history.is_empty());
    sqlx::raw_sql(sqlx::AssertSqlSafe(with.sql))
        .execute(&pool)
        .await
        .unwrap();
    let ViewsVerification::Intact {
        views_checked: sealed_with,
    } = verify_views(&pool, SCHEMA).await.unwrap()
    else {
        panic!("the surface with history verifies");
    };
    assert!(uncatalogued(&pool).await.is_empty());

    sqlx::raw_sql(sqlx::AssertSqlSafe(script(false).sql))
        .execute(&pool)
        .await
        .unwrap();
    let (answers,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {SCHEMA}.journal_line_history"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(answers, 0, "the view is still there to be read");
    let (sealed,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {SCHEMA}._morpholog_view_defs WHERE view_name LIKE '%\\_history'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sealed, 0, "the seal no longer covers it");
    match verify_views(&pool, SCHEMA).await.unwrap() {
        ViewsVerification::Intact { views_checked } => assert_eq!(
            views_checked + u64::try_from(history.len()).unwrap(),
            sealed_with,
            "the verdict covers the named surface and says nothing of the rest"
        ),
        other => panic!("expected Intact, got {other:?}"),
    }
    assert_eq!(uncatalogued(&pool).await, {
        let mut left = history.clone();
        left.sort();
        left
    });

    for view in &history {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP VIEW {SCHEMA}.{view}")))
            .execute(&pool)
            .await
            .unwrap();
    }
    assert!(uncatalogued(&pool).await.is_empty());
    assert!(matches!(
        verify_views(&pool, SCHEMA).await.unwrap(),
        ViewsVerification::Intact { .. }
    ));
}
