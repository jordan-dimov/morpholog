//! Attestation lineage as the runtime records and evidences it: every
//! commit carries which PostgreSQL-authenticated role asserted the
//! actor, the lineage joins the Merkle leaf, and a history mixing
//! attested rows with rows from before attestation existed verifies
//! whole - live and offline.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{compiled, expect_committed, propose_pg_with_test_actor, reset_db, test_pool};

use morpholog_core::EvalValue;
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    AuditAttestation, TreeVerification, create_checkpoint, list_audit_rows, verify_audit_tree,
    verify_pack,
};
use morpholog_test_support::{dec, subj};

fn ledger_args(entry: &str) -> Vec<EvalValue> {
    vec![
        subj(entry),
        subj("d_2026_07_22"),
        subj("p_2026_07"),
        subj("account_cash"),
        subj("account_revenue"),
        dec(40),
    ]
}

#[tokio::test]
async fn a_commit_records_the_sessions_authenticated_role() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = compiled(double_entry_ledger::program());
    propose_pg_with_test_actor(
        &pool,
        &program,
        &double_entry_ledger::post_simple_entry(),
        ledger_args("e_attested"),
    )
    .await
    .map(expect_committed)
    .unwrap();

    let session_user: String = sqlx::query_scalar("SELECT session_user")
        .fetch_one(&pool)
        .await
        .unwrap();
    let rows = list_audit_rows(&pool).await.unwrap();
    let AuditAttestation::Gateway { authenticated_by } =
        rows.last().unwrap().attestation.clone().expect("attested");
    assert_eq!(authenticated_by, session_user);
}

#[tokio::test]
async fn a_history_mixing_leaf_encodings_verifies_whole() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = compiled(double_entry_ledger::program());

    // One attested commit, then one row shaped like history from
    // before attestation existed (direct SQL, no attestation column),
    // then another attested commit. The checkpoint covers all three.
    propose_pg_with_test_actor(
        &pool,
        &program,
        &double_entry_ledger::post_simple_entry(),
        ledger_args("e_first"),
    )
    .await
    .map(expect_committed)
    .unwrap();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents
         ) VALUES ($1, 'legacy_import', '[]', '{\"type\":\"subject\",\"value\":\"importer\"}',
                   1, '[]', '[]', '[]', '[]')",
    )
    .bind(uuid::Uuid::now_v7())
    .execute(&pool)
    .await
    .unwrap();
    propose_pg_with_test_actor(
        &pool,
        &program,
        &double_entry_ledger::post_simple_entry(),
        ledger_args("e_second"),
    )
    .await
    .map(expect_committed)
    .unwrap();

    create_checkpoint(&pool, None).await.unwrap();
    let verification = verify_audit_tree(&pool, None).await.unwrap();
    assert!(
        matches!(verification, TreeVerification::Intact { .. }),
        "mixed-encoding history must verify: {verification:?}"
    );

    // The same rows travel whole into an evidence pack, so an offline
    // verifier recomputes each row's leaf under the row's own encoding.
    let pack = morpholog_postgres::export_pack(&pool, None).await.unwrap();
    assert!(matches!(
        verify_pack(&pack, None).unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn tampering_with_the_attestation_breaks_the_root() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = compiled(double_entry_ledger::program());
    propose_pg_with_test_actor(
        &pool,
        &program,
        &double_entry_ledger::post_simple_entry(),
        ledger_args("e_target"),
    )
    .await
    .map(expect_committed)
    .unwrap();
    create_checkpoint(&pool, None).await.unwrap();

    // Rewriting the lineage - or stripping it to fall back to the
    // other encoding - both change the leaf and break the root.
    sqlx::query(
        "UPDATE morpholog.audit
         SET attestation = '{\"mode\":\"gateway\",\"authenticated_by\":\"intruder\"}'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let rewritten = verify_audit_tree(&pool, None).await.unwrap();
    assert!(
        !matches!(rewritten, TreeVerification::Intact { .. }),
        "rewritten lineage must not verify: {rewritten:?}"
    );

    sqlx::query("UPDATE morpholog.audit SET attestation = NULL")
        .execute(&pool)
        .await
        .unwrap();
    let stripped = verify_audit_tree(&pool, None).await.unwrap();
    assert!(
        !matches!(stripped, TreeVerification::Intact { .. }),
        "stripped lineage must not verify: {stripped:?}"
    );
}
