//! The upgrade path for an existing database.
//!
//! `morpholog init` is day-zero only: it never drops and never migrates, so
//! an existing deployment gets a new column by applying the numbered file in
//! `crates/morpholog-core/sql/migrations/`. That makes each of those files a
//! claim - "run this and your database matches the head schema" - and a
//! claim needs a check. Without one, a binary that writes a column an
//! operator's table lacks fails on the *refusal* path: the post-rollback
//! insert names a column that is not there, so a lawful rejection surfaces
//! as a database error.
//!
//! **Isolation matters here.** `reset_db` only truncates, so DDL against the
//! shared `morpholog` schema would leave every later test in the run against
//! a shape nobody intended - the failure mode this repo has already been
//! bitten by once. So these tests build their own scratch schema and drop it
//! again, and never touch `morpholog`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use sqlx::{PgPool, Row};

const WITNESS_MIGRATION: &str =
    include_str!("../../morpholog-core/sql/migrations/010_rejections_witness.sql");

/// Run one statement whose text this test owns. The scratch schema name is a
/// literal here, never external input.
async fn ddl(pool: &PgPool, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .map(|_| ())
}

/// The column shape of one table, as `(name, is_nullable, data_type)`.
async fn columns(pool: &PgPool, schema: &str, table: &str) -> Vec<(String, String, String)> {
    sqlx::query(
        "SELECT column_name, is_nullable, data_type
         FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = $2
         ORDER BY column_name",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("reading the column shape")
    .into_iter()
    .map(|r| {
        (
            r.get::<String, _>("column_name"),
            r.get::<String, _>("is_nullable"),
            r.get::<String, _>("data_type"),
        )
    })
    .collect()
}

/// Applying the witness migration to a pre-witness table yields exactly the
/// head schema's shape, and leaves the rows already there alone.
///
/// The migration is applied verbatim except for its schema name, rewritten to
/// the scratch schema. That substitution is the one thing this test does not
/// prove; the column, its nullability, the constraint and idempotence are all
/// the shipped file's own DDL.
#[tokio::test]
async fn the_witness_migration_brings_an_old_table_to_the_head_shape() {
    let pool = test_pool().await;
    let scratch = "morpholog_migration_probe";
    ddl(&pool, format!("DROP SCHEMA IF EXISTS {scratch} CASCADE"))
        .await
        .unwrap();
    ddl(&pool, format!("CREATE SCHEMA {scratch}"))
        .await
        .unwrap();

    // The pre-witness shape: the head table with the column removed, which is
    // exactly what an operator who has not migrated is running.
    ddl(
        &pool,
        // INCLUDING ALL so the copy carries NOT NULLs, defaults and checks -
        // a bare CREATE TABLE AS drops them, and the comparison below would
        // then pass on a table nobody runs.
        format!(
            "CREATE TABLE {scratch}.rejections
             (LIKE morpholog.rejections INCLUDING ALL)"
        ),
    )
    .await
    .unwrap();
    ddl(
        &pool,
        format!("ALTER TABLE {scratch}.rejections DROP COLUMN witness"),
    )
    .await
    .expect("the head schema must have the column for this test to mean anything");

    // A refusal recorded before the upgrade, which must survive it.
    ddl(
        &pool,
        format!(
            "INSERT INTO {scratch}.rejections
               (rejection_id, transformation_name, arguments, actor, kind, rule,
                invariant_version, reason, rejected_at)
             VALUES (gen_random_uuid(), 'post', '[]'::jsonb, '{{}}'::jsonb,
                     'invariant', 'entry_unique_by_entry_id', 1, 'historical', now())"
        ),
    )
    .await
    .expect("the old shape accepts an old row");

    let migration =
        WITNESS_MIGRATION.replace("morpholog.rejections", &format!("{scratch}.rejections"));
    // Twice: an operator who re-runs a migration must not be punished for it.
    for _ in 0..2 {
        ddl(&pool, migration.clone())
            .await
            .expect("the migration applies, and applies again");
    }

    assert_eq!(
        columns(&pool, scratch, "rejections").await,
        columns(&pool, "morpholog", "rejections").await,
        "after migrating, the table must match the head schema column for column"
    );

    let surviving: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {scratch}.rejections WHERE witness IS NULL"
    )))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(
        surviving, 1,
        "the pre-upgrade row survives, with no witness invented for it"
    );

    // The constraint came with the column, not just the column.
    let empty = ddl(
        &pool,
        format!("UPDATE {scratch}.rejections SET witness = '[]'::jsonb"),
    )
    .await;
    assert!(
        empty.is_err(),
        "an empty witness must be unrepresentable after migrating too"
    );

    ddl(&pool, format!("DROP SCHEMA {scratch} CASCADE"))
        .await
        .unwrap();
}

/// An un-migrated database says so, and says what to do about it.
///
/// This is the failure an operator actually meets on upgrade, and its shape
/// is what makes it worth catching: commits keep working, so nothing looks
/// wrong until the first *refusal*, which then surfaces as an operational
/// database error rather than the lawful rejection it is. For an embedder
/// that means an exception where a decided outcome belongs.
///
/// Unlike the migration test above, this one needs the real `morpholog`
/// schema, because the point is what the production query does. It removes
/// the column and puts it back through the shipped migration - the same file
/// an operator would run - so the suite is left as it was found.
#[tokio::test]
async fn an_unmigrated_database_names_the_remedy() {
    let pool = test_pool().await;
    common::reset_db(&pool).await;

    ddl(
        &pool,
        "ALTER TABLE morpholog.rejections DROP COLUMN witness".to_string(),
    )
    .await
    .expect("dropping the column simulates a database from the previous release");

    let err = morpholog_postgres::list_rejection_rows(&pool, 10)
        .await
        .expect_err("a query naming the absent column must fail");

    // Restore before asserting, so a failed assertion cannot leave the
    // shared schema broken for every later test in the run.
    ddl(&pool, WITNESS_MIGRATION.to_string())
        .await
        .expect("the shipped migration restores the column");

    let rendered = err.to_string();
    assert!(
        matches!(err, morpholog_postgres::PgError::SchemaBehind { .. }),
        "an absent column must classify as a stale schema, got {err:?}"
    );
    assert!(
        rendered.contains("sql/migrations/"),
        "the message must name where the remedy lives, got: {rendered}"
    );

    // And the restore worked, so the suite is as it was.
    morpholog_postgres::list_rejection_rows(&pool, 10)
        .await
        .expect("the column is back");
}
