//! A deadlock is a retryable non-commit. Two transactions that take row
//! locks in opposite orders wait on each other; PostgreSQL aborts one
//! with SQLSTATE 40P01. The adapter reports that as
//! `PgError::DeadlockDetected`, nothing of the aborted batch is recorded,
//! and the same batch commits when submitted again. `DATABASE_URL`-gated.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{reset_db, seed_claims, session_is_superuser, test_pool};
use morpholog_core::{PreparedProgram, Subject, Transition};
use morpholog_postgres::{
    InvariantPlan, PgAtomicOutcome, PgError, PgPool, PgProgram, Proposal, propose_all_against_pg,
};
use morpholog_test_support::{claim_instance, subj};
use std::sync::Arc;
use std::time::Duration;

fn program() -> morpholog_core::Program {
    morpholog_surface::parse_program(
        "program deadlock
predicate X(k: Subject)
predicate Y(k: Subject)
invariant y_needs_x:
    Y(k) implies X(k)
transformation retract_x(k):
    require X(k)
    retract X(k)
",
    )
    .unwrap()
}

fn act(k: &str) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: "retract_x".into(),
        args: vec![subj(k)],
        actor: Subject::from("deadlock_test"),
    })
}

/// One programme per evaluator. The invariant is there so each really
/// takes its route: with none to check, both would be the compiled one.
fn routes() -> [PgProgram; 2] {
    let interpreted = PgProgram::interpreted(PreparedProgram::new(program()).unwrap());
    let compiled = PgProgram::new(PreparedProgram::new(program()).unwrap());
    assert!(matches!(
        interpreted.plan(),
        InvariantPlan::Interpreted { .. }
    ));
    assert!(matches!(compiled.plan(), InvariantPlan::Compiled));
    [interpreted, compiled]
}

fn claim_args(k: &str) -> serde_json::Value {
    serde_json::json!([{"type": "subject", "value": k}])
}

async fn lock_claim(conn: &mut sqlx::PgConnection, k: &str) {
    sqlx::query(
        "SELECT 1 FROM morpholog.claims
         WHERE predicate_name = 'X' AND arguments = $1 FOR UPDATE",
    )
    .bind(claim_args(k))
    .execute(conn)
    .await
    .unwrap();
}

/// Wait until some other session is blocked on `pid`'s locks.
async fn wait_until_blocked_by(pool: &PgPool, pid: i32) {
    for _ in 0..1000 {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND pg_blocking_pids(pid) @> ARRAY[$1::int]",
        )
        .bind(pid)
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the batch never blocked on the held claim");
}

async fn claims(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM morpholog.claims")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn audit_rows(pool: &PgPool) -> Vec<(Option<i64>, String)> {
    sqlx::query_as(
        "SELECT audit_order, transformation_name FROM morpholog.audit
         ORDER BY audit_order, act_ordinal",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_deadlocked_batch_is_reported_retryable_and_commits_when_resubmitted() {
    let pool = test_pool().await;
    for route in routes() {
        let route = Arc::new(route);
        reset_db(&pool).await;
        seed_claims(
            &pool,
            &[
                claim_instance("X", &[subj("k1")]),
                claim_instance("X", &[subj("k2")]),
            ],
        )
        .await;

        // The other party holds k1 and will ask for k2. Its deadlock
        // timer is lengthened where the role may set it, so the batch,
        // which starts waiting first anyway, is the one PostgreSQL aborts.
        let mut other = pool.acquire().await.unwrap();
        let other_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *other)
            .await
            .unwrap();
        sqlx::query("BEGIN").execute(&mut *other).await.unwrap();
        if session_is_superuser(&pool).await {
            sqlx::query("SET LOCAL deadlock_timeout = '10s'")
                .execute(&mut *other)
                .await
                .unwrap();
        }
        lock_claim(&mut other, "k1").await;

        // The batch retracts k2, then blocks retracting k1.
        let batch_pool = pool.clone();
        let batch_route = Arc::clone(&route);
        let batch = tokio::spawn(async move {
            propose_all_against_pg(&batch_pool, &batch_route, &[act("k2"), act("k1")]).await
        });
        wait_until_blocked_by(&pool, other_pid).await;

        // Closing the cycle: this waits until the batch is aborted.
        tokio::time::timeout(Duration::from_secs(60), lock_claim(&mut other, "k2"))
            .await
            .expect("the deadlock was not broken");
        let first = tokio::time::timeout(Duration::from_secs(60), batch)
            .await
            .expect("the batch did not return")
            .unwrap();
        assert!(
            matches!(first, Err(PgError::DeadlockDetected)),
            "the batch should report the deadlock: {first:?}"
        );
        sqlx::query("ROLLBACK").execute(&mut *other).await.unwrap();
        drop(other);

        // Nothing of the aborted attempt stands.
        assert_eq!(claims(&pool).await, 2);
        assert!(audit_rows(&pool).await.is_empty());

        // The same batch, submitted again, commits once.
        let again = propose_all_against_pg(&pool, &route, &[act("k2"), act("k1")])
            .await
            .unwrap();
        assert!(
            matches!(again, PgAtomicOutcome::Committed { .. }),
            "{again:?}"
        );
        assert_eq!(claims(&pool).await, 0);
        let rows = audit_rows(&pool).await;
        assert_eq!(rows.len(), 2, "one batch's worth of rows: {rows:?}");
        assert_eq!(rows[0].0, rows[1].0, "one order for the batch: {rows:?}");
    }
}
