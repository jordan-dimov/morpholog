//! The keys-as-claims authority layer: a signed checkpoint verifies as
//! intact only when its signing key was admitted as an `AuditSigningKey`
//! claim as of the checkpoint's prefix. A genuine signature by a key the
//! ledger never authorised is `UnauthorizedKey`, not intact - the
//! signature is real, the signer was not permitted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_postgres::{
    CheckpointOutcome, CheckpointSigner, TreeVerification, create_checkpoint, export_pack,
    generate_signing_key, render_public_key, verify_audit_tree, verify_pack,
};

mod common;
use common::{authorize_signing_key, reset_db, test_pool};

const PURPOSE: &str = "audit_checkpoint_v1";

#[tokio::test]
async fn a_checkpoint_signed_by_an_authorized_key_verifies_intact() {
    let pool = test_pool().await;
    reset_db(&pool).await;

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
    match create_checkpoint(&pool, Some(&signer)).await.unwrap() {
        CheckpointOutcome::Created(c) => assert_eq!(c.signatures.len(), 1),
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint: {other:?}")
        }
    };

    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn a_genuine_signature_by_an_unauthorized_key_is_unauthorized_key() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Authorise one key, then sign with a different, genuine key under the
    // same key id: the signature verifies, but the public key was never
    // authorised, so the verdict is UnauthorizedKey, not Intact.
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
    create_checkpoint(&pool, Some(&interloper)).await.unwrap();

    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::UnauthorizedKey { key_id, .. } if key_id == "k1"
    ));
}

#[tokio::test]
async fn an_offline_pack_resolves_authority_from_its_own_rows() {
    let pool = test_pool().await;
    reset_db(&pool).await;

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
    create_checkpoint(&pool, Some(&signer)).await.unwrap();

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

    create_checkpoint(&pool, None).await.unwrap();
    assert!(matches!(
        verify_audit_tree(&pool, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
}
