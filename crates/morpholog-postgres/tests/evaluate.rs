//! Scoring a candidate programme against committed history. Commits real
//! ledger transitions, then replays them under candidate invariants that
//! were never deployed - the evaluator pointed backward.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::CaseOutcome;
use morpholog_core::Program;
use morpholog_core::ir_builder::{claim, exists, invariant, not, pre, var, wildcard};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    Checkpoint, CheckpointOutcome, EvidencePack, PgError, PgPool, create_checkpoint, export_pack,
    score_candidate, score_candidate_against_pack, score_candidate_against_packs,
};

mod common;
use common::{dec, reset_db, subj, test_pool};

async fn commit_entry(pool: &PgPool, id: &str) -> uuid::Uuid {
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

fn no_entries() -> Program {
    candidate(invariant(
        "NoEntries",
        not(exists(
            "e",
            claim("JournalEntry", vec![var("e"), wildcard(), wildcard()]),
        )),
    ))
}

async fn export_history_pack(pool: &PgPool) -> EvidencePack {
    match create_checkpoint(pool, None, None).await.unwrap() {
        CheckpointOutcome::Created(_) | CheckpointOutcome::NoNewRows(_) => {}
    }
    export_pack(pool, None).await.unwrap()
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
    let report = score_candidate(&pool, &candidate(inv), None).await.unwrap();

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
    let report = score_candidate(&pool, &candidate(inv), None).await.unwrap();
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
    let err = score_candidate(&pool, &candidate(inv), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PgError::InvalidState(msg) if msg.contains("pre(...)")),
        "a transition-relational candidate must be refused, not mis-scored"
    );
}

#[tokio::test]
async fn pack_backed_score_reproduces_the_database_score() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    commit_entry(&pool, "e1").await;
    let pack = export_history_pack(&pool).await;

    let candidate = no_entries();
    let online = score_candidate(&pool, &candidate, None).await.unwrap();
    let offline = score_candidate_against_pack(&candidate, &pack, None, None).unwrap();

    // The offline score of a genuine pack reproduces the live score
    // exactly - same report, byte for byte.
    assert_eq!(
        serde_json::to_value(&online).unwrap(),
        serde_json::to_value(&offline).unwrap(),
    );
    assert_eq!(offline.invariants[0].would_refuse, 1);
}

#[tokio::test]
async fn refuses_to_score_a_tampered_pack() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    let pack = export_history_pack(&pool).await;

    // Edit a row's content the way an attacker holding the file would.
    let mut v = serde_json::to_value(&pack).unwrap();
    v["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    let tampered: EvidencePack = serde_json::from_value(v).unwrap();

    let err = score_candidate_against_pack(&no_entries(), &tampered, None, None).unwrap_err();
    assert!(
        matches!(err, PgError::InvalidState(msg) if msg.contains("does not verify")),
        "a pack that does not verify must not be scored"
    );
}

#[tokio::test]
async fn refuses_a_pre_candidate_against_a_pack() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    let pack = export_history_pack(&pool).await;

    let pre_candidate = candidate(invariant(
        "UsesPre",
        pre(exists(
            "e",
            claim("JournalEntry", vec![var("e"), wildcard(), wildcard()]),
        )),
    ));
    let err = score_candidate_against_pack(&pre_candidate, &pack, None, None).unwrap_err();
    assert!(matches!(err, PgError::InvalidState(msg) if msg.contains("pre(...)")));
}

#[tokio::test]
async fn pack_row_order_is_not_load_bearing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    commit_entry(&pool, "e1").await;
    let pack = export_history_pack(&pool).await;
    let candidate = no_entries();

    let ordered = score_candidate_against_pack(&candidate, &pack, None, None).unwrap();
    // The verifier re-sorts; so must scoring. A shuffled pack scores
    // identically.
    let mut shuffled = pack.clone();
    shuffled.rows.reverse();
    let reshuffled = score_candidate_against_pack(&candidate, &shuffled, None, None).unwrap();

    assert_eq!(
        serde_json::to_value(&ordered).unwrap(),
        serde_json::to_value(&reshuffled).unwrap(),
    );
}

#[tokio::test]
async fn refuses_to_score_against_a_mismatched_anchor() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    let pack = export_history_pack(&pool).await;

    // An anchor at a size the pack covers but with a different identity:
    // the coordinated-rewrite signal verify_pack catches.
    let forged = Checkpoint {
        tree_size: pack.manifest.tree_size,
        root_hash: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
        prev_checkpoint_hash: None,
        checkpoint_hash: "forged".to_string(),
        signatures: Vec::new(),
    };
    let err = score_candidate_against_pack(&no_entries(), &pack, Some(&forged), None).unwrap_err();
    assert!(matches!(err, PgError::InvalidState(msg) if msg.contains("does not verify")));
}

/// Build one independent single-case pack: a fresh ledger holding exactly
/// one firm-year, checkpointed and exported. The file-name label mirrors
/// what the CLI would use.
async fn build_case_pack(pool: &PgPool, id: &str) -> (String, EvidencePack) {
    reset_db(pool).await;
    commit_entry(pool, id).await;
    (format!("{id}.json"), export_history_pack(pool).await)
}

#[tokio::test]
async fn batch_over_packs_equals_individual_scores() {
    let pool = test_pool().await;
    let mut cases = Vec::new();
    for id in ["c0", "c1", "c2"] {
        cases.push(build_case_pack(&pool, id).await);
    }
    let candidate = no_entries();

    let batch = score_candidate_against_packs(&candidate, &cases).unwrap();
    assert_eq!(batch.cases.len(), 3);
    assert_eq!(batch.semantics, "fresh_state_violation_v1");

    // Each batch case equals the individual single-pack score on the
    // substantive fields (the batch hoists candidate identity).
    for (i, (name, pack)) in cases.iter().enumerate() {
        let single = score_candidate_against_pack(&candidate, pack, None, None).unwrap();
        let case = &batch.cases[i];
        assert_eq!(case.pack, *name);
        match &case.outcome {
            CaseOutcome::Scored {
                transitions_replayed,
                invariants,
            } => {
                assert_eq!(*transitions_replayed, single.transitions_replayed);
                let (batched, solo) = (&invariants[0], &single.invariants[0]);
                assert_eq!(batched.invariant, solo.invariant);
                assert_eq!(batched.version, solo.version);
                assert_eq!(batched.initially_holds, solo.initially_holds);
                assert_eq!(batched.would_refuse, solo.would_refuse);
                assert_eq!(batched.refused_transitions, solo.refused_transitions);
            }
            CaseOutcome::Failed { error } => {
                panic!("expected a scored case, got failed: {error}")
            }
        }
    }
}

#[tokio::test]
async fn a_tampered_pack_fails_only_its_own_case() {
    let pool = test_pool().await;
    let (n0, p0) = build_case_pack(&pool, "c0").await;
    let (n1, p1) = build_case_pack(&pool, "c1").await;

    // Tamper the second pack the way an attacker holding the file would.
    let mut v = serde_json::to_value(&p1).unwrap();
    v["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    let p1_bad: EvidencePack = serde_json::from_value(v).unwrap();

    let batch = score_candidate_against_packs(&no_entries(), &[(n0, p0), (n1, p1_bad)]).unwrap();
    assert!(matches!(batch.cases[0].outcome, CaseOutcome::Scored { .. }));
    match &batch.cases[1].outcome {
        CaseOutcome::Failed { error } => assert!(error.contains("does not verify")),
        CaseOutcome::Scored { .. } => panic!("expected a failed case, got a scored one"),
    }
}

#[tokio::test]
async fn a_pre_candidate_fails_the_whole_batch_once() {
    let pool = test_pool().await;
    let case = build_case_pack(&pool, "c0").await;
    let pre_candidate = candidate(invariant(
        "UsesPre",
        pre(exists(
            "e",
            claim("JournalEntry", vec![var("e"), wildcard(), wildcard()]),
        )),
    ));
    let err = score_candidate_against_packs(&pre_candidate, &[case]).unwrap_err();
    assert!(matches!(err, PgError::InvalidState(msg) if msg.contains("pre(...)")));
}

// ============================================================
// Train/test splits
// ============================================================

/// A candidate violated only by the second entry: its ground subject
/// pins the rule to `e1`, so `e0` cannot introduce it.
fn no_e1() -> Program {
    use morpholog_core::ir_builder::subj as ir_subj;
    candidate(invariant(
        "NoE1",
        not(exists(
            "d",
            claim("JournalEntry", vec![ir_subj("e1"), var("d"), wildcard()]),
        )),
    ))
}

#[tokio::test]
async fn a_split_attributes_violations_to_their_introducing_slice() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let e0 = commit_entry(&pool, "e0").await;
    commit_entry(&pool, "e1").await;

    // NoEntries is introduced by e0 - the boundary transition itself -
    // so it counts in train and its inherited presence never recounts
    // in test. NoE1 is introduced by e1, after the boundary.
    let boundary = Some(morpholog_postgres::SplitBoundary::Transition(e0));
    let train_hit = score_candidate(&pool, &no_entries(), boundary)
        .await
        .unwrap();
    let split = train_hit.split.expect("split was requested");
    assert_eq!(split.boundary.resolved_transition_id, e0.to_string());
    assert_eq!(split.train.transitions_replayed, 1);
    assert_eq!(split.test.transitions_replayed, 1);
    assert_eq!(split.train.invariants[0].would_refuse, 1);
    assert_eq!(split.test.invariants[0].would_refuse, 0);

    let test_hit = score_candidate(&pool, &no_e1(), boundary).await.unwrap();
    let split = test_hit.split.expect("split was requested");
    assert_eq!(split.train.invariants[0].would_refuse, 0);
    assert_eq!(split.test.invariants[0].would_refuse, 1);
    // The whole-history totals cover both slices either way.
    assert_eq!(test_hit.invariants[0].would_refuse, 1);
}

#[tokio::test]
async fn a_timestamp_boundary_matches_the_transition_form() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let e0 = commit_entry(&pool, "e0").await;
    commit_entry(&pool, "e1").await;

    let mut conn = pool.acquire().await.unwrap();
    let (at, _) = morpholog_postgres::audit_cursor_for(&mut conn, e0)
        .await
        .unwrap();
    drop(conn);

    let by_id = score_candidate(
        &pool,
        &no_entries(),
        Some(morpholog_postgres::SplitBoundary::Transition(e0)),
    )
    .await
    .unwrap();
    let by_time = score_candidate(
        &pool,
        &no_entries(),
        Some(morpholog_postgres::SplitBoundary::AtOrBefore(at)),
    )
    .await
    .unwrap();

    // Same slice counts either way; the boundary report differs only
    // in what was requested.
    let (a, b) = (by_id.split.unwrap(), by_time.split.unwrap());
    assert_eq!(
        a.boundary.resolved_transition_id,
        b.boundary.resolved_transition_id
    );
    assert_eq!(a.train.transitions_replayed, b.train.transitions_replayed);
    assert_eq!(a.test.transitions_replayed, b.test.transitions_replayed);
}

#[tokio::test]
async fn a_pack_split_reproduces_the_database_split() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let e0 = commit_entry(&pool, "e0").await;
    commit_entry(&pool, "e1").await;
    let pack = export_history_pack(&pool).await;

    let boundary = Some(morpholog_postgres::SplitBoundary::Transition(e0));
    let online = score_candidate(&pool, &no_entries(), boundary)
        .await
        .unwrap();
    let offline = score_candidate_against_pack(&no_entries(), &pack, None, boundary).unwrap();

    assert_eq!(
        serde_json::to_value(&online).unwrap(),
        serde_json::to_value(&offline).unwrap(),
    );
}

#[tokio::test]
async fn an_unknown_boundary_transition_is_an_error_in_both_modes() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    let pack = export_history_pack(&pool).await;

    let ghost = uuid::Uuid::from_u128(42);
    let boundary = Some(morpholog_postgres::SplitBoundary::Transition(ghost));
    let db_err = score_candidate(&pool, &no_entries(), boundary)
        .await
        .unwrap_err();
    assert!(matches!(db_err, PgError::TransitionNotFound(id) if id == ghost));
    let pack_err = score_candidate_against_pack(&no_entries(), &pack, None, boundary).unwrap_err();
    assert!(matches!(pack_err, PgError::TransitionNotFound(id) if id == ghost));
}

#[tokio::test]
async fn a_boundary_before_all_history_is_an_error_not_an_empty_slice() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;

    let dawn = chrono::DateTime::from_timestamp(0, 0).unwrap();
    let err = score_candidate(
        &pool,
        &no_entries(),
        Some(morpholog_postgres::SplitBoundary::AtOrBefore(dawn)),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, PgError::NoTransitionAtOrBefore(_)));
}

#[tokio::test]
async fn a_boundary_at_the_end_of_history_leaves_an_empty_test_slice() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "e0").await;
    let e1 = commit_entry(&pool, "e1").await;

    let report = score_candidate(
        &pool,
        &no_entries(),
        Some(morpholog_postgres::SplitBoundary::Transition(e1)),
    )
    .await
    .unwrap();
    let split = report.split.expect("split was requested");
    assert_eq!(split.train.transitions_replayed, 2);
    assert_eq!(split.test.transitions_replayed, 0);
    assert!(split.test.invariants[0].refused_transitions.is_empty());
}
