//! Attestation lineage as the runtime records and evidences it: every
//! commit carries which PostgreSQL-authenticated role asserted the
//! actor, the lineage joins the Merkle leaf, a history whose legacy
//! prefix predates attestation verifies whole - live and offline -
//! and the database floor refuses any new unattested row, so
//! attestation is a one-way boundary, not a per-row option.

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
async fn a_legacy_prefix_verifies_whole_and_new_unattested_rows_are_refused() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = compiled(double_entry_ledger::program());

    // Replay the real chronology of an upgraded deployment: rows from
    // before attestation existed (written under a schema with no
    // attestation column), then the activation boundary - the
    // migration's NOT VALID constraint - then attested commits.
    sqlx::query("ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS audit_attestation_required")
        .execute(&pool)
        .await
        .unwrap();
    legacy_insert(&pool).await.unwrap();
    sqlx::query(
        "ALTER TABLE morpholog.audit
         ADD CONSTRAINT audit_attestation_required
         CHECK (attestation IS NOT NULL) NOT VALID",
    )
    .execute(&pool)
    .await
    .unwrap();
    propose_pg_with_test_actor(
        &pool,
        &program,
        &double_entry_ledger::post_simple_entry(),
        ledger_args("e_attested"),
    )
    .await
    .map(expect_committed)
    .unwrap();

    // The whole history - legacy prefix plus attested suffix -
    // verifies, live and offline.
    create_checkpoint(&pool, None, None).await.unwrap();
    let verification = verify_audit_tree(&pool, None).await.unwrap();
    assert!(
        matches!(verification, TreeVerification::Intact { .. }),
        "upgraded history must verify: {verification:?}"
    );
    let pack = morpholog_postgres::export_pack(&pool, None).await.unwrap();
    assert!(matches!(
        verify_pack(&pack, None).unwrap(),
        TreeVerification::Intact { .. }
    ));

    // The boundary is one-way: after activation, an insert shaped like
    // the pre-attestation writer is refused by the database itself -
    // a stale binary cannot quietly extend the legacy prefix.
    let refused = legacy_insert(&pool).await;
    let err = refused.expect_err("an unattested insert must be refused after activation");
    assert!(
        err.to_string().contains("audit_attestation_required"),
        "the refusal names the activation constraint: {err}"
    );
}

/// An audit insert shaped like the writer from before attestation
/// existed: no attestation column at all.
async fn legacy_insert(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents
         ) VALUES ($1, 'legacy_import', '[]', '{\"type\":\"subject\",\"value\":\"importer\"}',
                   1, '[]', '[]', '[]', '[]')",
    )
    .bind(uuid::Uuid::now_v7())
    .execute(pool)
    .await
    .map(|_| ())
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
    create_checkpoint(&pool, None, None).await.unwrap();

    // An attacker with full DDL control can drop the database floor;
    // the tree is the layer that still catches them. Rewriting the
    // lineage - or stripping it to fall back to the other encoding -
    // both change the leaf and break the root.
    sqlx::query("ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS audit_attestation_required")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE morpholog.audit
         SET attestation = '{\"mode\":\"gateway\",\"authenticated_by\":\"intruder\"}'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let rewritten = verify_audit_tree(&pool, None).await.unwrap();

    sqlx::query("UPDATE morpholog.audit SET attestation = NULL")
        .execute(&pool)
        .await
        .unwrap();
    let stripped = verify_audit_tree(&pool, None).await.unwrap();

    // Restore the production floor BEFORE asserting, so a failing
    // assertion cannot leave every later test binary running against a
    // schema weaker than production (the tables are truncated between
    // tests; constraints are not re-provisioned). NOT VALID is the
    // restored upgraded-database state - the stripped rows above stay
    // NULL.
    sqlx::query(
        "ALTER TABLE morpholog.audit
         ADD CONSTRAINT audit_attestation_required
         CHECK (attestation IS NOT NULL) NOT VALID",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        !matches!(rewritten, TreeVerification::Intact { .. }),
        "rewritten lineage must not verify: {rewritten:?}"
    );
    assert!(
        !matches!(stripped, TreeVerification::Intact { .. }),
        "stripped lineage must not verify: {stripped:?}"
    );
}
