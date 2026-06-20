//! The audit tail's read contract: keyset pages in `(committed_at,
//! transition_id)` order, a strictly-greater resume cursor, and the
//! start-time watermark that makes resume lossless - including the
//! race test that IS the contract's proof: an in-flight writer's row
//! sorts below rows a naive pager would already have emitted, so the
//! horizon must withhold it now and surface it next time, never lose
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{reset_db, test_pool};

use chrono::{DateTime, Utc};
use common::{dec, subj};
use morpholog_core::Program;
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, audit_cursor_for, audit_resume_watermark,
    list_audit_rows_page,
};
use morpholog_surface::parse_program;
use uuid::Uuid;

const FIXTURE: &str = r#"
program audit_tail_fixture

predicate Entry(entry_id: Subject, amount: Decimal)

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)
"#;

fn fixture() -> Program {
    let p = parse_program(FIXTURE).expect("parses");
    p.validate().expect("validates");
    p
}

async fn post(pool: &PgPool, p: &Program, entry: &str) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        p.transformation("post").unwrap(),
        vec![subj(entry), dec(1)],
        &p.invariants,
        &p.definitions,
    )
    .await
    .expect("post commits");
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => panic!("unexpected rejection: {reason}"),
    }
}

#[tokio::test]
async fn pages_are_ordered_and_the_cursor_is_strictly_greater() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;
    let t2 = post(&pool, &p, "e2").await;
    let t3 = post(&pool, &p, "e3").await;

    let mut conn = pool.acquire().await.unwrap();

    // Limit 2 yields the first two in commit order; resuming from the
    // second's cursor yields exactly the third - the cursor row
    // itself is excluded.
    let page = list_audit_rows_page(&mut conn, None, None, 2)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t1, t2]
    );
    let cursor = audit_cursor_for(&mut conn, t2).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), None, 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t3]
    );

    // Resuming from the last row is an empty page, not an error.
    let cursor = audit_cursor_for(&mut conn, t3).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), None, 10)
        .await
        .unwrap();
    assert!(page.is_empty());
}

#[tokio::test]
async fn an_unknown_cursor_is_an_error_never_a_silent_restart() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let unknown = Uuid::now_v7();
    let err = audit_cursor_for(&mut conn, unknown)
        .await
        .expect_err("an unknown transition id must error");
    assert!(
        matches!(err, PgError::TransitionNotFound(id) if id == unknown),
        "got {err:?}"
    );
}

// THE RACE TEST - the proof-in-code behind the lossless-resume
// guarantee. `committed_at` is the WRITER's transaction start, while
// visibility follows commit order: a writer that started before the
// reader's snapshot but commits after it leaves a row that sorts
// BELOW rows the reader emits. Without the horizon, a resume cursor
// would skip that row forever. With it, the row is withheld now and
// surfaced by the next invocation's fresh horizon - no loss, no skip.
#[tokio::test]
async fn the_watermark_withholds_an_in_flight_writers_row_instead_of_losing_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;

    // Writer A: an open transaction whose start time is pinned before
    // the horizon is computed. The hand-written audit row stands in
    // for a propose whose SERIALIZABLE transaction is still in
    // flight; committed_at takes the schema default, A's now() = A's
    // transaction start.
    let mut writer = pool.begin().await.unwrap();
    let (writer_start,): (DateTime<Utc>,) = sqlx::query_as("SELECT transaction_timestamp()")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents
         ) VALUES ($1, 'post', '[]'::jsonb,
                   '{\"type\":\"subject\",\"value\":\"in_flight\"}'::jsonb,
                   1, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *writer)
    .await
    .unwrap();

    // The horizon, computed while A is in flight, clamps at or below
    // A's start.
    let horizon = audit_resume_watermark(&pool).await.unwrap();
    assert!(
        horizon <= writer_start,
        "the horizon must trail the in-flight writer: {horizon} > {writer_start}"
    );

    // A commits AFTER the horizon was computed - the naive-pager
    // poison: its row's committed_at sorts at A's start, below
    // anything a horizon-free reader would now emit.
    writer.commit().await.unwrap();

    // First invocation: reads under the horizon. A's row is
    // withheld; t1 (committed long before) is emitted.
    let mut conn = pool.acquire().await.unwrap();
    let page = list_audit_rows_page(&mut conn, None, Some(horizon), 10)
        .await
        .unwrap();
    let emitted: Vec<Uuid> = page.iter().map(|r| r.transition_id).collect();
    assert_eq!(
        emitted,
        vec![t1],
        "the in-flight row is withheld, t1 emitted"
    );

    // Second invocation: a fresh horizon (no open transactions now)
    // surfaces A's row after the resume cursor. Nothing was lost.
    let fresh = audit_resume_watermark(&pool).await.unwrap();
    let cursor = audit_cursor_for(&mut conn, t1).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), Some(fresh), 10)
        .await
        .unwrap();
    assert_eq!(
        page.len(),
        1,
        "the withheld row surfaces under the next horizon"
    );
    assert_eq!(page[0].actor.as_str(), "in_flight");
}

#[tokio::test]
async fn with_no_open_transactions_the_watermark_emits_everything_committed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;
    let t2 = post(&pool, &p, "e2").await;

    let horizon = audit_resume_watermark(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let page = list_audit_rows_page(&mut conn, None, Some(horizon), 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t1, t2],
        "a quiescent database's whole history sits below the horizon"
    );
}
