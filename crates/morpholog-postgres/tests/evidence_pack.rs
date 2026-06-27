//! Evidence packs: export a checkpointed prefix of the audit log and
//! verify it OFFLINE (no pool), the way a third party would. The realistic
//! complement to the pure envelope tests in `pack.rs`: here the rows and
//! checkpoints are real, so the cryptographic verdicts (intact, tampered,
//! chain-broken, anchor-mismatch) are exercised end to end. Tampering is
//! done by editing the pack's JSON, exactly as an attacker holding the
//! file would.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    Checkpoint, CheckpointOutcome, EvidencePack, PackError, PgPool, TreeVerification,
    create_checkpoint, export_pack, verify_pack,
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

/// Re-serialise a pack to JSON, apply an edit, and parse it back - the
/// attacker-with-the-file path.
fn edit_json(pack: &EvidencePack, edit: impl FnOnce(&mut serde_json::Value)) -> EvidencePack {
    let mut v = serde_json::to_value(pack).unwrap();
    edit(&mut v);
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn export_then_verify_offline_is_intact_and_matches_the_anchor() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..3 {
        commit_entry(&pool, &format!("e{i}")).await;
    }
    let anchor = make_checkpoint(&pool).await;

    let pack = export_pack(&pool, None).await.unwrap();
    assert_eq!(pack.manifest.tree_size, 3);
    assert_eq!(pack.rows.len(), 3);

    // Offline, no pool: intact, and intact against the external anchor.
    assert!(matches!(
        verify_pack(&pack, None).unwrap(),
        TreeVerification::Intact { tree_size: 3, .. }
    ));
    assert!(matches!(
        verify_pack(&pack, Some(&anchor)).unwrap(),
        TreeVerification::Intact { .. }
    ));

    // Row order in the file is irrelevant - the verifier re-sorts.
    let mut shuffled = pack.clone();
    shuffled.rows.reverse();
    assert!(matches!(
        verify_pack(&shuffled, None).unwrap(),
        TreeVerification::Intact { .. }
    ));
}

#[tokio::test]
async fn editing_a_packed_row_is_tampered() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..3 {
        commit_entry(&pool, &format!("t{i}")).await;
    }
    make_checkpoint(&pool).await;
    let pack = export_pack(&pool, None).await.unwrap();

    let tampered = edit_json(&pack, |v| {
        v["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    });
    assert!(matches!(
        verify_pack(&tampered, None).unwrap(),
        TreeVerification::Tampered { tree_size: 3, .. }
    ));
}

#[tokio::test]
async fn a_removed_or_extra_row_is_malformed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..3 {
        commit_entry(&pool, &format!("m{i}")).await;
    }
    make_checkpoint(&pool).await;
    let pack = export_pack(&pool, None).await.unwrap();

    let mut short = pack.clone();
    short.rows.pop();
    assert!(matches!(
        verify_pack(&short, None),
        Err(PackError::Malformed { .. })
    ));

    let mut extra = pack.clone();
    extra.rows.push(pack.rows[0].clone());
    assert!(matches!(
        verify_pack(&extra, None),
        Err(PackError::Malformed { .. })
    ));
}

#[tokio::test]
async fn editing_an_earlier_checkpoint_breaks_the_chain() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "c0").await;
    commit_entry(&pool, "c1").await;
    make_checkpoint(&pool).await; // tree_size 2
    commit_entry(&pool, "c2").await;
    make_checkpoint(&pool).await; // tree_size 3, covering
    let pack = export_pack(&pool, None).await.unwrap();
    assert_eq!(pack.checkpoints.len(), 2);

    // Rewrite the earlier checkpoint's root without fixing its hash. The
    // manifest references the covering (last) checkpoint, so it still
    // agrees - the break is in the chain, not the envelope.
    let mut broken = pack.clone();
    broken.checkpoints[0].root_hash =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".into();
    assert!(matches!(
        verify_pack(&broken, None).unwrap(),
        TreeVerification::ChainBroken { .. }
    ));
}

#[tokio::test]
async fn an_older_anchor_in_the_chain_is_intact_an_outside_anchor_mismatches() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "a0").await;
    commit_entry(&pool, "a1").await;
    let first = make_checkpoint(&pool).await; // tree_size 2
    commit_entry(&pool, "a2").await;
    make_checkpoint(&pool).await; // tree_size 3, covering
    let pack = export_pack(&pool, None).await.unwrap();

    // An older checkpoint, present in the pack's chain, still matches.
    assert!(matches!(
        verify_pack(&pack, Some(&first)).unwrap(),
        TreeVerification::Intact { .. }
    ));

    // An anchor at a size present in the pack but with a different
    // identity is the coordinated-rewrite signal.
    let forged = Checkpoint {
        tree_size: 2,
        root_hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        prev_checkpoint_hash: None,
        checkpoint_hash: "forged".into(),
        signatures: Vec::new(),
    };
    assert!(matches!(
        verify_pack(&pack, Some(&forged)).unwrap(),
        TreeVerification::AnchorMismatch { tree_size: 2, .. }
    ));
}

#[tokio::test]
async fn export_refuses_without_a_covering_checkpoint() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "n0").await;

    // No checkpoint at all.
    assert!(matches!(
        export_pack(&pool, None).await,
        Err(morpholog_postgres::PgError::NoCheckpoint)
    ));

    // A checkpoint exists, but not at the requested exact size.
    make_checkpoint(&pool).await; // tree_size 1
    assert!(matches!(
        export_pack(&pool, Some(99)).await,
        Err(morpholog_postgres::PgError::NoCheckpoint)
    ));
}

#[tokio::test]
async fn export_refuses_when_a_covered_row_is_missing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    for i in 0..3 {
        commit_entry(&pool, &format!("g{i}")).await;
    }
    make_checkpoint(&pool).await; // commits to 3 rows

    // Delete a covered audit row directly - the checkpoint still claims 3.
    // The exporter must fail rather than mint a known-incomplete pack.
    // (Clear the outbox FK to that transition first.)
    let first =
        "SELECT transition_id FROM morpholog.audit ORDER BY committed_at, transition_id LIMIT 1";
    sqlx::query(&format!(
        "DELETE FROM morpholog.outbox WHERE transition_id IN ({first})"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "DELETE FROM morpholog.audit WHERE transition_id = ({first})"
    ))
    .execute(&pool)
    .await
    .unwrap();

    assert!(matches!(
        export_pack(&pool, None).await,
        Err(morpholog_postgres::PgError::InvalidState(_))
    ));
}
