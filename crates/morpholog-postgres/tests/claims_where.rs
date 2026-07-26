//! Argument-level selection, evaluated in the database.
//!
//! The point of `list_claims_where` is that the comparison happens in
//! PostgreSQL, so a single-subject question stops paying for the whole
//! predicate. These tests exercise the SQL directly: a CLI-level test
//! would pass just as well if the filter ran after the rows crossed the
//! wire, which is the one thing worth proving here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{
    compiled, dec_str, expect_committed, propose_pg_with_test_actor, reset_db, subj, test_pool,
};

use morpholog_postgres::{PgPool, list_claims_where};
use serde_json::json;

/// Lines spread across more than one invoice, with an amount stored at a
/// scale the caller is unlikely to type back exactly.
///
/// Seeded through `propose` rather than by inserting rows: a claim
/// carries the transition that admitted it, so a hand-inserted row is
/// not a claim the runtime would ever have made.
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
        &[1],
        &[json!({"type": "subject", "value": "inv_1"})],
        &[false],
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "two lines belong to inv_1: {rows:?}");
}

#[tokio::test]
async fn a_decimal_matches_the_same_number_at_a_different_scale() {
    // The trap this closes: decimals are stored as strings to stay
    // exact, so 13.5 and 13.50 are equal numbers and different text.
    // Comparing the JSON would report no such row for a row that
    // exists - a filter answering "nothing here" about data that is.
    let pool = test_pool().await;
    seed(&pool).await;
    for typed in ["13.5", "13.50", "13.500"] {
        let rows = list_claims_where(
            &pool,
            "InvoiceLine",
            &[2],
            &[json!({"type": "decimal", "value": typed})],
            &[true],
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
        &[1, 2],
        &[
            json!({"type": "subject", "value": "inv_1"}),
            json!({"type": "decimal", "value": "13.5"}),
        ],
        &[false, true],
    )
    .await
    .unwrap();
    assert_eq!(both.len(), 1, "only line_3 is inv_1 AND 13.50");

    // Same two fields, values that never co-occur: conjunction, not
    // union - line_1 is inv_1 and line_2 is 12.50, but neither is both.
    let neither = list_claims_where(
        &pool,
        "InvoiceLine",
        &[1, 2],
        &[
            json!({"type": "subject", "value": "inv_1"}),
            json!({"type": "decimal", "value": "12.50"}),
        ],
        &[false, true],
    )
    .await
    .unwrap();
    assert!(neither.is_empty(), "got {neither:?}");
}

#[tokio::test]
async fn no_filters_means_every_row_not_none() {
    // An empty conjunction is true. `bool_and` over zero rows is NULL,
    // though, and `AND NULL` would hand a caller who asked for
    // everything an empty set - the wrong answer, silently.
    let pool = test_pool().await;
    seed(&pool).await;
    let rows = list_claims_where(&pool, "InvoiceLine", &[], &[], &[])
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
