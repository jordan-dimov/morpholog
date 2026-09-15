//! The claims key provides the orders the runtime pages and loads by.
//!
//! Re-keying claims on the argument digest took away the index behind
//! `ORDER BY predicate_name, arguments`, and the verify replay's keyset page
//! over that order became a per-page sort of a whole predicate - a
//! regression a review caught and every test passed through, because the
//! result is the same and only the plan changed. So the plan is asserted:
//! with the unordered scans disabled, an order the key supplies needs no
//! sort node, and one it does not supply cannot avoid one.
//!
//! The query texts are the production ones, copied: `sqlx::query!` takes a
//! literal, so there is no constant to share. A change to either query
//! belongs here too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::test_pool;

use morpholog_postgres::PgPool;
use sqlx::Row;

async fn node_types(pool: &PgPool, sql: &str) -> Vec<String> {
    let mut tx = pool.begin().await.unwrap();
    // Neither a sequential nor a bitmap scan yields rows in any order, so
    // with both off the planner's only path is an ordered index walk - and
    // a sort node then means no index supplies the order asked for.
    sqlx::raw_sql("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let plan: serde_json::Value =
        sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {sql}")))
            .fetch_one(&mut *tx)
            .await
            .unwrap()
            .get(0);
    tx.rollback().await.unwrap();
    let mut out = Vec::new();
    fn walk(node: &serde_json::Value, out: &mut Vec<String>) {
        if let Some(t) = node.get("Node Type").and_then(|t| t.as_str()) {
            out.push(t.to_string());
        }
        if let Some(children) = node.get("Plans").and_then(|p| p.as_array()) {
            for c in children {
                walk(c, out);
            }
        }
    }
    walk(&plan[0]["Plan"], &mut out);
    out
}

fn sorts(nodes: &[String]) -> bool {
    nodes.iter().any(|n| n.contains("Sort"))
}

#[tokio::test]
async fn the_verify_replay_page_is_an_index_walk() {
    let pool = test_pool().await;
    let page = node_types(
        &pool,
        "SELECT predicate_name, arguments, arguments_hash
         FROM morpholog.claims
         WHERE (predicate_name, arguments_hash) > ('P', '\\x00'::bytea)
         ORDER BY predicate_name, arguments_hash
         LIMIT 1024",
    )
    .await;
    assert!(
        !sorts(&page),
        "the keyset page must walk the key, not sort: {page:?}"
    );

    // The order the old key supplied, which the digest key cannot.
    let by_array = node_types(
        &pool,
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE (predicate_name, arguments) > ('P', '[]'::jsonb)
         ORDER BY predicate_name, arguments
         LIMIT 1024",
    )
    .await;
    assert!(
        sorts(&by_array),
        "anti-vacuity: an order the key does not supply must show a sort: {by_array:?}"
    );
}

#[tokio::test]
async fn the_scoped_load_orders_by_the_key() {
    let pool = test_pool().await;
    let load = node_types(
        &pool,
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE predicate_name = ANY('{P}')
         ORDER BY predicate_name, arguments_hash",
    )
    .await;
    assert!(
        !sorts(&load),
        "a one-predicate load in key order needs no sort: {load:?}"
    );
}
