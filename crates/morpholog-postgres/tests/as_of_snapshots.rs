//! An as-of read that starts from a kept snapshot reads what a replay of
//! the whole log reads: the same claims, in the same order, each with the
//! same admitting transition, scoped or not.
//!
//! Attacker capability modelled: none. A snapshot is a cache the owner
//! builds from the log; nothing that verifies the record reads it, so a
//! snapshot changed by hand is out of scope here. These tests hold the
//! cache to the replay over each case a replay must get right, for every
//! place a snapshot can be kept and every target after it.

#![expect(
    clippy::unwrap_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{commit_entry, expect_committed, pg_program, propose_pg_as, reset_db, test_pool};
use morpholog_core::{EvalValue, Program};
use morpholog_postgres::testing::read_claims_at_replayed;
use morpholog_postgres::{
    PgAtomicOutcome, PgError, PgPool, PgProgram, Proposal, propose_all_against_pg, read_claims_at,
    read_claims_at_for_predicates, refresh_state_snapshots, testing,
};
use morpholog_test_support::{dec, subj};
use testing::build_state_snapshot;
use uuid::Uuid;

const PROGRAMME: &str = "program as_of_snapshots
predicate X(k: Subject)
predicate Note(k: Subject, n: Decimal)
invariant a_note_needs_its_x:
    Note(k, n) implies X(k)
transformation admit_x(k):
    admit X(k)
transformation retract_x(k):
    require X(k)
    retract X(k)
transformation readmit_x(k):
    require X(k)
    retract X(k)
    admit X(k)
transformation note(k, n):
    require X(k)
    admit Note(k, n)
transformation drop_note(k, n):
    retract Note(k, n)
";

fn program() -> Program {
    morpholog_surface::parse_program(PROGRAMME).unwrap()
}

async fn act(pool: &PgPool, pg: &PgProgram, name: &str, args: Vec<EvalValue>) -> Uuid {
    let t = program().transformation(name).unwrap().clone();
    expect_committed(propose_pg_as(pool, pg, &t, args, "w").await.unwrap())
}

fn proposal(name: &str, args: Vec<EvalValue>) -> Proposal {
    common::attested(&morpholog_core::Transition {
        transformation_name: name.into(),
        args,
        actor: "w".into(),
        decided_at: None,
    })
}

/// A retraction that started first commits after a concurrent admit of
/// the same claim, which found it present and changed nothing: the
/// operations, not the proposed lists, say the claim is gone.
async fn retract_around_a_no_op_admit(pool: &PgPool, pg: &Arc<PgProgram>, k: &str) {
    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT 1 FROM morpholog.claims WHERE predicate_name = 'X'
           AND arguments = jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $1::text))
         FOR UPDATE",
    )
    .bind(k)
    .execute(&mut *holder)
    .await
    .unwrap();
    let retract = {
        let (pool, pg, k) = (pool.clone(), pg.clone(), k.to_owned());
        tokio::spawn(async move { act(&pool, &pg, "retract_x", vec![subj(&k)]).await })
    };
    for waited in 0.. {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND query LIKE 'DELETE FROM morpholog.claims%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked > 0 {
            break;
        }
        assert!(waited < 400, "the retraction never blocked");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    act(pool, pg, "admit_x", vec![subj(k)]).await;
    holder.commit().await.unwrap();
    retract.await.unwrap();
}

/// A retract and re-admit in one act, an admit of a claim already
/// present, a retraction, an atomic batch, and the concurrent pair.
async fn build_history(pool: &PgPool, pg: &Arc<PgProgram>) {
    act(pool, pg, "admit_x", vec![subj("a")]).await;
    act(pool, pg, "admit_x", vec![subj("b")]).await;
    act(pool, pg, "note", vec![subj("a"), dec(3)]).await;
    act(pool, pg, "note", vec![subj("a"), dec(7)]).await;
    act(pool, pg, "readmit_x", vec![subj("a")]).await;
    act(pool, pg, "admit_x", vec![subj("a")]).await;
    act(pool, pg, "drop_note", vec![subj("a"), dec(3)]).await;
    let batch = [
        proposal("admit_x", vec![subj("c")]),
        proposal("readmit_x", vec![subj("b")]),
        proposal("note", vec![subj("c"), dec(9)]),
    ];
    match propose_all_against_pg(pool, pg, &batch).await.unwrap() {
        PgAtomicOutcome::Committed { .. } => {}
        other @ PgAtomicOutcome::Rejected { .. } => panic!("the batch must commit: {other:?}"),
    }
    retract_around_a_no_op_admit(pool, pg, "b").await;
    act(pool, pg, "admit_x", vec![subj("d")]).await;
    act(pool, pg, "retract_x", vec![subj("d")]).await;
}

/// Every transition in log order that an as-of read may target.
async fn targets(pool: &PgPool) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit
         WHERE transformation_name <> 'morpholog.baseline'
         ORDER BY audit_order, act_ordinal, committed_at, transition_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn forget_snapshots(pool: &PgPool) {
    sqlx::query("DELETE FROM morpholog.state_snapshots")
        .execute(pool)
        .await
        .unwrap();
}

/// Each target read from each snapshot at or before it, unscoped and
/// scoped to each predicate, against the replay from the start.
async fn every_snapshot_agrees(pool: &PgPool, predicates: &[&str]) {
    let targets = targets(pool).await;
    for (i, &kept) in targets.iter().enumerate() {
        forget_snapshots(pool).await;
        assert!(build_state_snapshot(pool, kept).await.unwrap());
        for &target in &targets[i..] {
            assert_eq!(
                read_claims_at(pool, target).await.unwrap(),
                read_claims_at_replayed(pool, target, None).await.unwrap(),
                "kept at {i}, read at {target}"
            );
            for predicate in predicates {
                let scope = [(*predicate).to_string()];
                assert_eq!(
                    read_claims_at_for_predicates(pool, target, &scope)
                        .await
                        .unwrap(),
                    read_claims_at_replayed(pool, target, Some(&scope))
                        .await
                        .unwrap(),
                    "kept at {i}, read at {target}, scoped to {predicate}"
                );
            }
        }
    }
    // Every snapshot at once: each read starts from its nearest.
    forget_snapshots(pool).await;
    for &kept in &targets {
        build_state_snapshot(pool, kept).await.unwrap();
    }
    for &target in &targets {
        assert_eq!(
            read_claims_at(pool, target).await.unwrap(),
            read_claims_at_replayed(pool, target, None).await.unwrap(),
            "every snapshot kept, read at {target}"
        );
    }
}

#[tokio::test]
async fn a_read_from_any_snapshot_reads_what_the_whole_replay_reads() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = Arc::new(pg_program(program()));
    build_history(&pool, &pg).await;
    every_snapshot_agrees(&pool, &["X", "Note"]).await;
}

/// A legacy row, from before the canonical order, admitting `X(k)`, and
/// the claim it left.
async fn legacy_claim(pool: &PgPool, k: &str, second: u32) {
    sqlx::query(
        "WITH row AS (
            INSERT INTO morpholog.audit (
                transition_id, transformation_name, arguments, actor, invariant_epoch,
                invariants_checked, asserted_claims, retracted_claims, emitted_intents,
                committed_at, attestation, parameters, model_hash, semantics_version,
                drawn_subjects)
            VALUES ($1, 'admit_x', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)),
                '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]',
                jsonb_build_array(jsonb_build_object('predicate', 'X', 'args',
                    jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)))),
                '[]', '[]', timestamptz '2026-06-01T00:00:00Z' + $3 * interval '1 second',
                '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[\"k\"]',
                'sha256:' || repeat('d', 64), 1, '[]')
            RETURNING transition_id)
         INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'X', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)), transition_id
         FROM row",
    )
    .bind(Uuid::now_v7())
    .bind(k)
    .bind(f64::from(second))
    .execute(pool)
    .await
    .unwrap();
}

/// A database upgraded from before the canonical order: legacy rows, the
/// activation in chunks, then ordered rows. A snapshot kept before the
/// activation is replaced by its baseline; a chunk cannot be kept.
#[tokio::test]
async fn a_snapshot_across_the_activation_reads_what_the_replay_reads() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    for (i, k) in ["a", "b", "c", "d", "e"].iter().enumerate() {
        legacy_claim(&pool, k, u32::try_from(i).unwrap()).await;
    }
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(
        "ALTER TABLE morpholog.audit ENABLE TRIGGER audit_order;
         LOCK TABLE morpholog.claims, morpholog.audit IN EXCLUSIVE MODE;",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    morpholog_postgres::write_activation_record_with(&mut tx, 2, morpholog_postgres::CHUNK_BYTES)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    commit_entry(&pool, "after_one").await;
    commit_entry(&pool, "after_two").await;

    let chunk: Uuid = sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit
         WHERE transformation_name = 'morpholog.baseline' ORDER BY act_ordinal LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(matches!(
        build_state_snapshot(&pool, chunk).await,
        Err(PgError::BaselineChunk { .. })
    ));
    every_snapshot_agrees(&pool, &["X", "JournalEntry"]).await;
}

/// A snapshot is kept only where the log before it is final, and a read
/// cannot use one that holds fewer claims than it says or was written
/// in another format: it replays from the start and answers the same.
#[tokio::test]
async fn only_a_settled_complete_snapshot_is_used() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    reset_db(&pool).await;
    let pg = Arc::new(pg_program(program()));
    let first = act(&pool, &pg, "admit_x", vec![subj("a")]).await;
    act(&pool, &pg, "admit_x", vec![subj("b")]).await;

    // An open transaction holding an order before the target: the
    // trigger draws it when the row is inserted.
    let mut open = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor, invariant_epoch,
            invariants_checked, asserted_claims, retracted_claims, emitted_intents,
            attestation, parameters, model_hash, semantics_version, drawn_subjects, operations)
         VALUES ($1, 'admit_x', '[]', '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]',
            '[]', '[]', '[]', '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[]',
            'sha256:' || repeat('d', 64), 1, '[]', '[]')",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *open)
    .await
    .unwrap();
    let late = act(&pool, &pg, "admit_x", vec![subj("d")]).await;
    assert!(matches!(
        build_state_snapshot(&pool, late).await,
        Err(PgError::TransitionNotSettled(_))
    ));
    open.rollback().await.unwrap();

    assert!(build_state_snapshot(&pool, first).await.unwrap());
    assert!(
        !build_state_snapshot(&pool, first).await.unwrap(),
        "a place already kept is not written twice"
    );
    let oracle = read_claims_at_replayed(&pool, late, None).await.unwrap();

    sqlx::query("DELETE FROM morpholog.state_snapshot_claims")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(read_claims_at(&pool, late).await.unwrap(), oracle);

    forget_snapshots(&pool).await;
    build_state_snapshot(&pool, first).await.unwrap();
    sqlx::query("UPDATE morpholog.state_snapshots SET format = 2")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(read_claims_at(&pool, late).await.unwrap(), oracle);
}

async fn kept(pool: &PgPool) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.state_snapshots
         ORDER BY audit_order, act_ordinal, committed_at, transition_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A refresh keeps one snapshot after every `every` rows and no others:
/// it builds what is missing, keeps what is there, deletes what is off
/// the grid, and stops at the first place that is not settled. What it
/// keeps reads what the replay reads.
#[tokio::test]
async fn a_refresh_keeps_the_grid_and_only_the_grid() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    reset_db(&pool).await;
    let pg = Arc::new(pg_program(program()));
    let mut rows = Vec::new();
    for k in ["a", "b", "c", "d", "e", "f", "g"] {
        rows.push(act(&pool, &pg, "admit_x", vec![subj(k)]).await);
    }
    let every = |n: u64| std::num::NonZeroU64::new(n).unwrap();

    let first = refresh_state_snapshots(&pool, every(2)).await.unwrap();
    assert_eq!((first.built, first.kept, first.removed), (3, 0, 0));
    assert_eq!(first.latest, Some(rows[5]));
    assert_eq!(kept(&pool).await, vec![rows[1], rows[3], rows[5]]);
    let again = refresh_state_snapshots(&pool, every(2)).await.unwrap();
    assert_eq!((again.built, again.kept, again.removed), (0, 3, 0));

    // Off the grid: removed. Another interval: the grid moves.
    build_state_snapshot(&pool, rows[2]).await.unwrap();
    let pruned = refresh_state_snapshots(&pool, every(2)).await.unwrap();
    assert_eq!((pruned.built, pruned.kept, pruned.removed), (0, 3, 1));
    let regrid = refresh_state_snapshots(&pool, every(3)).await.unwrap();
    assert_eq!((regrid.built, regrid.kept, regrid.removed), (1, 1, 2));
    assert_eq!(kept(&pool).await, vec![rows[2], rows[5]]);
    for &target in &rows {
        assert_eq!(
            read_claims_at(&pool, target).await.unwrap(),
            read_claims_at_replayed(&pool, target, None).await.unwrap(),
        );
    }

    // An open transaction holds an order; the rows after it are not
    // settled, so the refresh keeps what comes before and stops there.
    let mut open = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor, invariant_epoch,
            invariants_checked, asserted_claims, retracted_claims, emitted_intents,
            attestation, parameters, model_hash, semantics_version, drawn_subjects, operations)
         VALUES ($1, 'admit_x', '[]', '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]',
            '[]', '[]', '[]', '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[]',
            'sha256:' || repeat('d', 64), 1, '[]', '[]')",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *open)
    .await
    .unwrap();
    let late = act(&pool, &pg, "admit_x", vec![subj("h")]).await;
    let stopped = refresh_state_snapshots(&pool, every(1)).await.unwrap();
    assert_eq!(stopped.latest, Some(rows[6]), "{stopped:?}");
    assert!(!kept(&pool).await.contains(&late));
    open.rollback().await.unwrap();
    let settled = refresh_state_snapshots(&pool, every(1)).await.unwrap();
    assert_eq!(settled.latest, Some(late), "{settled:?}");
}

/// A refresh that moves the grid deletes the snapshot a scoped read has
/// just chosen, before the read loads its claims. The read still loads
/// that snapshot whole, as it was when chosen, never an emptied one.
#[tokio::test]
async fn a_scoped_read_is_not_emptied_by_a_refresh_beside_it() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    reset_db(&pool).await;
    let pg = Arc::new(pg_program(program()));
    let mut rows = Vec::new();
    for k in ["a", "b", "c", "d", "e", "f"] {
        rows.push(act(&pool, &pg, "admit_x", vec![subj(k)]).await);
    }
    let every = |n: u64| std::num::NonZeroU64::new(n).unwrap();
    refresh_state_snapshots(&pool, every(2)).await.unwrap();
    assert_eq!(kept(&pool).await, vec![rows[1], rows[3], rows[5]]);

    let found = testing::nearest_snapshot_pausing(&pool, rows[2], &["X"], async || {
        refresh_state_snapshots(&pool, every(3)).await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(
        kept(&pool).await,
        vec![rows[2], rows[5]],
        "the refresh did not delete the chosen snapshot"
    );
    let scope = ["X".to_string()];
    assert_eq!(
        found,
        Some((
            rows[1],
            read_claims_at_replayed(&pool, rows[1], Some(&scope))
                .await
                .unwrap()
        )),
    );
}
