//! Selective evidence packs end to end: disclose a chosen subset of real
//! committed transitions, verify OFFLINE (no pool), and pin the
//! reveal-nothing property against genuine audit rows. Tampering is done
//! by editing the pack's JSON, as an attacker holding the file would.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    CheckpointOutcome, PgError, PgPool, SelectiveEvidencePack, SelectiveVerification,
    create_checkpoint, export_selective, verify_selective,
};
use uuid::Uuid;

mod common;
use common::{dec, reset_db, subj, test_pool};

async fn commit_entry(pool: &PgPool, id: &str) -> Uuid {
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
    common::expect_committed(outcome)
}

async fn checkpointed(pool: &PgPool) -> morpholog_postgres::Checkpoint {
    match create_checkpoint(pool, None, None).await.unwrap() {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint, got {other:?}")
        }
    }
}

fn edit_json(
    pack: &SelectiveEvidencePack,
    edit: impl FnOnce(&mut serde_json::Value),
) -> SelectiveEvidencePack {
    let mut v = serde_json::to_value(pack).unwrap();
    edit(&mut v);
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn a_disclosed_subset_verifies_intact_and_reveals_nothing_else() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let shown_a = commit_entry(&pool, "sel_a").await;
    let hidden = commit_entry(&pool, "sel_hidden").await;
    let shown_b = commit_entry(&pool, "sel_b").await;
    let covering = checkpointed(&pool).await;

    let pack = export_selective(&pool, None, &[shown_a, shown_b])
        .await
        .unwrap();
    assert_eq!(
        verify_selective(&pack, Some(&covering)).unwrap(),
        SelectiveVerification::Intact {
            tree_size: 3,
            rows_disclosed: 2,
        }
    );

    // The reveal-nothing property over real rows: neither the undisclosed
    // transition's id nor any of its business payload appears anywhere in
    // the pack bytes - not the entry subject, the accounts, or the claims.
    let bytes = serde_json::to_string(&pack).unwrap();
    assert!(bytes.contains(&shown_a.to_string()));
    assert!(bytes.contains("cash_sel_b"));
    assert!(!bytes.contains(&hidden.to_string()), "id leaked");
    assert!(!bytes.contains("sel_hidden"), "payload leaked");
}

#[tokio::test]
async fn a_tampered_disclosed_row_is_row_not_included() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let shown = commit_entry(&pool, "tam_a").await;
    commit_entry(&pool, "tam_b").await;
    checkpointed(&pool).await;

    let pack = export_selective(&pool, None, &[shown]).await.unwrap();
    let tampered = edit_json(&pack, |v| {
        v["rows"][0]["arguments"][5] = serde_json::json!({"type": "decimal", "value": "999"});
    });
    assert!(matches!(
        verify_selective(&tampered, None),
        Ok(SelectiveVerification::RowNotIncluded { leaf_index: 0 })
    ));
}

#[tokio::test]
async fn export_refuses_an_unknown_or_uncovered_transition() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "cov_a").await;
    let covering = checkpointed(&pool).await;
    // Committed, but after the covering checkpoint: not in its prefix.
    let late = commit_entry(&pool, "cov_late").await;

    let ghost = Uuid::from_u128(7);
    let err = export_selective(&pool, Some(covering.tree_size), &[ghost])
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::TransitionNotCovered { id, .. } if id == ghost));

    // The late transition EXISTS in the audit log - the error must say
    // "not covered by this checkpoint", not "not found".
    let err = export_selective(&pool, Some(covering.tree_size), &[late])
        .await
        .unwrap_err();
    assert!(
        matches!(err, PgError::TransitionNotCovered { id, tree_size } if id == late && tree_size == covering.tree_size)
    );
}

#[tokio::test]
async fn export_refuses_an_empty_or_duplicate_selection() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let only = commit_entry(&pool, "dup_a").await;
    checkpointed(&pool).await;

    let err = export_selective(&pool, None, &[]).await.unwrap_err();
    assert!(matches!(err, PgError::InvalidState(msg) if msg.contains("at least one row")));

    let err = export_selective(&pool, None, &[only, only])
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::InvalidState(msg) if msg.contains("more than once")));
}

#[tokio::test]
async fn a_forged_anchor_is_an_anchor_mismatch() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let shown = commit_entry(&pool, "anc_a").await;
    let covering = checkpointed(&pool).await;

    let pack = export_selective(&pool, None, &[shown]).await.unwrap();
    let mut forged = covering.clone();
    forged.root_hash = format!("sha256:{}", "f".repeat(64));
    assert!(matches!(
        verify_selective(&pack, Some(&forged)),
        Ok(SelectiveVerification::AnchorMismatch { .. })
    ));
}

#[tokio::test]
async fn export_refuses_without_a_covering_checkpoint() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let id = commit_entry(&pool, "nochk_a").await;

    let err = export_selective(&pool, None, &[id]).await.unwrap_err();
    assert!(matches!(err, PgError::NoCheckpoint));
}
