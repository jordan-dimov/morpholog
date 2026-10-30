//! `create_checkpoint` with a transition it must cover: a successful
//! outcome means the head returned contains that transition. A committed
//! row the horizon withholds is refused with nothing recorded, so a
//! deliverer can retry it; an id the log does not hold is refused as not
//! found.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_postgres::{
    CheckpointOutcome, CheckpointSigner, PgError, PgPool, create_checkpoint, generate_signing_key,
    load_checkpoint, render_public_key,
};
use uuid::Uuid;

mod common;
use common::{authorize_signing_key, reset_db, test_pool};

async fn commit_row(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    common::insert_in_flight_audit_row(&mut tx, id).await;
    tx.commit().await.unwrap();
    id
}

async fn checkpoints_recorded(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM morpholog.audit_checkpoints")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn created(outcome: CheckpointOutcome) -> i64 {
    match outcome {
        CheckpointOutcome::Created(c) => c.tree_size,
        CheckpointOutcome::NoNewRows(c) => panic!("expected a new checkpoint, got the head {c:?}"),
    }
}

#[tokio::test]
async fn a_withheld_transition_is_refused_then_covered_once_the_older_writer_finishes() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Writer A started first and is still in flight, so the horizon
    // trails it and withholds everything committed after its start.
    let mut older = pool.begin().await.unwrap();
    common::insert_in_flight_audit_row(&mut older, Uuid::now_v7()).await;
    let wanted = commit_row(&pool).await;

    let err = create_checkpoint(&pool, None, Some(wanted))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PgError::TransitionNotCovered { id, tree_size: 0 } if id == wanted),
        "a committed row beyond the horizon is not covered: {err}"
    );
    assert_eq!(
        checkpoints_recorded(&pool).await,
        0,
        "a refusal records nothing"
    );

    older.commit().await.unwrap();
    let tree_size = created(create_checkpoint(&pool, None, Some(wanted)).await.unwrap());
    assert_eq!(
        tree_size, 2,
        "both rows are in the prefix once A has finished"
    );
}

#[tokio::test]
async fn an_id_the_log_does_not_hold_is_not_found_and_records_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_row(&pool).await;

    let unknown = Uuid::now_v7();
    let err = create_checkpoint(&pool, None, Some(unknown))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PgError::TransitionNotFound(id) if id == unknown),
        "{err}"
    );
    assert_eq!(checkpoints_recorded(&pool).await, 0);
}

#[tokio::test]
async fn a_head_that_already_covers_the_transition_is_returned_unchanged() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let wanted = commit_row(&pool).await;
    let head = created(create_checkpoint(&pool, None, Some(wanted)).await.unwrap());

    match create_checkpoint(&pool, None, Some(wanted)).await.unwrap() {
        CheckpointOutcome::NoNewRows(c) => assert_eq!(c.tree_size, head),
        CheckpointOutcome::Created(c) => panic!("the existing head already covered it: {c:?}"),
    }
}

#[tokio::test]
async fn a_head_that_does_not_cover_the_transition_is_refused_with_nothing_new_to_checkpoint() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // Two authorised keys: the first signs the head, the second is what
    // the refused run would add. A re-sign by the first key is
    // deduplicated, so it could not show the signing path was reached.
    let mut signers = Vec::new();
    for key_id in ["first-key", "second-key"] {
        let key = generate_signing_key();
        authorize_signing_key(
            &pool,
            key_id,
            "audit_checkpoint_v1",
            &render_public_key(&key.verifying_key()),
        )
        .await;
        signers.push(CheckpointSigner {
            key_id: key_id.into(),
            key,
        });
    }
    let (first, second) = (&signers[0], &signers[1]);
    let head = created(create_checkpoint(&pool, Some(first), None).await.unwrap());

    // Nothing new can be checkpointed while A is in flight, so a plain run
    // would return the head, and one with the second key would also sign
    // it. The covering run must do neither.
    let mut older = pool.begin().await.unwrap();
    common::insert_in_flight_audit_row(&mut older, Uuid::now_v7()).await;
    let wanted = commit_row(&pool).await;
    for signer in [Some(second), None] {
        let err = create_checkpoint(&pool, signer, Some(wanted))
            .await
            .unwrap_err();
        assert!(
            matches!(err, PgError::TransitionNotCovered { id, tree_size } if id == wanted && tree_size == head),
            "{err}"
        );
    }
    assert_eq!(checkpoints_recorded(&pool).await, 1);
    let unchanged = load_checkpoint(&pool, head).await.unwrap().unwrap();
    let key_ids: Vec<&str> = unchanged
        .signatures
        .iter()
        .map(|s| s.key_id.as_str())
        .collect();
    assert_eq!(key_ids, ["first-key"], "the refused run added no signature");
    assert!(unchanged.witnesses.is_empty());

    older.commit().await.unwrap();
    let tree_size = created(
        create_checkpoint(&pool, Some(second), Some(wanted))
            .await
            .unwrap(),
    );
    assert_eq!(tree_size, head + 2);
}
