//! Spike step 1: hand-written stage-1 violation SQL for the ledger's
//! `balanced_posted_entry` invariant, diffed against the kernel's verdict
//! on the same states. The query is the compiler's target output; if this
//! agreement doesn't hold by hand, no compiler is worth building.
//!
//! Candidate-state representation under test: writes applied first inside
//! an open transaction, so the claims table IS the candidate state and the
//! violation query reads it directly (rolled back on violation).

mod common;

use common::{dec, expect_committed, propose_pg_with_test_actor, reset_db, subj, test_pool};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::PgProposalOutcome;
use sqlx::Row;

/// The denial form of `balanced_posted_entry`: one row (the witnessing
/// entry) when some entry's debit and credit sums disagree, no rows when
/// the invariant holds.
const BALANCED_VIOLATION_SQL: &str = r#"
/* morpholog-spike invariant balanced_posted_entry v1 stage1 (hand-written) */
SELECT (t0.arguments -> 0 ->> 'value')::text AS "w_entry"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND NOT (
    COALESCE((SELECT sum((s0.arguments -> 2 ->> 'value')::numeric)
              FROM morpholog.claims s0
              WHERE s0.predicate_name = 'JournalLine'
                AND (s0.arguments -> 0 ->> 'value') = (t0.arguments -> 0 ->> 'value')), 0::numeric)
    =
    COALESCE((SELECT sum((s1.arguments -> 3 ->> 'value')::numeric)
              FROM morpholog.claims s1
              WHERE s1.predicate_name = 'JournalLine'
                AND (s1.arguments -> 0 ->> 'value') = (t0.arguments -> 0 ->> 'value')), 0::numeric)
  )
ORDER BY t0.predicate_name, t0.arguments
LIMIT 1
"#;

async fn violation_witness<'e, E>(executor: E) -> Option<String>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(BALANCED_VIOLATION_SQL)
        .fetch_optional(executor)
        .await
        .expect("violation query runs")
        .map(|row| row.get::<String, _>("w_entry"))
}

async fn insert_claim(
    tx: &mut sqlx::PgConnection,
    predicate: &str,
    args: serde_json::Value,
) {
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         VALUES ($1, $2, $3)",
    )
    .bind(predicate)
    .bind(args)
    .bind(uuid::Uuid::nil())
    .execute(tx)
    .await
    .expect("hand insert");
}

fn s(v: &str) -> serde_json::Value {
    serde_json::json!({"type": "subject", "value": v})
}

fn d(v: &str) -> serde_json::Value {
    serde_json::json!({"type": "decimal", "value": v})
}

#[tokio::test]
async fn balanced_state_yields_no_violation_row_where_kernel_accepted() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(double_entry_ledger::program());

    // Two balanced entries, both admitted by the kernel (the acceptances
    // ARE the kernel verdict for this state).
    expect_committed(
        propose_pg_with_test_actor(
            &pool,
            &compiled,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("e1"),
                subj("d1"),
                subj("p1"),
                subj("cash"),
                subj("rev"),
                dec(100),
            ],
        )
        .await
        .unwrap(),
    );
    expect_committed(
        propose_pg_with_test_actor(
            &pool,
            &compiled,
            &double_entry_ledger::post_split_entry(),
            vec![
                subj("e2"),
                subj("d1"),
                subj("p1"),
                subj("cash"),
                dec(90),
                subj("pay"),
                dec(60),
                subj("tax"),
                dec(30),
            ],
        )
        .await
        .unwrap(),
    );

    assert_eq!(violation_witness(&pool).await, None);
}

#[tokio::test]
async fn unbalanced_candidate_yields_witness_where_kernel_rejects() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(double_entry_ledger::program());

    expect_committed(
        propose_pg_with_test_actor(
            &pool,
            &compiled,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("e1"),
                subj("d1"),
                subj("p1"),
                subj("cash"),
                subj("rev"),
                dec(100),
            ],
        )
        .await
        .unwrap(),
    );

    // Kernel verdict on an unbalanced split: 100 debit vs 60 + 30 credit.
    let outcome = propose_pg_with_test_actor(
        &pool,
        &compiled,
        &double_entry_ledger::post_split_entry(),
        vec![
            subj("e_bad"),
            subj("d1"),
            subj("p1"),
            subj("cash"),
            dec(100),
            subj("pay"),
            dec(60),
            subj("tax"),
            dec(30),
        ],
    )
    .await
    .unwrap();
    match outcome {
        PgProposalOutcome::Rejected { reason, .. } => {
            assert!(
                reason.contains("balanced_posted_entry"),
                "kernel rejected for a different rule: {reason}"
            );
        }
        PgProposalOutcome::Committed { .. } => panic!("kernel accepted an unbalanced entry"),
    }

    // SQL verdict on the same candidate: stage the identical delta inside
    // an open transaction (the spike's writes-first shape) and run the
    // violation query against it.
    let mut tx = pool.begin().await.unwrap();
    insert_claim(&mut tx, "JournalEntry", serde_json::json!([s("e_bad"), s("d1"), s("p1")]))
        .await;
    insert_claim(
        &mut tx,
        "JournalLine",
        serde_json::json!([s("e_bad"), s("cash"), d("100"), d("0")]),
    )
    .await;
    insert_claim(
        &mut tx,
        "JournalLine",
        serde_json::json!([s("e_bad"), s("pay"), d("0"), d("60")]),
    )
    .await;
    insert_claim(
        &mut tx,
        "JournalLine",
        serde_json::json!([s("e_bad"), s("tax"), d("0"), d("30")]),
    )
    .await;

    assert_eq!(violation_witness(&mut *tx).await, Some("e_bad".to_string()));
    tx.rollback().await.unwrap();

    // The candidate never leaked: the committed state still holds.
    assert_eq!(violation_witness(&pool).await, None);
}

#[tokio::test]
async fn scale_insensitive_decimals_agree_with_kernel_equality() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // "100.0" vs "100.00" are JSON-unequal but numerically equal; the
    // kernel's rust_decimal equality is scale-insensitive, so ::numeric
    // comparison must be too. Hand-stage an entry whose sums agree only
    // numerically.
    let mut tx = pool.begin().await.unwrap();
    insert_claim(&mut tx, "JournalEntry", serde_json::json!([s("e_s"), s("d1"), s("p1")]))
        .await;
    insert_claim(
        &mut tx,
        "JournalLine",
        serde_json::json!([s("e_s"), s("cash"), d("100.0"), d("0")]),
    )
    .await;
    insert_claim(
        &mut tx,
        "JournalLine",
        serde_json::json!([s("e_s"), s("rev"), d("0"), d("100.00")]),
    )
    .await;

    assert_eq!(violation_witness(&mut *tx).await, None);
    tx.rollback().await.unwrap();
}
