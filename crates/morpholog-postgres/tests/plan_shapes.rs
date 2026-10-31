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
            .unwrap_or_else(|e| panic!("{e}: {sql}"));
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

/// Every audit walk pages the canonical-order index in order, and each
/// bound is an index condition in the generic prepared plan. Behind flags
/// in one query the bounds would become a filter, so each bound has its
/// own query. The unfiltered walks cut a page before a second baseline
/// chunk with a window count under the limit, so the window runs over at
/// most a page. The texts are hand-kept copies of
/// `audit_pages::replay_page`, `list_audit_rows_page` and
/// `list_audit_rows_page_mentioning`.
#[tokio::test]
async fn every_audit_walk_is_an_index_walk_with_its_bounds_as_index_conditions() {
    let pool = test_pool().await;
    let replay = "transition_id, transformation_name, asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal, operations";
    let full = "transition_id, transformation_name, arguments, actor, invariant_epoch, invariants_checked, asserted_claims, retracted_claims, emitted_intents, committed_at, attestation, parameters, model_hash, semantics_version, drawn_subjects, audit_order, act_ordinal, ordered_at, operations";
    let key = "(audit_order, act_ordinal, committed_at, transition_id)";
    let order = format!("ORDER BY {}", &key[1..key.len() - 1]);
    // One page: the rows after the bounds, cut before a second chunk.
    let paged = |cols: &str, bounds: &str, name: &str| {
        let bounds = if bounds.is_empty() {
            String::new()
        } else {
            format!("WHERE {bounds} ")
        };
        format!(
            "SELECT {cols} FROM (SELECT {cols}, sum((transformation_name = {name})::int) OVER w AS chunks \
             FROM morpholog.audit {bounds}WINDOW w AS ({order} ROWS UNBOUNDED PRECEDING) {order} LIMIT $1) s \
             WHERE chunks <= 1 {order}"
        )
    };
    let (at, id, n, k, h, name) = (
        "'2026-06-01T00:00:00Z'::timestamptz",
        "'01900000-0000-7000-8000-000000000001'::uuid",
        "'3'::bigint",
        "'0'::integer",
        "'5'::bigint",
        "'morpholog.baseline'::text",
    );
    let after = |o: &str, a: &str| format!("ROW{key} > ROW(${o}, ${a}, $2, $3)");
    let mentions_at = r#"jsonb_path_exists(jsonb_build_array(arguments, asserted_claims, retracted_claims, emitted_intents), '$.** ? (@.type == "subject" && @.value == $s)', $"#;
    let vars = r#"'{"s":"x"}'::jsonb"#;
    let cases: Vec<(String, String, Vec<String>)> = vec![
        (paged(replay, "", "$2"), format!("1024, {name}"), vec![]),
        (
            paged(replay, &format!("{key} > ($4, $5, $2, $3)"), "$6"),
            format!("1024, {at}, {id}, {n}, {k}, {name}"),
            vec![after("4", "5")],
        ),
        (
            paged(replay, &format!("{key} <= ($4, $5, $2, $3)"), "$6"),
            format!("1024, {at}, {id}, {n}, {k}, {name}"),
            vec![format!("ROW{key} <= ROW($4, $5, $2, $3)")],
        ),
        (
            paged(
                replay,
                &format!("{key} > ($6, $7, $2, $3) AND {key} <= ($8, $9, $4, $5)"),
                "$10",
            ),
            format!("1024, {at}, {id}, {at}, {id}, {n}, {k}, {n}, {k}, {name}"),
            vec![after("6", "7"), format!("ROW{key} <= ROW($8, $9, $4, $5)")],
        ),
        (paged(full, "", "$2"), format!("1024, {name}"), vec![]),
        (
            paged(full, &format!("{key} > ($4, $5, $2, $3)"), "$6"),
            format!("1024, {at}, {id}, {n}, {k}, {name}"),
            vec![after("4", "5")],
        ),
        (
            paged(full, "audit_order < $2", "$3"),
            format!("1024, {h}, {name}"),
            vec!["audit_order < $2".into()],
        ),
        (
            paged(
                full,
                &format!("{key} > ($5, $6, $2, $3) AND audit_order < $4"),
                "$7",
            ),
            format!("1024, {at}, {id}, {h}, {n}, {k}, {name}"),
            vec![after("5", "6"), "audit_order < $4".into()],
        ),
        // The `--mentions` statements: the same walk and bounds, the
        // subject test a filter on each row, never a second scan. A
        // baseline chunk's filtered columns are empty, so these pages
        // never hold one and keep the plain shape.
        (
            format!("SELECT {full} FROM morpholog.audit WHERE {mentions_at}2) {order} LIMIT $1"),
            format!("1024, {vars}"),
            vec![],
        ),
        (
            format!(
                "SELECT {full} FROM morpholog.audit WHERE {key} > ($5, $6, $2, $3) AND {mentions_at}4) {order} LIMIT $1"
            ),
            format!("1024, {at}, {id}, {vars}, {n}, {k}"),
            vec![after("5", "6")],
        ),
        (
            format!(
                "SELECT {full} FROM morpholog.audit WHERE audit_order < $2 AND {mentions_at}3) {order} LIMIT $1"
            ),
            format!("1024, {h}, {vars}"),
            vec!["audit_order < $2".into()],
        ),
        (
            format!(
                "SELECT {full} FROM morpholog.audit WHERE {key} > ($6, $7, $2, $3) AND audit_order < $4 AND {mentions_at}5) {order} LIMIT $1"
            ),
            format!("1024, {at}, {id}, {h}, {vars}, {n}, {k}"),
            vec![after("6", "7"), "audit_order < $4".into()],
        ),
    ];
    for (sql, args, bounds) in &cases {
        let (nodes, conds) = plan(&pool, sql, Some(args)).await;
        assert!(
            nodes.iter().any(|n| n.ends_with("audit_canonical")),
            "must walk audit_canonical: {nodes:?} for {sql}"
        );
        assert!(!sorts(&nodes), "must not sort: {nodes:?} for {sql}");
        for bound in bounds {
            assert!(
                conds.iter().any(|c| c.contains(bound.as_str())),
                "`{bound}` must be an index condition, got {conds:?} for {sql}"
            );
        }
        if sql.contains("WINDOW") {
            // The limit feeds the window, so the window never runs past a
            // page: in the plan's walk the window is the limit's child.
            let limit_over_window = nodes
                .windows(2)
                .any(|pair| pair[0] == "Limit" && pair[1] == "WindowAgg");
            assert!(
                limit_over_window,
                "the window must sit under the limit: {nodes:?} for {sql}"
            );
        }
    }

    // The single flagged query keeps the order but loses the bound to a
    // filter, so this test can tell the difference.
    let (_, conds) = plan(
        &pool,
        &format!("SELECT {replay} FROM morpholog.audit WHERE ($6 OR {key} > ($4, $5, $2, $3)) {order} LIMIT $1"),
        Some(&format!("1024, {at}, {id}, {n}, {k}, false")),
    )
    .await;
    assert!(
        !conds.iter().any(|c| c.contains(&after("4", "5"))),
        "a flagged bound must not show as an index condition: {conds:?}"
    );
}
