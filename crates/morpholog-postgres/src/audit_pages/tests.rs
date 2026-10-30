//! The paging contract at a chunk of two, so every walk crosses chunk
//! edges. Legacy rows, from before the canonical order, tie on their
//! timestamp and are inserted in reverse id order, so a query that lost
//! the `transition_id` tie-break fails here. Then one ordered batch,
//! stamped earlier than every legacy row and with its ordinals against
//! its ids, so a query that let the timestamp or the id outrank the order
//! or the ordinal fails too. Index scans are off, because the index alone
//! would return rows in key order and hide that loss;
//! `tests/plan_shapes.rs` pins the index use. `DATABASE_URL`-gated.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use jiff::Timestamp;
use uuid::Uuid;

use super::{AuditPages, ReplayPages};
use crate::PgPool;
use crate::audit::AuditPosition;

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
    // The trigger is off, so each row takes the order the test gives it.
    crate::testing::reset_before_activation(&pool)
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

/// A legacy row's position.
fn pos(second: i64, n: u128) -> AuditPosition {
    ordered_pos(0, 0, second, n)
}

fn ordered_pos(audit_order: u64, act_ordinal: u32, second: i64, n: u128) -> AuditPosition {
    AuditPosition {
        audit_order,
        act_ordinal,
        committed_at: at(second),
        transition_id: id(n),
    }
}

// Hand-inserted audit rows carry an attestation, as every row the
// adapter writes does. `None` is a legacy row; an order and an ordinal
// make an ordered one.
async fn insert(pool: &PgPool, n: u128, second: i64, order: Option<(i64, i32)>) {
    insert_with_arguments(pool, n, second, order, "[]").await;
}

async fn insert_with_arguments(
    pool: &PgPool,
    n: u128,
    second: i64,
    order: Option<(i64, i32)>,
    arguments: &str,
) {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            attestation, parameters, model_hash, semantics_version, drawn_subjects,
            committed_at, audit_order, act_ordinal, ordered_at, operations
         ) VALUES ($1, 'post', ($3::text)::jsonb,
                   '{\"type\":\"subject\",\"value\":\"pager\"}'::jsonb,
                   1, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb,
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"test\"}'::jsonb,
                   coalesce((SELECT jsonb_agg('p' || i) FROM generate_series(1, jsonb_array_length(($3::text)::jsonb)) i), '[]'::jsonb),
                   'sha256:' || repeat('0', 64), 1, '[]'::jsonb, $2,
                   coalesce($4, 0), coalesce($5, 0),
                   CASE WHEN $4 IS NOT NULL THEN $2 END,
                   CASE WHEN $4 IS NOT NULL THEN '[]'::jsonb END)",
    )
    .bind(id(n))
    .bind(jiff_sqlx::ToSqlx::to_sqlx(at(second)))
    .bind(arguments)
    .bind(order.map(|o| o.0))
    .bind(order.map(|o| o.1))
    .execute(pool)
    .await
    .unwrap();
}

/// Six rows in replay order: legacy 1 at t1; legacy 2, 3, 4 tied at t2
/// but inserted 4, 3, 2; then one ordered batch at t0, act 0 being row 6
/// and act 1 row 5, inserted 5, 6.
async fn seeded() -> PgPool {
    let pool = pool().await;
    insert(&pool, 1, 1, None).await;
    for n in [4, 3, 2] {
        insert(&pool, n, 2, None).await;
    }
    insert(&pool, 5, 0, Some((1, 1))).await;
    insert(&pool, 6, 0, Some((1, 0))).await;
    pool
}

/// Every seeded row, in replay order.
const ALL: [u128; 6] = [1, 2, 3, 4, 6, 5];

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
    assert_eq!(seen, ids(&ALL));
    assert_eq!(
        audit_walk(&pool, AuditPages::with_chunk(None, 2)).await,
        ids(&ALL)
    );
}

#[tokio::test]
async fn a_horizon_excludes_every_row_at_or_after_it() {
    let pool = seeded().await;
    assert_eq!(
        audit_walk(&pool, AuditPages::with_chunk(Some(1), 2)).await,
        ids(&[1, 2, 3, 4]),
        "a legacy row is below every horizon"
    );
    assert_eq!(
        audit_walk(&pool, AuditPages::with_chunk(Some(2), 2)).await,
        ids(&ALL)
    );
}

#[tokio::test]
async fn a_walk_through_a_target_includes_it_and_nothing_after() {
    let pool = seeded().await;
    // Mid-chunk: without the bound the second page would hold 3 and 4.
    let (seen, _) = replay_walk(&pool, ReplayPages::with_chunk(Some(pos(2, 3)), 2)).await;
    assert_eq!(seen, ids(&[1, 2, 3]), "a target among tied timestamps");
    // The target is the first of three tied rows.
    let (seen, _) = replay_walk(&pool, ReplayPages::with_chunk(Some(pos(2, 2)), 2)).await;
    assert_eq!(seen, ids(&[1, 2]));
    let (seen, _) = replay_walk(
        &pool,
        ReplayPages::with_chunk(Some(ordered_pos(1, 0, 0, 6)), 2),
    )
    .await;
    assert_eq!(seen, ids(&[1, 2, 3, 4, 6]), "the batch's first act");
    let (seen, _) = replay_walk(
        &pool,
        ReplayPages::with_chunk(Some(ordered_pos(1, 1, 0, 5)), 2),
    )
    .await;
    assert_eq!(seen, ids(&ALL), "the last row");
}

#[tokio::test]
async fn a_page_ending_on_the_target_ends_the_walk_without_another_query() {
    let pool = seeded().await;
    // 4 is the last row of the second full page.
    let (seen, queries) = replay_walk(&pool, ReplayPages::with_chunk(Some(pos(2, 4)), 2)).await;
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
    insert(&pool, 7, 5, Some((2, 0))).await;
    assert!(pages.next(&mut conn).await.unwrap().is_empty());
    assert!(pages.keyset.done);
}

#[tokio::test]
async fn a_walk_resumed_after_a_row_starts_strictly_after_it() {
    let pool = seeded().await;
    assert_eq!(
        audit_walk(
            &pool,
            AuditPages::with_chunk(None, 2).after(Some(pos(2, 3)))
        )
        .await,
        ids(&[4, 6, 5])
    );
}

/// The filtered walk pages matching rows in replay order across chunk
/// boundaries, ties included, each once and none skipped: rows 1, 3, 4
/// and 6 name the subject, 3 and 4 tied at t2, chunk two.
#[tokio::test]
async fn a_filtered_walk_pages_matching_rows_in_order_across_chunks_and_ties() {
    let pool = pool().await;
    let names = r#"[{"type":"subject","value":"s"}]"#;
    insert_with_arguments(&pool, 1, 1, None, names).await;
    insert(&pool, 2, 2, None).await;
    insert_with_arguments(&pool, 4, 2, None, names).await;
    insert_with_arguments(&pool, 3, 2, None, names).await;
    insert(&pool, 5, 0, Some((1, 1))).await;
    insert_with_arguments(&pool, 6, 0, Some((1, 0)), names).await;
    let walked = audit_walk(&pool, AuditPages::with_chunk(None, 2).mentioning("s")).await;
    assert_eq!(walked, ids(&[1, 3, 4, 6]));
    let whole = audit_walk(&pool, AuditPages::with_chunk(None, 2)).await;
    assert_eq!(whole, ids(&ALL), "the unfiltered walk is untouched");
}

#[tokio::test]
async fn a_bounded_page_asks_only_for_what_it_needs_and_the_walk_resumes_after_it() {
    let pool = seeded().await;
    let mut conn = pool.acquire().await.unwrap();
    let mut pages = AuditPages::with_chunk(None, 4);
    let mut next = async |max| {
        let page = pages.next_up_to(&mut conn, max).await.unwrap();
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>()
    };
    assert_eq!(next(1).await, ids(&[1]));
    assert_eq!(next(10).await, ids(&[2, 3, 4, 6]), "capped at a chunk");
    assert_eq!(next(1).await, ids(&[5]));
    assert_eq!(next(1).await, ids(&[]), "a full page did not end the walk");
}

/// The order decides first, then the act's ordinal, then time, and the
/// id breaks a tie: the order the queries use. Legacy rows, order 0,
/// come first.
#[test]
fn a_position_orders_by_order_ordinal_time_then_id() {
    let mut positions = vec![
        ordered_pos(2, 0, 0, 1),
        pos(2, 3),
        ordered_pos(1, 1, 0, 2),
        pos(1, 9),
        ordered_pos(1, 0, 5, 8),
        pos(2, 1),
    ];
    positions.sort();
    assert_eq!(
        positions,
        [
            pos(1, 9),
            pos(2, 1),
            pos(2, 3),
            ordered_pos(1, 0, 5, 8),
            ordered_pos(1, 1, 0, 2),
            ordered_pos(2, 0, 0, 1),
        ]
    );
}
