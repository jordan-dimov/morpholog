//! Tamper-evidence over the audit log: checkpoints commit to an RFC 6962
//! Merkle root of the committed prefix, and `verify_audit_tree` catches
//! edits. The load-bearing test is the last one - it forces the trust
//! model honest: a coordinated rewrite of the audit log AND the
//! checkpoint table is internally self-consistent and passes a bare
//! verify, and is caught ONLY by an externally-held anchor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    Checkpoint, CheckpointOutcome, CheckpointSigner, PgPool, TreeVerification, create_checkpoint,
    generate_signing_key, render_public_key, verify_audit_tree,
};

const PURPOSE: &str = "audit_checkpoint_v1";

mod common;
use common::{dec, reset_db, subj, test_pool};

/// Commit one balanced ledger entry so the audit log grows by a row.
async fn commit_entry(pool: &PgPool, id: &str) {
    let compiled = common::compiled(double_entry_ledger::program());
    let t = double_entry_ledger::post_simple_entry();
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &compiled,
        &t,
        vec![
            subj(id),
            subj("d_2026_05_17"),
            subj("p1"),
            subj(&format!("cash_{id}")),
            subj(&format!("rev_{id}")),
            dec(100),
        ],
    )
    .await
    .unwrap();
    common::expect_committed(outcome);
}

async fn make_checkpoint(pool: &PgPool) -> Checkpoint {
    match create_checkpoint(pool, None, None).await.unwrap() {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint, got {other:?}")
        }
    }
}

#[tokio::test]
async fn checkpoint_verifies_then_catches_an_audit_edit() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    for i in 0..3 {
        commit_entry(&pool, &format!("e{i}")).await;
    }
    let anchor = make_checkpoint(&pool).await;
    assert_eq!(anchor.tree_size, 3);
    assert!(anchor.root_hash.starts_with("sha256:"));
    assert!(
        anchor.prev_checkpoint_hash.is_none(),
        "the first is genesis"
    );

    // Intact both ways.
    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { tree_size: 3, .. }
    ));
    assert!(matches!(
        verify_audit_tree(&pool, Some(anchor.clone()))
            .await
            .unwrap(),
        TreeVerification::Intact { .. }
    ));

    // Edit an audit row's content directly - the coordinated-edit an
    // honest replay alone cannot see, since claims could be edited to
    // match. The recomputed root no longer matches the checkpoint.
    sqlx::query("UPDATE morpholog.audit SET transformation_name = 'tampered' WHERE transition_id = (SELECT transition_id FROM morpholog.audit ORDER BY committed_at, transition_id LIMIT 1)")
        .execute(&pool)
        .await
        .unwrap();

    let verdict = verify_audit_tree(&pool, None).await.unwrap();
    assert!(
        matches!(verdict, TreeVerification::Tampered { tree_size: 3, .. }),
        "an edited audit row must be caught: {verdict:?}"
    );
}

#[tokio::test]
async fn checkpoint_chain_extends_and_old_prefix_stays_stable() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    commit_entry(&pool, "a").await;
    commit_entry(&pool, "b").await;
    let first = make_checkpoint(&pool).await;
    assert_eq!(first.tree_size, 2);

    // Appending rows then checkpointing again: the new checkpoint chains
    // off the first, and the first still verifies (its prefix is stable).
    commit_entry(&pool, "c").await;
    let second = make_checkpoint(&pool).await;
    assert_eq!(second.tree_size, 3);
    assert_eq!(
        second.prev_checkpoint_hash.as_deref(),
        Some(first.checkpoint_hash.as_str()),
        "second checkpoint chains off the first"
    );

    assert!(matches!(
        verify_audit_tree(&pool, Some(first)).await.unwrap(),
        TreeVerification::Intact { .. }
    ));

    // No new rows -> no-op returning the unchanged head (a usable anchor),
    // not a forked checkpoint.
    let noop = create_checkpoint(&pool, None, None).await.unwrap();
    let CheckpointOutcome::NoNewRows(head) = noop else {
        panic!("expected NoNewRows, got {noop:?}");
    };
    assert_eq!(head.tree_size, 3);
    assert_eq!(head, second, "the no-op returns the current head unchanged");
}

#[tokio::test]
async fn editing_a_checkpoint_row_breaks_the_chain() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "x").await;
    make_checkpoint(&pool).await;

    // Rewrite the stored root without fixing checkpoint_hash: the
    // checkpoint's contents no longer hash to its recorded identity.
    sqlx::query("UPDATE morpholog.audit_checkpoints SET root_hash = 'sha256:0000000000000000000000000000000000000000000000000000000000000000'")
        .execute(&pool)
        .await
        .unwrap();

    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::ChainBroken { .. }
    ));
}

/// The honest trust model: a coordinated rewrite of the audit log AND a
/// fresh, self-consistent checkpoint over the false history passes a bare
/// verify - and is caught ONLY by the checkpoint that left the database.
#[tokio::test]
async fn coordinated_rewrite_passes_bare_verify_but_fails_against_an_anchor() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..3 {
        commit_entry(&pool, &format!("h{i}")).await;
    }
    // The operator saved this externally before the tampering.
    let anchor = make_checkpoint(&pool).await;

    // Attacker edits the log AND rebuilds the checkpoint chain to match -
    // the same tree_size, a new self-consistent root.
    sqlx::query("UPDATE morpholog.audit SET transformation_name = 'tampered' WHERE transition_id = (SELECT transition_id FROM morpholog.audit ORDER BY committed_at, transition_id LIMIT 1)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM morpholog.audit_checkpoints")
        .execute(&pool)
        .await
        .unwrap();
    let forged = make_checkpoint(&pool).await;
    assert_eq!(forged.tree_size, anchor.tree_size);
    assert_ne!(
        forged.root_hash, anchor.root_hash,
        "the false history has a different root"
    );

    // A bare verify cannot tell - the forged checkpoint matches the
    // forged log. This is the honest limit of internal checks.
    assert!(
        matches!(
            verify_audit_tree(&pool, None).await.unwrap(),
            TreeVerification::Intact { .. }
        ),
        "without an external anchor the rewrite is self-consistent"
    );

    // The externally-held anchor exposes it: the stored checkpoint at
    // that tree size no longer matches the copy that left the database.
    let verdict = verify_audit_tree(&pool, Some(anchor)).await.unwrap();
    assert!(
        matches!(
            verdict,
            TreeVerification::AnchorMismatch { tree_size: 3, .. }
        ),
        "the anchor must catch the coordinated rewrite: {verdict:?}"
    );
}

#[tokio::test]
async fn a_signed_checkpoint_verifies_and_a_corrupted_signature_is_caught() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..2 {
        commit_entry(&pool, &format!("s{i}")).await;
    }

    let key = generate_signing_key();
    common::authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&key.verifying_key()),
    )
    .await;
    let signer = CheckpointSigner {
        key_id: "k1".into(),
        key,
    };
    let cp = match create_checkpoint(&pool, Some(&signer), None).await.unwrap() {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint, got {other:?}")
        }
    };
    assert_eq!(cp.signatures.len(), 1);
    assert_eq!(cp.signatures[0].key_id, "k1");

    // A genuinely signed checkpoint verifies intact.
    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));

    // Replace the stored signature with one that does not verify, keeping
    // the tree head intact: the verdict is SignatureInvalid, not Tampered.
    let corrupted = format!(
        r#"[{{"key_id":"k1","purpose":"audit_checkpoint_v1","public_key":"{}","signature":"ed25519-sig:{}"}}]"#,
        cp.signatures[0].public_key,
        "0".repeat(128)
    );
    sqlx::query("UPDATE morpholog.audit_checkpoints SET signatures = $1::jsonb")
        .bind(corrupted)
        .execute(&pool)
        .await
        .unwrap();

    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::SignatureInvalid { key_id, .. } if key_id == "k1"
    ));
}

#[tokio::test]
async fn signing_an_existing_unsigned_head_attaches_the_signature_idempotently() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..2 {
        commit_entry(&pool, &format!("u{i}")).await;
    }
    // Authorise the key before the head exists, so it is in force as of the
    // checkpoint's prefix.
    let key = generate_signing_key();
    common::authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&key.verifying_key()),
    )
    .await;

    // An unsigned head, then a sign run with no new rows: the signature is
    // attached to the existing head, not dropped (the operational trap).
    let unsigned = make_checkpoint(&pool).await;
    assert!(unsigned.signatures.is_empty());

    let signer = CheckpointSigner {
        key_id: "k1".into(),
        key,
    };
    let signed = match create_checkpoint(&pool, Some(&signer), None).await.unwrap() {
        CheckpointOutcome::NoNewRows(c) => c,
        other @ CheckpointOutcome::Created(_) => panic!("expected no new rows, got {other:?}"),
    };
    assert_eq!(
        signed.checkpoint_hash, unsigned.checkpoint_hash,
        "same head"
    );
    assert_eq!(signed.signatures.len(), 1);
    assert_eq!(signed.signatures[0].key_id, "k1");
    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));

    // Re-signing the same head with the same key is idempotent.
    let again = match create_checkpoint(&pool, Some(&signer), None).await.unwrap() {
        CheckpointOutcome::NoNewRows(c) => c,
        other @ CheckpointOutcome::Created(_) => panic!("expected no new rows, got {other:?}"),
    };
    assert_eq!(again.signatures.len(), 1, "exact re-sign is de-duplicated");
}

#[tokio::test]
async fn an_anchor_differing_only_in_signatures_is_not_a_mismatch() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..2 {
        commit_entry(&pool, &format!("a{i}")).await;
    }
    let key = generate_signing_key();
    common::authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&key.verifying_key()),
    )
    .await;
    let signer = CheckpointSigner {
        key_id: "k1".into(),
        key,
    };
    let signed = match create_checkpoint(&pool, Some(&signer), None).await.unwrap() {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint, got {other:?}")
        }
    };

    // An anchor with the same tree head but no signatures must still
    // verify intact: the anchor check is on the head, not the signatures.
    let unsigned_anchor = Checkpoint {
        signatures: Vec::new(),
        ..signed.clone()
    };
    assert!(matches!(
        verify_audit_tree(&pool, Some(unsigned_anchor))
            .await
            .unwrap(),
        TreeVerification::Intact { .. }
    ));
}
