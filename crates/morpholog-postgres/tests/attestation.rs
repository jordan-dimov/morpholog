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
use common::{expect_committed, pg_program, propose_pg_with_test_actor, reset_db, test_pool};

use morpholog_core::EvalValue;
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    AuditAttestation, CheckpointOutcome, TreeVerification, create_checkpoint, list_audit_rows,
    verify_audit_tree, verify_pack,
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
    let program = pg_program(double_entry_ledger::program());
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
    let role_oid: sqlx::postgres::types::Oid =
        sqlx::query_scalar("SELECT oid FROM pg_roles WHERE rolname = session_user")
            .fetch_one(&pool)
            .await
            .unwrap();
    let rows = list_audit_rows(&pool).await.unwrap();
    let AuditAttestation::Gateway {
        authenticated_by,
        authenticated_by_oid,
    } = rows.last().unwrap().attestation.clone().expect("attested");
    assert_eq!(authenticated_by, session_user);
    // Which incarnation of the name: the role's OID when it committed.
    assert_eq!(authenticated_by_oid, Some(role_oid.0));
}

#[tokio::test]
async fn a_legacy_prefix_verifies_whole_and_new_unattested_rows_are_refused() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = pg_program(double_entry_ledger::program());

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
    sqlx::query("ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS audit_model_hash_required")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS audit_semantics_version_required",
    )
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
    // The third regime: rows that name their parameters but not their
    // programme, checkpointed before the model-hash boundary.
    named_unhashed_insert(&pool).await.unwrap();
    let CheckpointOutcome::Created(before) = create_checkpoint(&pool, None, None).await.unwrap()
    else {
        panic!("three rows must make a new checkpoint");
    };
    sqlx::query(
        "ALTER TABLE morpholog.audit
         ADD CONSTRAINT audit_model_hash_required
         CHECK (model_hash IS NOT NULL) NOT VALID",
    )
    .execute(&pool)
    .await
    .unwrap();
    // The fourth regime: rows that name their programme but not their
    // semantics, checkpointed before the semantics boundary.
    hashed_unversioned_insert(&pool).await.unwrap();
    let CheckpointOutcome::Created(before_semantics) =
        create_checkpoint(&pool, None, None).await.unwrap()
    else {
        panic!("a fourth row must make a new checkpoint");
    };
    sqlx::query(
        "ALTER TABLE morpholog.audit
         ADD CONSTRAINT audit_semantics_version_required
         CHECK (semantics_version IS NOT NULL) NOT VALID",
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

    // The whole history - legacy, attested, self-describing, naming its
    // programme, naming its semantics - verifies as one tree, live and
    // offline, and the checkpoints taken before the last two boundaries
    // keep the roots they had: verification recomputes every stored
    // checkpoint's root.
    create_checkpoint(&pool, None, None).await.unwrap();
    let verification = verify_audit_tree(&pool, None).await.unwrap();
    assert!(
        matches!(verification, TreeVerification::Intact { .. }),
        "upgraded history must verify: {verification:?}"
    );
    let kept = morpholog_postgres::load_checkpoint(&pool, before.tree_size)
        .await
        .unwrap()
        .expect("the pre-boundary checkpoint is still stored");
    assert_eq!(
        kept.root_hash, before.root_hash,
        "the pre-boundary checkpoint keeps its root"
    );
    let kept = morpholog_postgres::load_checkpoint(&pool, before_semantics.tree_size)
        .await
        .unwrap()
        .expect("the pre-semantics checkpoint is still stored");
    assert_eq!(
        kept.root_hash, before_semantics.root_hash,
        "adding the semantics rung reinterprets no earlier leaf"
    );
    let pack = morpholog_postgres::export_pack(&pool, None).await.unwrap();
    assert!(matches!(
        verify_pack(&pack, None).unwrap(),
        TreeVerification::Intact { .. }
    ));

    // Every exporter announces the version for the highest rung it
    // discloses, so a verifier from before a rung refuses that pack as too
    // new rather than misjudging it, while a pack of older rows stays
    // readable by it.
    assert_eq!(pack.manifest.pack_format_version, 12);
    for (size, expected) in [(before.tree_size, 1), (before_semantics.tree_size, 8)] {
        let older = morpholog_postgres::export_pack(&pool, Some(size))
            .await
            .unwrap();
        assert_eq!(older.manifest.pack_format_version, expected, "{size}");
    }
    for (size, expected) in [
        (Some(before.tree_size), 4),
        (Some(before_semantics.tree_size), 5),
        (None, 9),
    ] {
        let export = morpholog_postgres::begin_prefix_export(&pool, size)
            .await
            .unwrap();
        assert_eq!(export.manifest.pack_format_version, expected, "{size:?}");
    }
    let window = morpholog_postgres::export_window(
        &pool,
        morpholog_postgres::WindowStart::TreeSize(before.tree_size),
        Some(before_semantics.tree_size),
    )
    .await
    .unwrap();
    assert_eq!(window.manifest.pack_format_version, 6);
    // A window across the V4-to-V5 boundary is one history, not two eras.
    let across = morpholog_postgres::export_window(
        &pool,
        morpholog_postgres::WindowStart::TreeSize(before.tree_size),
        None,
    )
    .await
    .unwrap();
    assert_eq!(across.manifest.pack_format_version, 10);
    assert!(
        matches!(
            morpholog_postgres::verify_window(&across, None).unwrap(),
            morpholog_postgres::WindowVerification::Intact { .. }
        ),
        "a window across the semantics boundary verifies"
    );
    for (row, expected) in [(0, 3), (3, 7), (4, 11)] {
        let selective =
            morpholog_postgres::export_selective(&pool, None, &[pack.rows[row].transition_id])
                .await
                .unwrap();
        assert_eq!(
            selective.manifest.pack_format_version, expected,
            "an older row disclosed under a newer checkpoint keeps the readable version"
        );
    }
    let regimes: Vec<(bool, bool, bool, bool)> = pack
        .rows
        .iter()
        .map(|r| {
            (
                r.attestation.is_some(),
                r.parameters.is_some(),
                r.model_hash.is_some(),
                r.semantics_version.is_some(),
            )
        })
        .collect();
    assert_eq!(
        regimes,
        vec![
            (false, false, false, false),
            (true, false, false, false),
            (true, true, false, false),
            (true, true, true, false),
            (true, true, true, true)
        ],
        "every encoding, in the real chronology"
    );
    assert_eq!(
        pack.rows[4].model_hash.as_deref(),
        Some(program.prepared().model_hash()),
        "the newest row names the programme that admitted it"
    );
    assert_eq!(
        pack.rows[4].semantics_version,
        Some(morpholog_core::SEMANTICS_VERSION),
        "and the semantics that decided it"
    );
    let declared: Vec<String> = double_entry_ledger::post_simple_entry()
        .parameters
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        pack.rows[4].parameters.as_deref(),
        Some(declared.as_slice()),
        "the stamped row names its own signature, in declaration order"
    );

    // A stamped name is evidence: edit one in the exported pack and the
    // tree no longer verifies.
    let mut edited = pack.clone();
    edited.rows[4].parameters.as_mut().unwrap()[0] = "entry".to_string();
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
    // PostgreSQL reports the first failing check by constraint name, so
    // an insert missing two later fields names either boundary.
    let refused = attested_unstamped_insert(&pool).await;
    let err = refused.expect_err("an unstamped insert must be refused after activation");
    assert!(
        [
            "audit_model_hash_required",
            "audit_parameters_required",
            "audit_semantics_version_required"
        ]
        .iter()
        .any(|c| err.to_string().contains(c)),
        "the refusal names an activation constraint: {err}"
    );
    let refused = named_unhashed_insert(&pool).await;
    let err = refused.expect_err("an insert naming no programme must be refused after activation");
    assert!(
        [
            "audit_model_hash_required",
            "audit_semantics_version_required"
        ]
        .iter()
        .any(|c| err.to_string().contains(c)),
        "the refusal names an activation constraint: {err}"
    );
    let refused = hashed_unversioned_insert(&pool).await;
    let err = refused.expect_err("an insert naming no semantics must be refused after activation");
    assert!(
        err.to_string().contains("audit_semantics_version_required"),
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

/// An audit insert shaped like the writer from between parameter names
/// and the model hash: attested and named, no programme.
async fn named_unhashed_insert(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation,
            parameters
         ) VALUES ($1, 'named_import', '[]', '{\"type\":\"subject\",\"value\":\"importer\"}',
                   1, '[]', '[]', '[]', '[]',
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"importer_role\"}', '[]')",
    )
    .bind(uuid::Uuid::now_v7())
    .execute(pool)
    .await
    .map(|_| ())
}

/// An audit insert shaped like the writer from before the semantics
/// version existed: it names its programme but not its semantics.
async fn hashed_unversioned_insert(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation,
            parameters, model_hash
         ) VALUES ($1, 'hashed_import', '[]', '{\"type\":\"subject\",\"value\":\"importer\"}',
                   1, '[]', '[]', '[]', '[]',
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"importer_role\"}', '[]',
                   'sha256:' || repeat('1', 64))",
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
    let program = pg_program(double_entry_ledger::program());
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
    sqlx::query("ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS audit_model_hash_shape")
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
    sqlx::query(
        "ALTER TABLE morpholog.audit
         ADD CONSTRAINT audit_model_hash_shape CHECK (
             model_hash IS NULL
             OR (model_hash ~ '^sha256:[0-9a-f]{64}$'
                 AND attestation IS NOT NULL
                 AND parameters IS NOT NULL)
         ) NOT VALID",
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
            if detail.contains("without an attestation")),
        "a stripped stamped row is refused by name: {stripped:?}"
    );
}
