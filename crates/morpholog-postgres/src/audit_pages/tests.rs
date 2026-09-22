//! The paging contract at a chunk of two, so every walk crosses chunk
//! edges, over rows whose timestamps tie. The tied rows' ids sort in the
//! opposite order to their insertion, so a query that dropped the
//! `transition_id` tie-break would return them in insertion order and
//! fail here, rather than pass by accident. The connections run with
//! index scans off: the audit index returns tied rows in id order by
//! itself, which would hide a query that lost its tie-break. That the
//! index is used is pinned apart, in `tests/plan_shapes.rs`; here the
//! order must come from the query. `DATABASE_URL`-gated like every PG
//! suite.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use jiff::Timestamp;
use uuid::Uuid;

use super::{AuditPages, ReplayPages};
use crate::PgPool;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for the audit paging tests (e.g. postgres:///morpholog_dev)",
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::raw_sql(
                    "SET enable_indexscan = off; SET enable_indexonlyscan = off; \
                     SET enable_bitmapscan = off",
                )
                .execute(conn)
                .await?;
                Ok(())
            })
        })
        .connect(&crate::with_default_user(&url))
        .await
        .expect("failed to connect to PostgreSQL test database");
    sqlx::raw_sql(crate::testing::RESET_SQL)
        .execute(&pool)
        .await
        .expect("truncate");
    pool
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0000 + n)
}

fn at(second: i64) -> Timestamp {
    Timestamp::from_second(1_780_000_000 + second).unwrap()
}

// Hand-inserted audit rows carry an attestation, as every row the
// adapter writes does.
async fn insert(pool: &PgPool, transition_id: Uuid, committed_at: Timestamp) {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            attestation, parameters, committed_at
         ) VALUES ($1, 'post', '[]'::jsonb,
                   '{\"type\":\"subject\",\"value\":\"pager\"}'::jsonb,
                   1, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb,
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"test\"}'::jsonb,
                   '[]'::jsonb, $2)",
    )
    .bind(transition_id)
    .bind(jiff_sqlx::ToSqlx::to_sqlx(committed_at))
    .execute(pool)
    .await
    .unwrap();
}

/// Six rows in replay order: 1 at t1; 2, 3, 4 tied at t2 but inserted
/// 4, 3, 2; 5 at t3; 6 at t4.
async fn seeded() -> PgPool {
    let pool = pool().await;
    insert(&pool, id(1), at(1)).await;
    for n in [4, 3, 2] {
        insert(&pool, id(n), at(2)).await;
    }
    insert(&pool, id(5), at(3)).await;
    insert(&pool, id(6), at(4)).await;
    pool
}

fn ids(n: &[u128]) -> Vec<Uuid> {
    n.iter().map(|&n| id(n)).collect()
}

async fn replay_walk(pool: &PgPool, mut pages: ReplayPages) -> (Vec<Uuid>, usize) {
    let mut conn = pool.acquire().await.unwrap();
    let (mut seen, mut queries) = (Vec::new(), 0);
    loop {
        let done_before = pages.0.done;
        let page = pages.next(&mut conn).await.unwrap();
        if !done_before {
            queries += 1;
        }
        if page.is_empty() {
            break;
        }
        seen.extend(page.iter().map(|r| r.transition_id));
    }
    (seen, queries)
}

async fn audit_walk(pool: &PgPool, mut pages: AuditPages) -> Vec<Uuid> {
    let mut conn = pool.acquire().await.unwrap();
    let mut seen = Vec::new();
    loop {
        let page = pages.next(&mut conn).await.unwrap();
        if page.is_empty() {
            break;
        }
        seen.extend(page.iter().map(|r| r.transition_id));
    }
    seen
}

#[tokio::test]
async fn a_walk_to_the_end_yields_every_row_once_in_replay_order() {
    let pool = seeded().await;
    let (seen, _) = replay_walk(&pool, ReplayPages::with_chunk(None, 2)).await;
    assert_eq!(seen, ids(&[1, 2, 3, 4, 5, 6]));
    assert_eq!(
        audit_walk(&pool, AuditPages::with_chunk(None, 2)).await,
        ids(&[1, 2, 3, 4, 5, 6])
    );
}

#[tokio::test]
async fn a_horizon_excludes_every_row_at_or_after_it() {
    let pool = seeded().await;
    assert_eq!(
        audit_walk(&pool, AuditPages::with_chunk(Some(at(2)), 2)).await,
        ids(&[1])
    );
    assert_eq!(
        audit_walk(&pool, AuditPages::with_chunk(Some(at(3)), 2)).await,
        ids(&[1, 2, 3, 4])
    );
}

#[tokio::test]
async fn a_walk_through_a_target_includes_it_and_nothing_after() {
    let pool = seeded().await;
    // Mid-chunk: without the bound the second page would hold 3 and 4.
    let (seen, _) = replay_walk(&pool, ReplayPages::with_chunk(Some((at(2), id(3))), 2)).await;
    assert_eq!(seen, ids(&[1, 2, 3]), "a target among tied timestamps");
    // The target is the first of three tied rows.
    let (seen, _) = replay_walk(&pool, ReplayPages::with_chunk(Some((at(2), id(2))), 2)).await;
    assert_eq!(seen, ids(&[1, 2]));
    let (seen, _) = replay_walk(&pool, ReplayPages::with_chunk(Some((at(4), id(6))), 2)).await;
    assert_eq!(seen, ids(&[1, 2, 3, 4, 5, 6]), "the last row");
}

#[tokio::test]
async fn a_page_ending_on_the_target_ends_the_walk_without_another_query() {
    let pool = seeded().await;
    // 4 is the last row of the second full page.
    let (seen, queries) =
        replay_walk(&pool, ReplayPages::with_chunk(Some((at(2), id(4))), 2)).await;
    assert_eq!(seen, ids(&[1, 2, 3, 4]));
    assert_eq!(queries, 2, "two full pages, and no empty third query");
}

#[tokio::test]
async fn an_empty_log_yields_one_empty_page() {
    let pool = pool().await;
    let (seen, queries) = replay_walk(&pool, ReplayPages::with_chunk(None, 2)).await;
    assert!(seen.is_empty());
    assert_eq!(queries, 1);
    assert!(
        audit_walk(&pool, AuditPages::with_chunk(None, 2))
            .await
            .is_empty()
    );
}

/// Once exhausted a walk stays exhausted without asking again: rows
/// committed afterwards, past its cursor, are not picked up.
#[tokio::test]
async fn an_exhausted_walk_stays_empty_and_asks_nothing() {
    let pool = seeded().await;
    let mut conn = pool.acquire().await.unwrap();
    let mut pages = AuditPages::with_chunk(None, 4);
    while !pages.next(&mut conn).await.unwrap().is_empty() {}
    insert(&pool, id(7), at(5)).await;
    assert!(pages.next(&mut conn).await.unwrap().is_empty());
    assert!(pages.keyset.done);
}

#[tokio::test]
async fn a_walk_resumed_after_a_row_starts_strictly_after_it() {
    let pool = seeded().await;
    assert_eq!(
        audit_walk(
            &pool,
            AuditPages::with_chunk(None, 2).after(Some((at(2), id(3))))
        )
        .await,
        ids(&[4, 5, 6])
    );
}
