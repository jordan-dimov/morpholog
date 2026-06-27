//! The keys-as-claims authority layer: a signed checkpoint verifies as
//! intact only when its signing key was admitted as an `AuditSigningKey`
//! claim as of the checkpoint's prefix. A genuine signature by a key the
//! ledger never authorised is `UnauthorizedKey`, not intact - the
//! signature is real, the signer was not permitted. Signing itself refuses
//! an unauthorised key, so an unauthorised signature only arises from
//! tampering with the signatures column or an externally-supplied anchor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use morpholog_postgres::{
    Checkpoint, CheckpointOutcome, CheckpointSigner, TreeHead, TreeHeadSignature, TreeVerification,
    create_checkpoint, export_pack, generate_signing_key, render_public_key, render_signature,
    sign_tree_head, verify_audit_tree, verify_pack,
};

mod common;
use common::{authorize_signing_key, reset_db, test_pool};

const PURPOSE: &str = "audit_checkpoint_v1";

/// A genuine signature over a checkpoint's tree head by an arbitrary key -
/// what an attacker who edits the (out-of-tree) signatures column, or a
/// forged anchor, would carry.
fn signature_over(key: &SigningKey, key_id: &str, cp: &Checkpoint) -> TreeHeadSignature {
    let head = TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_deref(),
        checkpoint_hash: &cp.checkpoint_hash,
    };
    TreeHeadSignature {
        key_id: key_id.into(),
        purpose: PURPOSE.into(),
        public_key: render_public_key(&key.verifying_key()),
        signature: render_signature(&sign_tree_head(key, PURPOSE, key_id, &head)),
    }
}

fn created(outcome: CheckpointOutcome) -> Checkpoint {
    match outcome {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint: {other:?}")
        }
    }
}

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
    let cp = created(create_checkpoint(&pool, Some(&signer)).await.unwrap());
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
    let err = create_checkpoint(&pool, Some(&interloper))
        .await
        .expect_err("signing with an unauthorised key must be refused, not produced");
    assert!(
        err.to_string().contains("not authorised"),
        "expected an authority refusal, got: {err}"
    );
}

#[tokio::test]
async fn a_genuine_signature_by_an_unauthorized_key_is_unauthorized_key() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Sign legitimately with k1, then tamper the signatures column with a
    // genuine signature by an unauthorised key (the merkle root is blind to
    // it - signatures are not in the tree head, so only authority catches it).
    let k1 = generate_signing_key();
    authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&k1.verifying_key()),
    )
    .await;
    let cp = created(
        create_checkpoint(
            &pool,
            Some(&CheckpointSigner {
                key_id: "k1".into(),
                key: k1,
            }),
        )
        .await
        .unwrap(),
    );

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

    let key = generate_signing_key();
    authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&key.verifying_key()),
    )
    .await;
    create_checkpoint(
        &pool,
        Some(&CheckpointSigner {
            key_id: "k1".into(),
            key,
        }),
    )
    .await
    .unwrap();

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

#[tokio::test]
async fn a_signed_anchor_is_verified_even_when_the_stored_signature_is_stripped() {
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
    let signed_anchor = created(
        create_checkpoint(
            &pool,
            Some(&CheckpointSigner {
                key_id: "k1".into(),
                key,
            }),
        )
        .await
        .unwrap(),
    );

    // Attacker strips the signature from the database; the operator still
    // holds the signed anchor. The anchor's own signature is verified, so
    // the tree is attributable despite the stripped database copy.
    sqlx::query("UPDATE morpholog.audit_checkpoints SET signatures = '[]'::jsonb")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        verify_audit_tree(&pool, Some(signed_anchor)).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn a_signed_anchor_with_a_corrupted_signature_is_caught() {
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
    let mut anchor = created(
        create_checkpoint(
            &pool,
            Some(&CheckpointSigner {
                key_id: "k1".into(),
                key,
            }),
        )
        .await
        .unwrap(),
    );
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

    let k1 = generate_signing_key();
    authorize_signing_key(
        &pool,
        "k1",
        PURPOSE,
        &render_public_key(&k1.verifying_key()),
    )
    .await;
    let cp = created(
        create_checkpoint(
            &pool,
            Some(&CheckpointSigner {
                key_id: "k1".into(),
                key: k1,
            }),
        )
        .await
        .unwrap(),
    );

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
