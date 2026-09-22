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

/// The generic plan of one prepared query - the plan a statement keeps
/// once it is cached - as its node types and every index condition.
async fn generic_plan(pool: &PgPool, sql: &str, args: &str) -> (Vec<String>, Vec<String>) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET LOCAL plan_cache_mode = force_generic_plan; PREPARE audit_page AS {sql}"
    )))
    .execute(&mut *tx)
    .await
    .unwrap();
    let plan: serde_json::Value = sqlx::query(sqlx::AssertSqlSafe(format!(
        "EXPLAIN (FORMAT JSON) EXECUTE audit_page({args})"
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap()
    .get(0);
    // A prepared statement belongs to the session, not the transaction.
    sqlx::raw_sql("DEALLOCATE audit_page")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let (mut nodes, mut conds) = (Vec::new(), Vec::new());
    fn walk(node: &serde_json::Value, nodes: &mut Vec<String>, conds: &mut Vec<String>) {
        if let Some(t) = node.get("Node Type").and_then(|t| t.as_str()) {
            let index = node
                .get("Index Name")
                .and_then(|i| i.as_str())
                .unwrap_or("");
            nodes.push(format!("{t} {index}").trim().to_string());
        }
        if let Some(c) = node.get("Index Cond").and_then(|c| c.as_str()) {
            conds.push(c.to_string());
        }
        for child in node
            .get("Plans")
            .and_then(|p| p.as_array())
            .into_iter()
            .flatten()
        {
            walk(child, nodes, conds);
        }
    }
    walk(&plan[0]["Plan"], &mut nodes, &mut conds);
    (nodes, conds)
}

/// Every audit walk pages the `(committed_at, transition_id)` index in
/// order, and each bound it reads to is an index condition in the plan
/// the statement keeps once cached. A single query with the bounds behind
/// flags turns them into a filter, which is why each bound has its own
/// query. The texts are the production ones, copied: `audit_pages` holds
/// the replay projection's, `list_audit_rows_page` the full row's.
#[tokio::test]
async fn every_audit_walk_is_an_index_walk_with_its_bounds_as_index_conditions() {
    let pool = test_pool().await;
    let replay = "SELECT transition_id, transformation_name, asserted_claims, retracted_claims, committed_at FROM morpholog.audit";
    let full = "SELECT transition_id, transformation_name, arguments, actor, invariant_epoch, invariants_checked, asserted_claims, retracted_claims, emitted_intents, committed_at, attestation, parameters FROM morpholog.audit";
    let order = "ORDER BY committed_at, transition_id LIMIT $1";
    let (at, id) = (
        "'2026-06-01T00:00:00Z'::timestamptz",
        "'01900000-0000-7000-8000-000000000001'::uuid",
    );
    let after = "ROW(committed_at, transition_id) > ROW($2, $3)";
    let cases: Vec<(String, String, Vec<&str>)> = vec![
        (format!("{replay} {order}"), "1024".into(), vec![]),
        (
            format!("{replay} WHERE (committed_at, transition_id) > ($2, $3) {order}"),
            format!("1024, {at}, {id}"),
            vec![after],
        ),
        (
            format!("{replay} WHERE (committed_at, transition_id) <= ($2, $3) {order}"),
            format!("1024, {at}, {id}"),
            vec!["ROW(committed_at, transition_id) <= ROW($2, $3)"],
        ),
        (
            format!(
                "{replay} WHERE (committed_at, transition_id) > ($2, $3) AND (committed_at, transition_id) <= ($4, $5) {order}"
            ),
            format!("1024, {at}, {id}, {at}, {id}"),
            vec![after, "ROW(committed_at, transition_id) <= ROW($4, $5)"],
        ),
        (format!("{full} {order}"), "1024".into(), vec![]),
        (
            format!("{full} WHERE (committed_at, transition_id) > ($2, $3) {order}"),
            format!("1024, {at}, {id}"),
            vec![after],
        ),
        (
            format!("{full} WHERE committed_at < $2 {order}"),
            format!("1024, {at}"),
            vec!["committed_at < $2"],
        ),
        (
            format!(
                "{full} WHERE (committed_at, transition_id) > ($2, $3) AND committed_at < $4 {order}"
            ),
            format!("1024, {at}, {id}, {at}"),
            vec![after, "committed_at < $4"],
        ),
    ];
    for (sql, args, bounds) in &cases {
        let (nodes, conds) = generic_plan(&pool, sql, args).await;
        assert!(
            nodes.iter().any(|n| n.ends_with("audit_committed_at")),
            "must walk audit_committed_at: {nodes:?} for {sql}"
        );
        assert!(!sorts(&nodes), "must not sort: {nodes:?} for {sql}");
        for bound in bounds {
            assert!(
                conds.iter().any(|c| c.contains(bound)),
                "`{bound}` must be an index condition, got {conds:?} for {sql}"
            );
        }
    }

    // Anti-vacuity: the flagged single query the literal bounds replace
    // keeps the order but loses the bound to a filter.
    let (_, conds) = generic_plan(
        &pool,
        &format!("{replay} WHERE ($4 OR (committed_at, transition_id) > ($2, $3)) {order}"),
        &format!("1024, {at}, {id}, false"),
    )
    .await;
    assert!(
        !conds.iter().any(|c| c.contains(after)),
        "a flagged bound must not show as an index condition: {conds:?}"
    );
}
