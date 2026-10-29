//! The audit tail's read contract: pages in the canonical order, a
//! resume cursor that excludes its own row, and a watermark that makes
//! resume lossless. A writer still in flight holds an order below rows
//! a later writer has already committed, so everything from its order on
//! must be withheld now and surfaced next time, never lost.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{drop_roles_if_present, recreate_roles, reset_db, session_is_superuser, test_pool};

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
        &common::pg_program(p.clone()),
        p.transformation("post").unwrap(),
        vec![subj(entry), dec(1)],
    )
    .await
    .expect("post commits");
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => panic!("unexpected rejection: {reason}"),
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

    // Limit 2 yields the first two; resuming from the second yields
    // only the third.
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

// The race behind lossless resume. Orders are drawn in one sequence but
// rows become visible in commit order. A writer that draws its order and
// is still open when a later writer commits leaves a gap below a row
// that could already be emitted. Without the horizon a resume cursor
// past the later row would skip the first forever; with it, both are
// withheld now and surfaced next time, in order.
#[tokio::test]
async fn the_watermark_withholds_an_in_flight_writers_row_instead_of_losing_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;

    // Writer A draws its order and stays open, a propose still in flight.
    let mut writer = pool.begin().await.unwrap();
    let a = Uuid::now_v7();
    common::insert_in_flight_audit_row(&mut writer, a).await;
    // Writer B starts later, draws the next order, and commits first.
    let t2 = post(&pool, &p, "e2").await;

    let horizon = audit_resume_watermark(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let page = list_audit_rows_page(&mut conn, None, Some(horizon), 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t1],
        "A's order and everything after it is withheld, B included"
    );

    writer.commit().await.unwrap();
    let fresh = audit_resume_watermark(&pool).await.unwrap();
    let cursor = audit_cursor_for(&mut conn, t1).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), Some(fresh), 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![a, t2],
        "both surface under the next horizon, in order"
    );
}

// The horizon reads no session view, so a reader that cannot see the
// writer's session still withholds its row. Visibility follows the
// current role, so a pool that sets an unprivileged role stands in for a
// second login.
#[tokio::test]
async fn a_reader_that_cannot_see_the_writer_still_withholds_its_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role to SET ROLE");
        return;
    }
    let roles = ["mtest625_reader"];
    recreate_roles(
        &pool,
        &roles,
        &[
            "CREATE ROLE mtest625_reader NOLOGIN",
            "GRANT USAGE ON SCHEMA morpholog TO mtest625_reader",
            "GRANT EXECUTE ON FUNCTION morpholog.audit_watermark() TO mtest625_reader",
        ],
    )
    .await;
    let p = fixture();
    post(&pool, &p, "e1").await;

    let mut writer = pool.begin().await.unwrap();
    common::insert_in_flight_audit_row(&mut writer, Uuid::now_v7()).await;
    let held = audit_resume_watermark(&pool).await.unwrap();

    let url = morpholog_postgres::with_default_user(&std::env::var("DATABASE_URL").unwrap());
    let reader = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET ROLE mtest625_reader").await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let hidden: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND query = '<insufficient privilege>'",
    )
    .fetch_one(&reader)
    .await
    .unwrap();
    let seen = audit_resume_watermark(&reader).await;
    reader.close().await;
    writer.rollback().await.unwrap();
    drop_roles_if_present(&pool, &roles).await;

    assert!(
        hidden >= 1,
        "the writer's session is hidden from the reader"
    );
    assert_eq!(seen.unwrap(), held, "the reader's horizon is the writer's");
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
