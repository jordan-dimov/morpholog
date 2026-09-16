//! The verifier's signature policy, layered over the intrinsic verdict.
//!
//! Attacker capability modelled: a credential able to propose brings an
//! ungated programme that admits its own `AuditSigningKey`, so the
//! keys-as-claims fold authorises a key the operator never chose. The
//! log is then intact and genuinely signed, and only the verifier's own
//! pin can say "not by the key I trust". The pin is an intersection with
//! the fold, never a substitute: a pinned key the log never authorised
//! stays an intrinsic refusal, not a policy one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{authorize_signing_key, commit_entry, make_checkpoint_at, reset_db, test_pool};

use morpholog_postgres::{
    Checkpoint, CheckpointOutcome, CheckpointSigner, PgPool, SignaturePolicy,
    SignaturePolicyViolation, TreeHead, TreeHeadSignature, TreeVerification, create_checkpoint,
    generate_signing_key, render_public_key, render_signature, sign_tree_head,
    verify_audit_tree_under,
};

const PURPOSE: &str = "audit_checkpoint_v1";

fn unsigned(tree_size: i64) -> Checkpoint {
    Checkpoint {
        tree_size,
        root_hash: "sha256:00".into(),
        prev_checkpoint_hash: None,
        checkpoint_hash: "sha256:00".into(),
        signatures: vec![],
    }
}

fn signed_by(tree_size: i64, keys: &[&str]) -> Checkpoint {
    let mut c = unsigned(tree_size);
    c.signatures = keys
        .iter()
        .map(|k| TreeHeadSignature {
            key_id: "k".into(),
            purpose: PURPOSE.into(),
            public_key: (*k).into(),
            signature: "ed25519-sig:00".into(),
        })
        .collect();
    c
}

fn policy(from: i64, key: Option<&str>) -> SignaturePolicy {
    SignaturePolicy {
        from_tree_size: from,
        required_public_key: key.map(str::to_string),
    }
}

#[test]
fn the_threshold_asks_of_checkpoints_at_or_after_it_and_no_others() {
    let chain = [unsigned(1), unsigned(2), unsigned(3)];
    for (from, expect) in [(1, Some(1)), (2, Some(2)), (3, Some(3)), (4, None)] {
        assert_eq!(
            policy(from, None).violation(&chain),
            expect.map(|tree_size| SignaturePolicyViolation::SignatureRequired { tree_size }),
            "from {from}"
        );
        assert_eq!(
            policy(from, None).unsigned_at_or_after(&chain),
            expect,
            "presence-only, from {from}"
        );
    }
}

#[test]
fn a_pin_is_satisfied_by_any_one_signature_by_that_key() {
    let chain = [signed_by(1, &["A", "H"]), signed_by(2, &["A"])];
    assert_eq!(policy(0, Some("H")).violation(&chain[..1]), None);
    assert_eq!(
        policy(0, Some("H")).violation(&chain),
        Some(SignaturePolicyViolation::SigningKeyRequired {
            tree_size: 2,
            public_key: "H".into(),
        })
    );
    // A missing signature outranks a missing pinned key.
    let chain = [unsigned(1), signed_by(2, &["A"])];
    assert_eq!(
        policy(0, Some("H")).violation(&chain),
        Some(SignaturePolicyViolation::SignatureRequired { tree_size: 1 })
    );
    // The threshold exempts the pin too.
    assert_eq!(policy(3, Some("H")).violation(&chain), None);
}

async fn signer(pool: &PgPool, key_id: &str) -> (CheckpointSigner, String) {
    let key = generate_signing_key();
    let public = render_public_key(&key.verifying_key());
    authorize_signing_key(pool, key_id, PURPOSE, &public).await;
    (
        CheckpointSigner {
            key_id: key_id.into(),
            key,
        },
        public,
    )
}

async fn head(pool: &PgPool, signer: &CheckpointSigner) -> Checkpoint {
    match create_checkpoint(pool, Some(signer), None).await.unwrap() {
        CheckpointOutcome::Created(c) | CheckpointOutcome::NoNewRows(c) => c,
    }
}

#[tokio::test]
async fn a_rogue_authorised_signer_passes_intrinsically_and_fails_the_pin() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "x1").await;
    let (honest, honest_key) = signer(&pool, "honest").await;
    let (rogue, _) = signer(&pool, "rogue").await;

    let cp = head(&pool, &rogue).await;
    assert_eq!(cp.signatures.len(), 1);
    // Intrinsically the log is intact: the rogue key IS authorised.
    assert!(matches!(
        verify_audit_tree_under(&pool, None, None).await.unwrap(),
        TreeVerification::Intact { .. }
    ));
    // The pin narrows which authorised signer the verifier accepts.
    let pinned = policy(0, Some(&honest_key));
    assert_eq!(
        verify_audit_tree_under(&pool, None, Some(&pinned))
            .await
            .unwrap(),
        TreeVerification::SigningKeyRequired {
            tree_size: cp.tree_size,
            public_key: honest_key.clone(),
        }
    );
    // The honest key's signature attached alongside satisfies it; the
    // rogue signature is not held against the head.
    let both = head(&pool, &honest).await;
    assert_eq!(both.signatures.len(), 2);
    assert!(matches!(
        verify_audit_tree_under(&pool, None, Some(&pinned))
            .await
            .unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn a_pinned_key_the_log_never_authorised_is_an_intrinsic_refusal_not_policy() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "x1").await;
    let (authorised, _) = signer(&pool, "k1").await;
    let cp = head(&pool, &authorised).await;

    // The verifier pins a key that signed the head out of band and was
    // never admitted: the pin must not launder it into acceptance.
    let pinned_key = generate_signing_key();
    let pinned_public = render_public_key(&pinned_key.verifying_key());
    let sig = TreeHeadSignature {
        key_id: "pinned".into(),
        purpose: PURPOSE.into(),
        public_key: pinned_public.clone(),
        signature: render_signature(&sign_tree_head(
            &pinned_key,
            PURPOSE,
            "pinned",
            &TreeHead {
                tree_size: cp.tree_size,
                root_hash: &cp.root_hash,
                prev_checkpoint_hash: cp.prev_checkpoint_hash.as_deref(),
                checkpoint_hash: &cp.checkpoint_hash,
            },
        )),
    };
    sqlx::query("UPDATE morpholog.audit_checkpoints SET signatures = $1::jsonb")
        .bind(serde_json::to_string(&vec![sig]).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        verify_audit_tree_under(&pool, None, Some(&policy(0, Some(&pinned_public))))
            .await
            .unwrap(),
        TreeVerification::UnauthorizedKey { .. }
    ));
}

#[tokio::test]
async fn honest_history_before_signing_began_passes_under_a_threshold() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "x1").await;
    make_checkpoint_at(&pool, 1).await;
    commit_entry(&pool, "x2").await;
    make_checkpoint_at(&pool, 2).await;
    for (from, expect) in [(1, Some(1)), (2, Some(2)), (3, None)] {
        let verdict = verify_audit_tree_under(&pool, None, Some(&policy(from, None)))
            .await
            .unwrap();
        match expect {
            Some(tree_size) => assert_eq!(
                verdict,
                TreeVerification::SignatureRequired { tree_size },
                "from {from}"
            ),
            None => assert!(
                matches!(verdict, TreeVerification::Intact { .. }),
                "from {from}: {verdict:?}"
            ),
        }
    }
}

#[tokio::test]
async fn a_signed_anchor_satisfies_the_pin_when_the_stored_copy_is_stripped() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "x1").await;
    let (honest, honest_key) = signer(&pool, "honest").await;
    let anchor = head(&pool, &honest).await;
    // The database copy loses its signatures; the verifier still holds
    // the signed head. Intrinsically attributable through the anchor,
    // and the policy judges what the verifier holds.
    sqlx::query("UPDATE morpholog.audit_checkpoints SET signatures = '[]'::jsonb")
        .execute(&pool)
        .await
        .unwrap();
    let pinned = policy(0, Some(&honest_key));
    assert_eq!(
        verify_audit_tree_under(&pool, None, Some(&pinned))
            .await
            .unwrap(),
        TreeVerification::SignatureRequired {
            tree_size: anchor.tree_size
        },
        "without the anchor the stored copy is unsigned"
    );
    assert!(matches!(
        verify_audit_tree_under(&pool, Some(anchor), Some(&pinned))
            .await
            .unwrap(),
        TreeVerification::Intact { .. }
    ));
}
