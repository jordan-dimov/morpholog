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

// An activation's rows, inserted by hand: `name` is the control row's
// transformation, `operations` its payload. The trigger is off, so the
// row takes the order and ordinal given.
async fn insert_control(pool: &PgPool, n: u128, order: (i64, i32), name: &str, operations: &str) {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            attestation, parameters, model_hash, semantics_version, drawn_subjects,
            committed_at, audit_order, act_ordinal, ordered_at, operations
         ) VALUES ($1, $2, '[]'::jsonb,
                   '{\"type\":\"subject\",\"value\":\"migrator\"}'::jsonb,
                   1, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb,
                   '{\"mode\":\"migration\",\"authenticated_by\":\"test\",\"authenticated_by_oid\":10}'::jsonb,
                   '[]'::jsonb, 'sha256:' || repeat('0', 64), 1, '[]'::jsonb,
                   $3, $4, $5, $3, ($6::text)::jsonb)",
    )
    .bind(id(n))
    .bind(name)
    .bind(jiff_sqlx::ToSqlx::to_sqlx(at(0)))
    .bind(order.0)
    .bind(order.1)
    .bind(operations)
    .execute(pool)
    .await
    .unwrap();
}

/// A chunk's payload: `claims` near-identical insertions, so the stored
/// JSONB compresses to a small fraction of its text. Each names the
/// subject `s` in its arguments.
fn chunk_operations(claims: usize) -> String {
    let ops: Vec<String> = (0..claims)
        .map(|i| {
            format!(
                r#"{{"op":"ins","predicate":"Held","args":[{{"type":"subject","value":"s"}},{{"type":"decimal","value":"{}"}}],"asserted_in":"{}"}}"#,
                i % 7,
                id(1)
            )
        })
        .collect();
    format!("[{}]", ops.join(","))
}

/// The chronology the activation leaves: legacy rows 1 to 3, three
/// baseline chunks (11, 12, 13) and the record (14) at order 1, then
/// ordinary rows 21 and 22. Row 21 names the subject the chunks name.
async fn activated() -> PgPool {
    let pool = pool().await;
    insert(&pool, 1, 1, None).await;
    insert(&pool, 2, 2, None).await;
    insert(&pool, 3, 3, None).await;
    for (n, ordinal) in [(11, 0), (12, 1), (13, 2)] {
        insert_control(
            &pool,
            n,
            (1, ordinal),
            crate::activation::BASELINE,
            &chunk_operations(2_000),
        )
        .await;
    }
    insert_control(&pool, 14, (1, 3), crate::activation::ACTIVATE, "[]").await;
    insert_with_arguments(
        &pool,
        21,
        4,
        Some((2, 0)),
        r#"[{"type":"subject","value":"s"}]"#,
    )
    .await;
    insert(&pool, 22, 5, Some((3, 0))).await;
    pool
}

const ACTIVATED: [u128; 9] = [1, 2, 3, 11, 12, 13, 14, 21, 22];

/// The pages a walk yields, each as its ids.
async fn audit_pages(pool: &PgPool, mut pages: AuditPages) -> Vec<Vec<Uuid>> {
    let mut conn = pool.acquire().await.unwrap();
    let mut seen = Vec::new();
    loop {
        let page = pages.next(&mut conn).await.unwrap();
        if page.is_empty() {
            return seen;
        }
        seen.push(page.iter().map(|r| r.transition_id).collect());
    }
}

async fn replay_pages(pool: &PgPool, mut pages: ReplayPages) -> Vec<Vec<Uuid>> {
    let mut conn = pool.acquire().await.unwrap();
    let mut seen = Vec::new();
    loop {
        let page = pages.next(&mut conn).await.unwrap();
        if page.is_empty() {
            return seen;
        }
        seen.push(page.iter().map(|r| r.transition_id).collect());
    }
}

fn pages_of(groups: &[&[u128]]) -> Vec<Vec<Uuid>> {
    groups.iter().map(|g| ids(g)).collect()
}

/// A page ends before a second baseline chunk, whatever the row limit,
/// and the walk goes on past the short page: the first page crossing
/// from the legacy rows holds one chunk, then one chunk a page, then the
/// last chunk's page runs on through the record and the rows after it.
/// Both projections, each row once, in order.
#[tokio::test]
async fn a_page_holds_at_most_one_baseline_chunk_and_the_walk_loses_nothing() {
    let pool = activated().await;
    let expected = pages_of(&[&[1, 2, 3, 11], &[12], &[13, 14, 21, 22]]);
    assert_eq!(
        audit_pages(&pool, AuditPages::with_chunk(None, 1024)).await,
        expected,
        "every row fits a page; only the chunks cut it"
    );
    assert_eq!(
        replay_pages(&pool, ReplayPages::with_chunk(None, 1024)).await,
        expected
    );
    // At a row limit of two, the limit and the chunk cut take turns.
    let expected = pages_of(&[&[1, 2], &[3, 11], &[12], &[13, 14], &[21, 22]]);
    assert_eq!(
        audit_pages(&pool, AuditPages::with_chunk(None, 2)).await,
        expected
    );
    assert_eq!(
        replay_pages(&pool, ReplayPages::with_chunk(None, 2)).await,
        expected
    );
}

/// The cut does not depend on the chunks' stored size: the fixture's
/// chunks compress to under a tenth of their text, and still page one at
/// a time. A page bounded by stored bytes would hold them all.
#[tokio::test]
async fn compressible_chunks_still_page_one_at_a_time() {
    let pool = activated().await;
    let (stored, text): (i32, i32) = sqlx::query_as(
        "SELECT max(pg_column_size(operations)), min(octet_length(operations::text))
         FROM morpholog.audit WHERE transformation_name = $1",
    )
    .bind(crate::activation::BASELINE)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        stored * 10 < text,
        "the fixture must compress well: {stored} bytes stored of {text}"
    );
    let pages = audit_pages(&pool, AuditPages::with_chunk(None, 1024)).await;
    for page in &pages {
        let chunks = page
            .iter()
            .filter(|id| ids(&[11, 12, 13]).contains(id))
            .count();
        assert!(chunks <= 1, "a page with {chunks} chunks: {page:?}");
    }
    assert_eq!(pages.concat(), ids(&ACTIVATED));
}

/// A target inside the chunks ends the walk on the page that reaches it,
/// without another query; a horizon cuts the rows at and after it.
#[tokio::test]
async fn bounds_hold_across_the_chunks() {
    let pool = activated().await;
    let (seen, queries) = replay_walk(
        &pool,
        ReplayPages::with_chunk(Some(ordered_pos(1, 1, 0, 12)), 1024),
    )
    .await;
    assert_eq!(seen, ids(&[1, 2, 3, 11, 12]));
    assert_eq!(queries, 2);
    assert_eq!(
        audit_pages(&pool, AuditPages::with_chunk(Some(2), 1024)).await,
        pages_of(&[&[1, 2, 3, 11], &[12], &[13, 14]])
    );
    assert_eq!(
        audit_pages(
            &pool,
            AuditPages::with_chunk(None, 1024).after(Some(ordered_pos(1, 0, 0, 11)))
        )
        .await,
        pages_of(&[&[12], &[13, 14, 21, 22]])
    );
}

/// The whole-table read holds its contract across the activation: every
/// row, the chunks, the record and the rows after them.
#[tokio::test]
async fn the_whole_table_read_returns_every_row_across_the_chunks() {
    let pool = activated().await;
    let rows = crate::audit::list_audit_rows(&pool).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        ids(&ACTIVATED)
    );
}

/// A chunk names its subjects only in `operations`, which the subject
/// filter does not read, so a filtered walk never returns a chunk and
/// keeps its plain pages.
#[tokio::test]
async fn a_filtered_walk_never_returns_a_chunk() {
    let pool = activated().await;
    assert_eq!(
        audit_pages(&pool, AuditPages::with_chunk(None, 1024).mentioning("s")).await,
        pages_of(&[&[21]])
    );
}
