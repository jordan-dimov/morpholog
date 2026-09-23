//! Argument-level selection, evaluated in the database.
//!
//! `list_claims_where` compares in PostgreSQL, so a single-subject
//! question does not load the whole predicate. These tests call it
//! directly: a CLI test would also pass if the filter ran client-side.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{
    compiled, dec_str, expect_committed, propose_pg_with_test_actor, reset_db, subj, test_pool,
};

use morpholog_postgres::{ClaimFilter, PgPool, list_claims_where};
use serde_json::json;

/// Lines spread across more than one invoice, with an amount stored at a
/// scale the caller is unlikely to type back exactly.
///
/// Seeded through `propose`, so every claim carries the transition that
/// admitted it, as a real one would.
async fn seed(pool: &PgPool) {
    use morpholog_core::ir_builder::{assert_, params, predicate, program, transformation, var};
    reset_db(pool).await;
    let t = transformation(
        "note_line",
        params(&["line", "invoice", "net_gbp"]),
        vec![assert_(
            "InvoiceLine",
            vec![var("line"), var("invoice"), var("net_gbp")],
        )],
    );
    let prog = program("billing_probe")
        .predicates(vec![
            predicate("InvoiceLine")
                .subject("line")
                .subject("invoice")
                .decimal("net_gbp")
                .build(),
        ])
        .transformations(vec![t.clone()])
        .build();
    let compiled = compiled(prog);
    for (line, invoice, net) in [
        ("line_1", "inv_1", "11.50"),
        ("line_2", "inv_0", "12.50"),
        ("line_3", "inv_1", "13.50"),
    ] {
        let outcome = propose_pg_with_test_actor(
            pool,
            &compiled,
            &t,
            vec![subj(line), subj(invoice), dec_str(net)],
        )
        .await
        .unwrap();
        expect_committed(outcome);
    }
}

#[tokio::test]
async fn a_filter_returns_only_the_matching_rows() {
    let pool = test_pool().await;
    seed(&pool).await;
    let rows = list_claims_where(
        &pool,
        "InvoiceLine",
        &[ClaimFilter {
            position: 1,
            value: json!({"type": "subject", "value": "inv_1"}),
            numeric: false,
        }],
        3,
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "two lines belong to inv_1: {rows:?}");
}

#[tokio::test]
async fn a_decimal_matches_the_same_number_at_a_different_scale() {
    // Decimals are stored as strings to stay exact, so 13.5 and 13.50
    // are equal numbers but different text. Comparing the JSON would
    // miss a row that exists.
    let pool = test_pool().await;
    seed(&pool).await;
    for typed in ["13.5", "13.50", "13.500"] {
        let rows = list_claims_where(
            &pool,
            "InvoiceLine",
            &[ClaimFilter {
                position: 2,
                value: json!({"type": "decimal", "value": typed}),
                numeric: true,
            }],
            3,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "`{typed}` must find the 13.50 line");
    }
}

#[tokio::test]
async fn filters_are_conjunctive_and_a_miss_returns_nothing() {
    let pool = test_pool().await;
    seed(&pool).await;
    let both = list_claims_where(
        &pool,
        "InvoiceLine",
        &[
            ClaimFilter {
                position: 1,
                value: json!({"type": "subject", "value": "inv_1"}),
                numeric: false,
            },
            ClaimFilter {
                position: 2,
                value: json!({"type": "decimal", "value": "13.5"}),
                numeric: true,
            },
        ],
        3,
    )
    .await
    .unwrap();
    assert_eq!(both.len(), 1, "only line_3 is inv_1 AND 13.50");

    // Same two fields, values that never co-occur: conjunction, not
    // union - line_1 is inv_1 and line_2 is 12.50, but neither is both.
    let neither = list_claims_where(
        &pool,
        "InvoiceLine",
        &[
            ClaimFilter {
                position: 1,
                value: json!({"type": "subject", "value": "inv_1"}),
                numeric: false,
            },
            ClaimFilter {
                position: 2,
                value: json!({"type": "decimal", "value": "12.50"}),
                numeric: true,
            },
        ],
        3,
    )
    .await
    .unwrap();
    assert!(neither.is_empty(), "got {neither:?}");
}

#[tokio::test]
async fn no_filters_means_every_row_not_none() {
    // An empty conjunction is true. But `bool_and` over zero rows is
    // NULL, and `AND NULL` would silently return nothing.
    let pool = test_pool().await;
    seed(&pool).await;
    let rows = list_claims_where(&pool, "InvoiceLine", &[], 3)
        .await
        .unwrap();
    let all = morpholog_postgres::list_claims_for_predicates(&pool, &["InvoiceLine".to_string()])
        .await
        .unwrap();
    assert_eq!(
        rows, all,
        "an unfiltered call must agree with the unfiltered read"
    );
    assert!(!rows.is_empty(), "and the fixture is not empty");
}

#[tokio::test]
async fn a_row_of_the_wrong_arity_survives_the_filter() {
    // A row that does not match the programme's shape is a hard error,
    // but the decoder only sees rows it is given. Filtering on a position
    // a short row lacks yields NULL and would drop the row in SQL, so the
    // filtered call would succeed where the unfiltered one refuses.
    let pool = test_pool().await;
    seed(&pool).await;
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'InvoiceLine', '[{\"type\":\"subject\",\"value\":\"legacy\"}]'::jsonb, transition_id
         FROM morpholog.audit LIMIT 1",
    )
    .execute(&pool)
    .await
    .unwrap();

    let rows = list_claims_where(
        &pool,
        "InvoiceLine",
        &[ClaimFilter {
            position: 1,
            value: json!({"type": "subject", "value": "inv_1"}),
            numeric: false,
        }],
        3,
    )
    .await
    .unwrap();
    assert!(
        rows.iter().any(|r| r.args.len() != 3),
        "the short row must come back so the decoder can refuse it: {rows:?}"
    );
}
