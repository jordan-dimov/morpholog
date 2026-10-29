//! What every replay of the audit log reports over one fixed history,
//! pinned as a golden file. The history holds each case a replay has to
//! get right: a retract and re-admit in one act, an admit of a claim
//! already present, an atomic batch, a predicate read on its own, and a
//! lawful concurrent pair: a retraction that commits after a concurrent
//! admit found the claim present, which replay must follow as the table
//! did.
//! A change to how history is replayed shows up here as a diff, so a
//! refactor that must not change any result is held to that.
//!
//! `UPDATE_GOLDENS=1` rewrites the file after a deliberate change.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{
    attested, expect_committed, make_checkpoint, pg_program, propose_pg_as, reset_db, test_pool,
};
use morpholog_core::{EvalValue, Program, Transition};
use morpholog_postgres::{
    PgAtomicOutcome, PgPool, PgProgram, Proposal, SplitBoundary, coverage_replay, export_pack,
    list_audit_rows, list_claims_at_for_predicates, propose_all_against_pg, read_claims_at,
    rehearse_candidate, rehearse_candidate_against_pack, score_candidate,
    score_candidate_against_pack, verify_replay,
};
use morpholog_test_support::{dec, subj, validated};
use serde_json::{Value, json};
use uuid::Uuid;

const PROGRAMME: &str = "program replay_characterisation
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

/// The deployed programme plus a rule history broke, for scoring and
/// rehearsal.
fn candidate() -> Program {
    let source = format!("{PROGRAMME}invariant small_notes:\n    Note(k, n) implies n <= 5\n");
    morpholog_surface::parse_program(&source).unwrap()
}

fn program() -> Program {
    morpholog_surface::parse_program(PROGRAMME).unwrap()
}

async fn act(pool: &PgPool, pg: &PgProgram, name: &str, args: Vec<EvalValue>) -> Uuid {
    let t = program().transformation(name).unwrap().clone();
    expect_committed(propose_pg_as(pool, pg, &t, args, "w").await.unwrap())
}

fn proposal(name: &str, args: Vec<EvalValue>) -> Proposal {
    attested(&Transition {
        transformation_name: name.into(),
        args,
        actor: "w".into(),
    })
}

/// A retraction that started first commits after a concurrent admit of
/// the same claim, which found it present and changed nothing. Replayed
/// from the proposed lists, the retraction came first and the claim
/// survived; replayed from the operations, it is gone, as in the table.
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
}

/// Transition ids become their place in the log and times become a
/// placeholder, so the file holds only what a replay decided.
fn normalise(value: Value, labels: &HashMap<String, String>) -> Value {
    match value {
        Value::String(s) => match labels.get(&s) {
            Some(label) => Value::String(label.clone()),
            None if s.parse::<jiff::Timestamp>().is_ok() => Value::String("<time>".into()),
            None => Value::String(s),
        },
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| normalise(v, labels)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, normalise(v, labels)))
                .collect(),
        ),
        other => other,
    }
}

#[tokio::test]
async fn every_replay_reports_what_it_reported_before() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = Arc::new(pg_program(program()));
    build_history(&pool, &pg).await;

    let rows = list_audit_rows(&pool).await.unwrap();
    let labels: HashMap<String, String> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r.transition_id.to_string(), format!("t{i}")))
        .collect();
    let last = rows.last().unwrap().transition_id;

    let mut claims_at = Vec::new();
    for row in &rows {
        let claims = read_claims_at(&pool, row.transition_id).await.unwrap();
        claims_at.push(json!({ "at": row.transition_id.to_string(), "claims": claims }));
    }
    let notes_only = list_claims_at_for_predicates(&pool, last, &["Note".to_string()])
        .await
        .unwrap();
    let verified = verify_replay(&pool, &mut |_| {}).await.unwrap();
    let deployed = program();
    let coverage = coverage_replay(&pool, &validated(&deployed)).await.unwrap();
    let strict = candidate();
    let score = score_candidate(&pool, &validated(&strict), None)
        .await
        .unwrap();
    let split = score_candidate(
        &pool,
        &validated(&strict),
        Some(SplitBoundary::Transition(rows[4].transition_id)),
    )
    .await
    .unwrap();
    let strict_pg = pg_program(candidate());
    let rehearsal = rehearse_candidate(&pool, strict_pg.prepared())
        .await
        .unwrap();
    make_checkpoint(&pool).await;
    let pack = export_pack(&pool, None).await.unwrap();
    let pack_score = score_candidate_against_pack(&validated(&strict), &pack, None, None).unwrap();
    let pack_rehearsal =
        rehearse_candidate_against_pack(strict_pg.prepared(), &pack, None).unwrap();

    let report = normalise(
        json!({
            "transformations": rows.iter().map(|r| r.transformation_name.to_string()).collect::<Vec<_>>(),
            "claims_at": claims_at,
            "notes_only_at_last": notes_only,
            "verify": verified,
            "coverage": coverage,
            "score": score,
            "score_split_after_t4": split,
            "rehearsal": rehearsal,
            "pack_score": pack_score,
            "pack_rehearsal": pack_rehearsal,
        }),
        &labels,
    );
    let text = serde_json::to_string_pretty(&report).unwrap() + "\n";
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/replay_characterisation.json"
    );
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &text).unwrap();
    }
    let golden =
        std::fs::read_to_string(path).expect("the golden exists; UPDATE_GOLDENS=1 writes it");
    assert_eq!(text, golden, "a replay result changed; diff against {path}");
}
