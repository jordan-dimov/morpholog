//! The least-privilege floor as PostgreSQL itself enforces it: after
//! provisioning, the writer role can read and append the audit log but
//! never rewrite it, and provisioning can safely run again.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::test_pool;

use morpholog_postgres::{WRITER_ROLE, provision_least_privilege};

#[tokio::test]
async fn the_floor_provisions_idempotently_and_audit_is_append_only() {
    let pool = test_pool().await;
    provision_least_privilege(&pool)
        .await
        .expect("first provisioning");
    provision_least_privilege(&pool)
        .await
        .expect("second provisioning: roles kept, grants reapplied");

    // One session for the whole probe, so SET ROLE governs the checks
    // and RESET ROLE runs before the connection returns to the pool.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE \"{WRITER_ROLE}\"")))
        .execute(&mut *conn)
        .await
        .expect("superuser test role assumes the writer");

    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM morpholog.audit")
        .fetch_one(&mut *conn)
        .await
        .expect("the writer reads the audit log");

    let denied = sqlx::raw_sql("DELETE FROM morpholog.audit")
        .execute(&mut *conn)
        .await
        .expect_err("the audit log is append-only even for the writer");
    let code = match &denied {
        sqlx::Error::Database(db) => db.code().map(|c| c.to_string()),
        other => panic!("expected a database permission error, got {other:?}"),
    };
    assert_eq!(code.as_deref(), Some("42501"), "insufficient_privilege");

    sqlx::raw_sql("RESET ROLE")
        .execute(&mut *conn)
        .await
        .unwrap();
}
