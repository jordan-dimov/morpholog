//! Attestation lineage: every commit records which authenticated
//! PostgreSQL role asserted the actor, and that lineage is part of the
//! Merkle leaf. A history with a pre-attestation prefix still verifies
//! whole, live and offline. The database refuses any new unattested row,
//! so a writer shaped like an older one cannot extend an older regime.
//!
//! Attackers modelled: a stale writer that omits the attestation, and one
//! with full DDL control that drops the database's refusal and then
//! rewrites or strips the lineage; the Merkle tree catches the second.

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

    // The real order of an upgraded deployment: rows from before
    // attestation, then the migration's NOT VALID constraint, then
    // attested commits.
    sqlx::query("ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS audit_attestation_required")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS audit_parameters_required")
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
    // Then the second regime: attested rows from before parameter
    // names existed, then that activation boundary too.
    attested_unstamped_insert(&pool).await.unwrap();
    sqlx::query(
        "ALTER TABLE morpholog.audit
         ADD CONSTRAINT audit_parameters_required
         CHECK (parameters IS NOT NULL) NOT VALID",
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

    // The whole history - legacy, attested, self-describing - verifies
    // as one tree, live and offline; the rows carry their encodings.
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
    let regimes: Vec<(bool, bool)> = pack
        .rows
        .iter()
        .map(|r| (r.attestation.is_some(), r.parameters.is_some()))
        .collect();
    assert_eq!(
        regimes,
        vec![(false, false), (true, false), (true, true)],
        "every encoding, in the real chronology"
    );
    let declared: Vec<String> = double_entry_ledger::post_simple_entry()
        .parameters
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        pack.rows[2].parameters.as_deref(),
        Some(declared.as_slice()),
        "the stamped row names its own signature, in declaration order"
    );

    // A stamped name is evidence: edit one in the exported pack and the
    // tree no longer verifies.
    let mut edited = pack.clone();
    edited.rows[2].parameters.as_mut().unwrap()[0] = "entry".to_string();
    assert!(
        !matches!(
            verify_pack(&edited, None).unwrap(),
            TreeVerification::Intact { .. }
        ),
        "a renamed parameter must break the leaf"
    );
    // Names grafted onto an attested but unstamped row match no
    // encoding: malformed, not intact.
    let mut grafted = pack.clone();
    grafted.rows[0].parameters = Some(vec![]);
    assert!(
        !matches!(
            verify_pack(&grafted, None),
            Ok(TreeVerification::Intact { .. })
        ),
        "names on an unattested row are no encoding"
    );

    // After activation the database refuses an insert shaped like an
    // earlier writer, so a stale binary cannot extend an older regime.
    let refused = legacy_insert(&pool).await;
    let err = refused.expect_err("an unattested insert must be refused after activation");
    assert!(
        err.to_string().contains("audit_attestation_required"),
        "the refusal names the activation constraint: {err}"
    );
    let refused = attested_unstamped_insert(&pool).await;
    let err = refused.expect_err("an unstamped insert must be refused after activation");
    assert!(
        err.to_string().contains("audit_parameters_required"),
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

/// An audit insert shaped like the writer from between attestation and
/// parameter names: attested, no names.
async fn attested_unstamped_insert(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation
         ) VALUES ($1, 'attested_import', '[]', '{\"type\":\"subject\",\"value\":\"importer\"}',
                   1, '[]', '[]', '[]', '[]',
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"importer_role\"}')",
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

    // Attacker: full DDL control, so it can drop the database floor.
    // The tree still catches it. Rewriting the lineage changes the leaf
    // and breaks the root. Stripping it leaves a row shape no writer
    // produced, refused on read before anything is hashed.
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
    let stripped = verify_audit_tree(&pool, None).await;

    // Restore the constraint before asserting, so a failure cannot leave
    // later tests on a weaker schema; resets do not recreate it. NOT
    // VALID leaves the stripped rows above as they are.
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
        matches!(&stripped, Err(morpholog_postgres::PgError::InvalidState(detail))
            if detail.contains("parameter names but no attestation")),
        "a stripped stamped row is refused by name: {stripped:?}"
    );
}
