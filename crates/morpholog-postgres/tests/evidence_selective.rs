//! Selective evidence packs end to end: disclose a chosen subset of real
//! committed transitions, verify OFFLINE (no pool), and pin the
//! reveal-nothing property against genuine audit rows. Tampering is done
//! by editing the pack's JSON, as an attacker holding the file would.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_postgres::{PgError, SelectiveVerification, export_selective, verify_selective};
use uuid::Uuid;

mod common;
use common::{reset_db, test_pool};

#[tokio::test]
async fn a_disclosed_subset_verifies_intact_and_reveals_nothing_else() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let shown_a = common::commit_entry(&pool, "sel_a").await;
    let hidden = common::commit_entry(&pool, "sel_hidden").await;
    let shown_b = common::commit_entry(&pool, "sel_b").await;
    let covering = common::make_checkpoint(&pool).await;

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
    let shown = common::commit_entry(&pool, "tam_a").await;
    common::commit_entry(&pool, "tam_b").await;
    common::make_checkpoint(&pool).await;

    let pack = export_selective(&pool, None, &[shown]).await.unwrap();
    let tampered = common::edit_json(&pack, |v| {
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
    common::commit_entry(&pool, "cov_a").await;
    let covering = common::make_checkpoint(&pool).await;
    // Committed, but after the covering checkpoint: not in its prefix.
    let late = common::commit_entry(&pool, "cov_late").await;

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
    let only = common::commit_entry(&pool, "dup_a").await;
    common::make_checkpoint(&pool).await;

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
    let shown = common::commit_entry(&pool, "anc_a").await;
    let covering = common::make_checkpoint(&pool).await;

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
    let id = common::commit_entry(&pool, "nochk_a").await;

    let err = export_selective(&pool, None, &[id]).await.unwrap_err();
    assert!(matches!(err, PgError::NoCheckpoint));
}
