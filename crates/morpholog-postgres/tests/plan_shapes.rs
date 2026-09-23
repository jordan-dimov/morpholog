//! The claims key provides the orders the runtime pages and loads by.
//!
//! A query paging in an order no index supplies still returns the right
//! rows, just with a sort of the whole predicate per page, so only the plan
//! shows the problem. With unordered scans disabled, an order the key
//! supplies needs no sort node, and one it does not supply cannot avoid one.
//!
//! The query texts are copies of the production ones (`sqlx::query!` takes
//! a literal), so a change to either belongs here too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::test_pool;

use morpholog_postgres::PgPool;
use sqlx::Row;

/// A query's plan, as its node types (with the index each scan uses) and
/// every index condition. With `args`, the query is prepared and the plan
/// is the generic one PostgreSQL may use for a cached prepared statement.
async fn plan(pool: &PgPool, sql: &str, args: Option<&str>) -> (Vec<String>, Vec<String>) {
    let mut tx = pool.begin().await.unwrap();
    // With sequential and bitmap scans off, the planner must walk an index
    // in order, so a sort node means no index supplies the order.
    sqlx::raw_sql("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let explain = match args {
        None => format!("EXPLAIN (FORMAT JSON) {sql}"),
        Some(args) => {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "SET LOCAL plan_cache_mode = force_generic_plan; PREPARE page AS {sql}"
            )))
            .execute(&mut *tx)
            .await
            .unwrap();
            format!("EXPLAIN (FORMAT JSON) EXECUTE page({args})")
        }
    };
    let plan: serde_json::Value = sqlx::query(sqlx::AssertSqlSafe(explain))
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get(0);
    if args.is_some() {
        // A prepared statement belongs to the session, not the transaction.
        sqlx::raw_sql("DEALLOCATE page")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
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

fn sorts(nodes: &[String]) -> bool {
    nodes.iter().any(|n| n.contains("Sort"))
}

#[tokio::test]
async fn the_verify_replay_page_is_an_index_walk() {
    let pool = test_pool().await;
    let (page, _) = plan(
        &pool,
        "SELECT predicate_name, arguments, arguments_hash
         FROM morpholog.claims
         WHERE (predicate_name, arguments_hash) > ('P', '\\x00'::bytea)
         ORDER BY predicate_name, arguments_hash
         LIMIT 1024",
        None,
    )
    .await;
    assert!(
        !sorts(&page),
        "the keyset page must walk the key, not sort: {page:?}"
    );

    // An order the digest key cannot supply.
    let (by_array, _) = plan(
        &pool,
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE (predicate_name, arguments) > ('P', '[]'::jsonb)
         ORDER BY predicate_name, arguments
         LIMIT 1024",
        None,
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
    let (load, _) = plan(
        &pool,
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE predicate_name = ANY('{P}')
         ORDER BY predicate_name, arguments_hash",
        None,
    )
    .await;
    assert!(
        !sorts(&load),
        "a one-predicate load in key order needs no sort: {load:?}"
    );
}

/// Every audit walk pages the `(committed_at, transition_id)` index in
/// order, and each bound is an index condition in the generic prepared
/// plan. Behind flags in one query the bounds would become a filter, so
/// each bound has its own query. The texts are hand-kept copies of
/// `audit_pages::replay_page` and `list_audit_rows_page`.
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
        let (nodes, conds) = plan(&pool, sql, Some(args)).await;
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

    // The single flagged query keeps the order but loses the bound to a
    // filter, so this test can tell the difference.
    let (_, conds) = plan(
        &pool,
        &format!("{replay} WHERE ($4 OR (committed_at, transition_id) > ($2, $3)) {order}"),
        Some(&format!("1024, {at}, {id}, false")),
    )
    .await;
    assert!(
        !conds.iter().any(|c| c.contains(after)),
        "a flagged bound must not show as an index condition: {conds:?}"
    );
}
