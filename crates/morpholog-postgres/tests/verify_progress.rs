//! A long `audit verify` says it is working: the replay and the tree pass
//! each report the rows they have read, page by page, ending at the rows
//! they set out to read.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{expect_committed, propose_pg_with_test_actor, reset_db, subj, test_pool};
use morpholog_examples::approval_controls;
use morpholog_postgres::{
    VerifyPass, VerifyProgress, create_checkpoint, verify_audit_tree_with_chain, verify_replay,
};

#[tokio::test]
async fn both_passes_report_progress_to_their_totals() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = common::pg_program(approval_controls::program());
    for doc in ["doc_1", "doc_2", "doc_3"] {
        expect_committed(
            propose_pg_with_test_actor(
                &pool,
                &program,
                &approval_controls::submit_document(),
                vec![subj(doc), subj("vendor_onboarding")],
            )
            .await
            .unwrap(),
        );
    }
    create_checkpoint(&pool, None, None).await.unwrap();

    let mut seen: Vec<VerifyProgress> = Vec::new();
    verify_replay(&pool, &mut |p| seen.push(p)).await.unwrap();
    verify_audit_tree_with_chain(&pool, None, None, &mut |p| seen.push(p))
        .await
        .unwrap();

    let last = |pass| seen.iter().rfind(|p| p.pass == pass).copied();
    for pass in [VerifyPass::Replay, VerifyPass::Tree] {
        assert_eq!(
            last(pass),
            Some(VerifyProgress {
                pass,
                done: 3,
                total: 3
            }),
            "{seen:?}"
        );
    }
}

// A transition after the latest checkpoint is in the replay but not in the
// tree: the tree pass still ends at the checkpoint's size.
#[tokio::test]
async fn the_tree_pass_ends_at_the_checkpoint_when_later_rows_exist() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let program = common::pg_program(approval_controls::program());
    for doc in ["doc_1", "doc_2", "doc_3"] {
        submit(&pool, &program, doc).await;
    }
    create_checkpoint(&pool, None, None).await.unwrap();
    submit(&pool, &program, "doc_4").await;

    let mut seen: Vec<VerifyProgress> = Vec::new();
    verify_audit_tree_with_chain(&pool, None, None, &mut |p| seen.push(p))
        .await
        .unwrap();
    assert_eq!(
        seen.last().copied(),
        Some(VerifyProgress {
            pass: VerifyPass::Tree,
            done: 3,
            total: 3
        }),
        "{seen:?}"
    );
    assert!(seen.iter().all(|p| p.done <= 3), "{seen:?}");
}

async fn submit(
    pool: &morpholog_postgres::PgPool,
    program: &morpholog_postgres::PgProgram,
    doc: &str,
) {
    expect_committed(
        propose_pg_with_test_actor(
            pool,
            program,
            &approval_controls::submit_document(),
            vec![subj(doc), subj("vendor_onboarding")],
        )
        .await
        .unwrap(),
    );
}
