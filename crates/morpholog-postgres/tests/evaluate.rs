//! Scoring a candidate programme against committed history. Commits real
//! ledger transitions, then replays them under candidate invariants that
//! were never deployed - the evaluator pointed backward.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::Program;
use morpholog_core::ir_builder::{claim, exists, invariant, not, pre, var, wildcard};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{PgError, PgPool, score_candidate};

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

fn candidate(inv: morpholog_core::Invariant) -> Program {
    Program {
        name: "candidate".into(),
        predicates: vec![],
        intents: vec![],
        definitions: vec![],
        invariants: vec![inv],
        transformations: vec![],
        derived_claims: vec![],
    }
}

#[tokio::test]
async fn a_candidate_history_violates_reports_the_introducing_commit() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    commit_entry(&pool, "e1").await;

    // "No journal entries may exist" - a prohibition the ledger violates.
    let inv = invariant(
        "NoEntries",
        not(exists(
            "e",
            claim("JournalEntry", vec![var("e"), wildcard(), wildcard()]),
        )),
    );
    let report = score_candidate(&pool, &candidate(inv)).await.unwrap();

    assert_eq!(report.transitions_replayed, 2);
    assert_eq!(report.semantics, "fresh_state_violation_v1");
    assert!(report.program_hash.starts_with("sha256:"));

    let scored = &report.invariants[0];
    assert_eq!(scored.invariant, "NoEntries");
    // The FIRST entry introduces the violation; the second inherits it
    // (entries persist), so fresh-violation counts only the introducing
    // commit - this is the semantics, not an under-count.
    assert_eq!(scored.would_refuse, 1);
    assert_eq!(scored.refused_transitions.len(), 1);
}

#[tokio::test]
async fn a_candidate_that_always_holds_refuses_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;

    // References a predicate the ledger never asserts: never violated.
    let inv = invariant(
        "NoUnicorns",
        not(exists("u", claim("Unicorn", vec![var("u")]))),
    );
    let report = score_candidate(&pool, &candidate(inv)).await.unwrap();
    assert_eq!(report.invariants[0].would_refuse, 0);
    assert!(report.invariants[0].refused_transitions.is_empty());
}

#[tokio::test]
async fn a_pre_candidate_is_rejected_not_silently_mis_scored() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;

    let inv = invariant(
        "UsesPre",
        pre(exists(
            "e",
            claim("JournalEntry", vec![var("e"), wildcard(), wildcard()]),
        )),
    );
    let err = score_candidate(&pool, &candidate(inv)).await.unwrap_err();
    assert!(
        matches!(err, PgError::InvalidState(msg) if msg.contains("pre(...)")),
        "a transition-relational candidate must be refused, not mis-scored"
    );
}
