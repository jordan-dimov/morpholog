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
        program.validated().unwrap(),
        SCHEMA,
        &canonical_hash(&program),
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

    // The tamperer did not even keep the column set: the inventory is
    // unreadable, but the verdict must still be structured tampering
    // (via the seal's own inventory and the hash comparison), never an
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
