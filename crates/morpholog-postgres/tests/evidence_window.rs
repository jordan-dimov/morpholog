//! Windowed evidence packs end to end: export the interval between two real
//! checkpoints and verify it OFFLINE (no pool), the way a regulator holding
//! the prior period's anchor would. The realistic complement to the pure
//! tests in `pack.rs`: real rows and checkpoints, so the consistency and
//! inclusion proofs are exercised against genuine Merkle data. Tampering is
//! done by editing the pack's JSON, as an attacker holding the file would.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    Checkpoint, CheckpointOutcome, PgPool, WindowEvidencePack, WindowStart, WindowVerification,
    create_checkpoint, export_window, verify_window,
};

mod common;
use common::{dec, reset_db, subj, test_pool};

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
    match create_checkpoint(pool, None).await.unwrap() {
        CheckpointOutcome::Created(c) => c,
        other @ CheckpointOutcome::NoNewRows(_) => {
            panic!("expected a created checkpoint, got {other:?}")
        }
    }
}

/// Commit two entries, checkpoint (the prior period's anchor), commit two
/// more, checkpoint again (the window end), and export `[from, to)`.
async fn window_q1_q2(pool: &PgPool, tag: &str) -> (WindowEvidencePack, Checkpoint, Checkpoint) {
    for i in 0..2 {
        commit_entry(pool, &format!("{tag}_q1_{i}")).await;
    }
    let q1 = make_checkpoint(pool).await; // tree_size 2
    for i in 0..2 {
        commit_entry(pool, &format!("{tag}_q2_{i}")).await;
    }
    let q2 = make_checkpoint(pool).await; // tree_size 4
    let pack = export_window(
        pool,
        WindowStart::TreeSize(q1.tree_size),
        Some(q2.tree_size),
    )
    .await
    .unwrap();
    (pack, q1, q2)
}

fn edit_json(
    pack: &WindowEvidencePack,
    edit: impl FnOnce(&mut serde_json::Value),
) -> WindowEvidencePack {
    let mut v = serde_json::to_value(pack).unwrap();
    edit(&mut v);
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn a_window_extends_its_anchor_and_includes_its_rows() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (pack, q1, _q2) = window_q1_q2(&pool, "ok").await;

    assert_eq!(pack.manifest.from_tree_size, 2);
    assert_eq!(pack.manifest.to_tree_size, 4);
    assert_eq!(pack.rows.len(), 2);
    assert_eq!(pack.inclusion_proofs.len(), 2);

    // Offline, no pool: intact, and intact against the prior anchor.
    assert!(matches!(
        verify_window(&pack, None).unwrap(),
        WindowVerification::Intact {
            from_tree_size: 2,
            to_tree_size: 4,
            rows: 2
        }
    ));
    assert!(matches!(
        verify_window(&pack, Some(&q1)).unwrap(),
        WindowVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn a_tampered_window_row_is_caught_by_inclusion_not_consistency() {
    // The headline overclaim guard, end to end: the consistency proof is
    // untouched and genuine, yet editing a window row's body fails its
    // inclusion proof - consistency alone would never catch it.
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (pack, _q1, _q2) = window_q1_q2(&pool, "tamper").await;

    let tampered = edit_json(&pack, |v| {
        v["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    });
    assert!(matches!(
        verify_window(&tampered, None).unwrap(),
        WindowVerification::RowNotIncluded { .. }
    ));
}

#[tokio::test]
async fn a_corrupted_consistency_proof_is_an_inconsistent_extension() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (pack, _q1, _q2) = window_q1_q2(&pool, "incon").await;

    let broken = edit_json(&pack, |v| {
        v["consistency_proof"][0] = serde_json::json!(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        );
    });
    assert!(matches!(
        verify_window(&broken, None).unwrap(),
        WindowVerification::InconsistentExtension {
            from_tree_size: 2,
            to_tree_size: 4
        }
    ));
}

#[tokio::test]
async fn a_forged_anchor_is_an_anchor_mismatch() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (pack, _q1, _q2) = window_q1_q2(&pool, "anchor").await;

    // The pack's internal proof verifies, but the prior anchor the regulator
    // holds does not match the pack's from-checkpoint - a coordinated rewrite.
    let forged = Checkpoint {
        tree_size: 2,
        root_hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        prev_checkpoint_hash: None,
        checkpoint_hash: "forged".into(),
        signatures: Vec::new(),
    };
    assert!(matches!(
        verify_window(&pack, Some(&forged)).unwrap(),
        WindowVerification::AnchorMismatch { tree_size: 2, .. }
    ));
}

#[tokio::test]
async fn export_refuses_a_window_with_an_unknown_endpoint() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..2 {
        commit_entry(&pool, &format!("end_{i}")).await;
    }
    make_checkpoint(&pool).await; // only checkpoint, tree_size 2

    // `from` is not an existing checkpoint.
    assert!(matches!(
        export_window(&pool, WindowStart::TreeSize(1), Some(2)).await,
        Err(morpholog_postgres::PgError::NoCheckpoint)
    ));
    // `to` is not an existing checkpoint.
    assert!(matches!(
        export_window(&pool, WindowStart::TreeSize(2), Some(99)).await,
        Err(morpholog_postgres::PgError::NoCheckpoint)
    ));
    // `from` is not strictly before `to` (latest == from here).
    assert!(matches!(
        export_window(&pool, WindowStart::TreeSize(2), None).await,
        Err(morpholog_postgres::PgError::InvalidState(_))
    ));
}

#[tokio::test]
async fn export_from_a_diverged_anchor_refuses() {
    // `--from-anchor` is the trust object: if the stored start checkpoint no
    // longer matches the anchor the operator holds, export must refuse rather
    // than silently export from the diverged stored checkpoint.
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (_pack, q1, q2) = window_q1_q2(&pool, "div").await;

    // The genuine anchor exports fine.
    assert!(
        export_window(&pool, WindowStart::Anchor(q1.clone()), Some(q2.tree_size))
            .await
            .is_ok()
    );

    // An anchor at the same size but a different tree head is refused.
    let forged = Checkpoint {
        tree_size: q1.tree_size,
        root_hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        prev_checkpoint_hash: None,
        checkpoint_hash: "forged".into(),
        signatures: Vec::new(),
    };
    assert!(matches!(
        export_window(&pool, WindowStart::Anchor(forged), Some(q2.tree_size)).await,
        Err(morpholog_postgres::PgError::AnchorDivergedFromStart { tree_size }) if tree_size == q1.tree_size
    ));
}
