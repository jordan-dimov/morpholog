//! Storing external witnesses on a checkpoint.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_postgres::{Witness, WitnessScheme, attach_witness, load_checkpoint};

mod common;
use common::{reset_db, test_pool};

fn witness(i: usize) -> Witness {
    Witness {
        scheme: WitnessScheme::Rfc3161,
        proof: format!("proof-{i}"),
        submitted_to: format!("http://tsa{i}.example/tsr"),
    }
}

/// Attacker capability: none - two honest operators witnessing the same
/// head at once. The later attachment must not erase the earlier.
#[tokio::test]
async fn attachments_arriving_together_all_land() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    common::commit_entry(&pool, "w1").await;
    let cp = common::make_checkpoint(&pool).await;

    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..8 {
        let pool = pool.clone();
        let hash = cp.checkpoint_hash.clone();
        let tree_size = cp.tree_size;
        tasks.spawn(async move { attach_witness(&pool, tree_size, &hash, witness(i)).await });
    }
    while let Some(joined) = tasks.join_next().await {
        joined.unwrap().unwrap();
    }
    let stored = load_checkpoint(&pool, cp.tree_size).await.unwrap().unwrap();
    let mut proofs: Vec<_> = stored.witnesses.iter().map(|w| w.proof.clone()).collect();
    proofs.sort();
    assert_eq!(
        proofs,
        (0..8).map(|i| format!("proof-{i}")).collect::<Vec<_>>(),
        "every attachment survives"
    );

    // The same proof again is one witness, whatever endpoint served it.
    let mut again = witness(3);
    again.submitted_to = "http://mirror.example/tsr".into();
    let after = attach_witness(&pool, cp.tree_size, &cp.checkpoint_hash, again)
        .await
        .unwrap();
    assert_eq!(after.witnesses.len(), 8);

    // A proof for a head this chain does not hold is refused.
    let err = attach_witness(&pool, cp.tree_size, "sha256:not-this-head", witness(9))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no longer holds"), "{err}");
    let err = attach_witness(&pool, cp.tree_size + 5, &cp.checkpoint_hash, witness(9))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no checkpoint at tree size"),
        "{err}"
    );
}
