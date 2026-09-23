//! Signing-key authority: a signed checkpoint is intact only if its key
//! was admitted as an `AuditSigningKey` claim as of the checkpoint's
//! prefix. A genuine signature by a key the ledger never authorised is
//! `UnauthorizedKey`. Signing refuses such a key, so the attacker here
//! is one who edits the signatures column or supplies a forged anchor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use morpholog_postgres::{
    Checkpoint, CheckpointOutcome, CheckpointSigner, PgError, PgPool, TreeHead, TreeHeadSignature,
    TreeVerification, create_checkpoint, export_pack, generate_signing_key, render_public_key,
    render_signature, sign_tree_head, verify_audit_tree, verify_pack,
};

mod common;
use common::{
    authorize_signing_key, drop_roles_if_present, recreate_roles, reset_db, retract_signing_key,
    session_is_superuser, test_pool,
};

const PURPOSE: &str = "audit_checkpoint_v1";

/// Authorise a fresh key under `key_id` and create a checkpoint signed by
/// it: the legitimate starting point most tests then attack.
async fn signed_checkpoint(pool: &PgPool, key_id: &str) -> Checkpoint {
    let key = generate_signing_key();
    authorize_signing_key(
        pool,
        key_id,
        PURPOSE,
        &render_public_key(&key.verifying_key()),
    )
    .await;
    let signer = CheckpointSigner {
        key_id: key_id.into(),
        key,
    };
    match create_checkpoint(pool, Some(&signer), None).await.unwrap() {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint: {other:?}")
        }
    }
}

/// A genuine signature over a checkpoint's tree head by any key, as an
/// edited signatures column or a forged anchor would carry.
fn signature_over(key: &SigningKey, key_id: &str, cp: &Checkpoint) -> TreeHeadSignature {
    let head = TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_ref(),
        checkpoint_hash: &cp.checkpoint_hash,
    };
    TreeHeadSignature {
        key_id: key_id.into(),
        purpose: PURPOSE.into(),
        public_key: render_public_key(&key.verifying_key()),
        signature: render_signature(&sign_tree_head(key, PURPOSE, key_id, &head)),
    }
}

#[tokio::test]
async fn a_checkpoint_signed_by_an_authorized_key_verifies_intact() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let cp = signed_checkpoint(&pool, "k1").await;
    assert_eq!(cp.signatures.len(), 1);
    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn signing_with_an_unauthorized_key_is_refused() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Authorise k1; then try to sign with a genuine but unauthorised key.
    let authorized = generate_signing_key();
    authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&authorized.verifying_key()),
    )
    .await;

    let interloper = CheckpointSigner {
        key_id: "k1".into(),
        key: generate_signing_key(),
    };
    let err = create_checkpoint(&pool, Some(&interloper), None)
        .await
        .expect_err("signing with an unauthorised key must be refused, not produced");
    // Nothing withheld, so the refusal is about the key, not the horizon.
    assert!(
        matches!(err, PgError::SigningKeyUnauthorised { tree_size: 1, .. }),
        "expected the plain authority refusal as of this prefix, got: {err}"
    );
}

#[tokio::test]
async fn an_unauthorised_key_with_a_withheld_authorisation_names_the_horizon() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // An older transaction is still open when the key is authorised, so
    // the authorisation is withheld from the checkpoint, not missing.
    let mut interferer = pool.begin().await.unwrap();
    sqlx::query("SELECT transaction_timestamp()")
        .execute(&mut *interferer)
        .await
        .unwrap();

    let key = generate_signing_key();
    authorize_signing_key(
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

    let err = create_checkpoint(&pool, Some(&signer), None)
        .await
        .expect_err("the authorisation is above the horizon, so signing must refuse");
    match &err {
        PgError::SigningKeyUnauthorisedAtTruncatedPrefix {
            tree_size,
            committed_beyond_horizon,
            ..
        } => {
            assert_eq!(*tree_size, 0, "everything is withheld, the prefix is empty");
            assert_eq!(
                *committed_beyond_horizon, 1,
                "exactly the authorisation row is beyond the horizon"
            );
        }
        other => panic!("expected the truncated-prefix diagnosis, got: {other}"),
    }
    assert!(
        err.to_string().contains("retry after the horizon advances"),
        "the message must point at the workload, not the key: {err}"
    );

    // The remedy the message names works: once the older transaction
    // ends, the same signer succeeds.
    interferer.rollback().await.unwrap();
    match create_checkpoint(&pool, Some(&signer), None).await.unwrap() {
        CheckpointOutcome::Created(c) => {
            assert_eq!(c.tree_size, 1);
            assert_eq!(c.signatures.len(), 1);
        }
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created signed checkpoint: {other:?}")
        }
    }
}

// A signing run sees a prefix the horizon may cut short, but its
// signature attaches to the chain head, which may be past a revocation.
// Authority must be judged at the head, or signing makes a checkpoint
// `verify` rejects as UnauthorizedKey.
#[tokio::test]
async fn attaching_a_signature_resolves_authority_as_of_the_head_actually_signed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        // The checkpoint below relies on the asserted horizon ignoring
        // our superuser interferer session.
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let roles = ["mtest303_idle"];
    recreate_roles(&pool, &roles, &["CREATE ROLE mtest303_idle LOGIN"]).await;

    // Authorise k1; an older transaction opens; then k1 is revoked, the
    // revocation committing above that transaction's start.
    let key = generate_signing_key();
    let public_key = render_public_key(&key.verifying_key());
    authorize_signing_key(&pool, "k1", PURPOSE, &public_key).await;

    let mut interferer = pool.begin().await.unwrap();
    sqlx::query("SELECT transaction_timestamp()")
        .execute(&mut *interferer)
        .await
        .unwrap();

    retract_signing_key(&pool, "k1", PURPOSE, &public_key).await;

    // A checkpoint under the asserted horizon (which ignores the
    // interferer) advances the head past the revocation.
    let head = match create_checkpoint(&pool, None, Some(&["mtest303_idle".to_string()]))
        .await
        .unwrap()
    {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint: {other:?}")
        }
    };
    assert_eq!(
        head.tree_size, 2,
        "the head covers authorisation and revocation"
    );

    // Without the assertion the horizon trails the interferer, so the
    // prefix (tree_size 1) shows k1 authorised. But the signature would
    // attach to the head at 2, where k1 is revoked.
    let signer = CheckpointSigner {
        key_id: "k1".into(),
        key,
    };
    let err = create_checkpoint(&pool, Some(&signer), None)
        .await
        .expect_err("signing must be judged against the head that receives the signature");
    assert!(
        matches!(err, PgError::SigningKeyUnauthorised { tree_size: 2, .. }),
        "expected the refusal as of the head's own prefix, got: {err}"
    );

    // No unauthorised signature was attached: the tree still verifies.
    interferer.rollback().await.unwrap();
    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
    drop_roles_if_present(&pool, &roles).await;
}

// The mirror image: the key IS authorised, in a committed row past the
// head that the held-back horizon keeps out of any checkpoint. The
// refusal must blame the horizon, not tell the caller to fix the key.
#[tokio::test]
async fn an_authorisation_beyond_the_head_is_diagnosed_as_withheld_not_missing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let roles = ["mtest305_idle"];
    recreate_roles(&pool, &roles, &["CREATE ROLE mtest305_idle LOGIN"]).await;

    // An older transaction opens; an unrelated row commits; a checkpoint
    // under the asserted horizon makes that row the head; then the key
    // is authorised.
    let mut interferer = pool.begin().await.unwrap();
    sqlx::query("SELECT transaction_timestamp()")
        .execute(&mut *interferer)
        .await
        .unwrap();
    common::commit_entry(&pool, "e1").await;
    let head = match create_checkpoint(&pool, None, Some(&["mtest305_idle".to_string()]))
        .await
        .unwrap()
    {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint: {other:?}")
        }
    };
    assert_eq!(head.tree_size, 1, "the unrelated row is the head");

    let key = generate_signing_key();
    authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&key.verifying_key()),
    )
    .await;

    // Without the assertion the prefix is empty and the head (tree_size
    // 1) predates the authorisation, which is committed one row beyond.
    // The refusal must say so, not blame the key.
    let signer = CheckpointSigner {
        key_id: "k1".into(),
        key,
    };
    let err = create_checkpoint(&pool, Some(&signer), None)
        .await
        .expect_err("the head predates the authorisation, so signing must refuse");
    match &err {
        PgError::SigningKeyUnauthorisedAtTruncatedPrefix {
            tree_size,
            committed_beyond_horizon,
            ..
        } => {
            assert_eq!(*tree_size, 1, "judged as of the head actually signable");
            assert_eq!(
                *committed_beyond_horizon, 1,
                "exactly the authorisation row lies beyond the head"
            );
        }
        other => panic!("expected the truncated-prefix diagnosis, got: {other}"),
    }

    // Once the interferer ends, the same signer checkpoints the suffix
    // that authorises it.
    interferer.rollback().await.unwrap();
    match create_checkpoint(&pool, Some(&signer), None).await.unwrap() {
        CheckpointOutcome::Created(c) => {
            assert_eq!(c.tree_size, 2);
            assert_eq!(c.signatures.len(), 1);
        }
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created signed checkpoint: {other:?}")
        }
    }
    drop_roles_if_present(&pool, &roles).await;
}

#[tokio::test]
async fn a_genuine_signature_by_an_unauthorized_key_is_unauthorized_key() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Sign legitimately, then put a genuine signature by an unauthorised
    // key in the signatures column. Signatures are not in the tree head,
    // so only the authority check catches it.
    let cp = signed_checkpoint(&pool, "k1").await;
    let forged = signature_over(&generate_signing_key(), "k2", &cp);
    sqlx::query("UPDATE morpholog.audit_checkpoints SET signatures = $1::jsonb")
        .bind(serde_json::to_string(&vec![forged]).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::UnauthorizedKey { key_id, .. } if key_id == "k2"
    ));
}

#[tokio::test]
async fn an_offline_pack_resolves_authority_from_its_own_rows() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    signed_checkpoint(&pool, "k1").await;

    // The pack carries the AuditSigningKey admission in its own rows, so a
    // third party verifies authority offline, with no database.
    let pack = export_pack(&pool, None).await.unwrap();
    assert!(matches!(
        verify_pack(&pack, None).unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn an_unsigned_checkpoint_asks_no_authority_question() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    authorize_signing_key(&pool, "k1", PURPOSE, "ed25519-pub:unused").await;

    create_checkpoint(&pool, None, None).await.unwrap();
    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn a_signed_anchor_is_verified_even_when_the_stored_signature_is_stripped() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let anchor = signed_checkpoint(&pool, "k1").await;

    // Attacker strips the signature from the database; the operator still
    // holds the signed anchor. The anchor's own signature is verified, so
    // the tree is attributable despite the stripped database copy.
    sqlx::query("UPDATE morpholog.audit_checkpoints SET signatures = '[]'::jsonb")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        verify_audit_tree(&pool, Some(anchor)).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn a_signed_anchor_with_a_corrupted_signature_is_caught() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let mut anchor = signed_checkpoint(&pool, "k1").await;
    // The held anchor's signature does not verify over its tree head.
    anchor.signatures[0].signature = format!("ed25519-sig:{}", "0".repeat(128));

    assert!(matches!(
        verify_audit_tree(&pool, Some(anchor)).await.unwrap(),
        TreeVerification::SignatureInvalid { key_id, .. } if key_id == "k1"
    ));
}

#[tokio::test]
async fn a_signed_anchor_by_an_unauthorized_key_is_caught() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let cp = signed_checkpoint(&pool, "k1").await;

    // A forged anchor: same tree head, a genuine signature by a key the
    // ledger never authorised. The signature verifies; authority does not.
    let anchor = Checkpoint {
        signatures: vec![signature_over(&generate_signing_key(), "k2", &cp)],
        ..cp
    };
    assert!(matches!(
        verify_audit_tree(&pool, Some(anchor)).await.unwrap(),
        TreeVerification::UnauthorizedKey { key_id, .. } if key_id == "k2"
    ));
}
