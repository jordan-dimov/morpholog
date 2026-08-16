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
/// it - the legitimate starting point most of these tests then attack.
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
    // Quiet horizon, nothing withheld: the plain refusal, about the key -
    // never the truncated-prefix diagnosis.
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
    // the authorisation commits at or above the resume horizon - withheld
    // from the checkpoint's stable prefix, not missing.
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

    // The remedy the message names is true: once the older transaction
    // ends the horizon advances and the same signer succeeds.
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

// The race this pins: a signing run resolves authority over its own
// (possibly horizon-truncated) prefix, but its signature attaches to the
// chain head, which may sit past a revocation. The signature must be
// judged against the head it attests, or signing produces a checkpoint
// `verify` itself rejects as UnauthorizedKey.
#[tokio::test]
async fn attaching_a_signature_resolves_authority_as_of_the_head_actually_signed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        // The head-advancing checkpoint below rides the asserted horizon
        // ignoring our (superuser, census-exempt) interferer session.
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

    // Sign without the assertion: this run's horizon trails the
    // interferer, so its own prefix (tree_size 1) still shows k1
    // authorised - but the signature would attach to the head at 2,
    // where k1 is revoked. Authority must be judged as of 2.
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

#[tokio::test]
async fn a_genuine_signature_by_an_unauthorized_key_is_unauthorized_key() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Sign legitimately, then tamper the signatures column with a genuine
    // signature by an unauthorised key (the merkle root is blind to it -
    // signatures are not in the tree head, so only authority catches it).
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
