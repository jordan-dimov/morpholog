//! What `audit verify` says of a history in the one canonical order: the
//! legacy prefix against the activation baseline, and each later row's
//! operations against the state they find. Attacker capability: a role
//! that writes the audit and claims tables directly, with no checkpoint
//! over the rows it touches, so the replay, not the tree, has to notice.
//! The histories follow the real chronology: legacy rows, the activation
//! record, then ordered rows.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{expect_committed, pg_program, propose_pg_as, reset_db, test_pool};
use morpholog_core::{ClaimInstance, Program};
use morpholog_postgres::{PgPool, PgProgram, VerifyOutcome, testing, verify_replay};
use morpholog_test_support::subj;
use uuid::Uuid;

const PROGRAMME: &str = "program canonical_order
predicate X(k: Subject)
transformation admit_x(k):
    admit X(k)
transformation retract_x(k):
    require X(k)
    retract X(k)
";

fn program() -> Program {
    morpholog_surface::parse_program(PROGRAMME).unwrap()
}

async fn act(pool: &PgPool, pg: &PgProgram, name: &str, k: &str) -> Uuid {
    let t = program().transformation(name).unwrap().clone();
    expect_committed(
        propose_pg_as(pool, pg, &t, vec![subj(k)], "w")
            .await
            .unwrap(),
    )
}

fn x(k: &str) -> ClaimInstance {
    ClaimInstance {
        predicate: "X".into(),
        args: vec![subj(k)],
    }
}

/// A row an older binary wrote: no order, read from what it proposed.
async fn legacy(pool: &PgPool, name: &str, k: &str, committed_at: &str) -> Uuid {
    let id = Uuid::now_v7();
    let claim = serde_json::json!([{"predicate": "X", "args": [{"type": "subject", "value": k}]}]);
    let (asserted, retracted) = if name == "admit_x" {
        (claim, serde_json::json!([]))
    } else {
        (serde_json::json!([]), claim)
    };
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor, invariant_epoch,
            invariants_checked, asserted_claims, retracted_claims, emitted_intents,
            committed_at, attestation, parameters, model_hash, semantics_version,
            drawn_subjects)
         VALUES ($1, $2, jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $3::text)),
            '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]', $4, $5, '[]',
            $6::timestamptz, '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[\"k\"]',
            'sha256:' || repeat('d', 64), 1, '[]')",
    )
    .bind(id)
    .bind(name)
    .bind(k)
    .bind(asserted)
    .bind(retracted)
    .bind(committed_at)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn execute(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .expect(sql);
}

async fn verdict(pool: &PgPool) -> VerifyOutcome {
    verify_replay(pool, &mut |_| {}).await.unwrap()
}

/// The conflict details of a divergent verdict with the two records
/// otherwise agreeing.
fn conflicts(outcome: VerifyOutcome) -> Vec<(Uuid, String)> {
    match outcome {
        VerifyOutcome::Divergent {
            only_in_claims_table,
            only_in_replay,
            conflicts,
        } if only_in_claims_table.is_empty() && only_in_replay.is_empty() => conflicts
            .into_iter()
            .map(|c| (c.transition_id, c.detail))
            .collect(),
        other => panic!("expected conflicts alone: {other:?}"),
    }
}

/// A retraction that started first committed after an admit of the same
/// claim that found it present and changed nothing. Legacy rows sort by
/// start time, so their replay keeps the claim the table lost. Nothing
/// later can tell what happened, so the verdict stays indeterminate
/// whatever lawful history follows.
#[tokio::test]
async fn legacy_uncertainty_survives_later_transitions() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    legacy(&pool, "admit_x", "a", "2026-06-01T00:00:01Z").await;
    legacy(&pool, "retract_x", "a", "2026-06-01T00:00:02Z").await;
    legacy(&pool, "admit_x", "a", "2026-06-01T00:00:03Z").await;
    testing::activate(&pool).await.unwrap();

    let indeterminate = |outcome: VerifyOutcome, rows: i64| match outcome {
        VerifyOutcome::Indeterminate {
            transitions,
            claims,
            legacy_only_in_baseline,
            legacy_only_in_replay,
        } => {
            assert_eq!(transitions, rows);
            assert_eq!(legacy_only_in_replay, vec![x("a")]);
            assert!(legacy_only_in_baseline.is_empty());
            claims
        }
        other => panic!("expected indeterminate: {other:?}"),
    };
    assert_eq!(indeterminate(verdict(&pool).await, 4), 0);

    let pg = pg_program(program());
    act(&pool, &pg, "admit_x", "a").await;
    act(&pool, &pg, "retract_x", "a").await;
    act(&pool, &pg, "admit_x", "a").await;
    assert_eq!(
        indeterminate(verdict(&pool).await, 7),
        1,
        "a lawful readmit does not settle it"
    );
}

/// An admit removed from the log leaves the later retraction deleting a
/// claim the history never held, though the table agrees with the end.
#[tokio::test]
async fn a_delete_the_history_cannot_explain_diverges() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let admitted = act(&pool, &pg, "admit_x", "a").await;
    act(&pool, &pg, "admit_x", "b").await;
    let retracted = act(&pool, &pg, "retract_x", "a").await;
    assert!(matches!(
        verdict(&pool).await,
        VerifyOutcome::Consistent {
            transitions: 3,
            claims: 1
        }
    ));

    execute(
        &pool,
        &format!("DELETE FROM morpholog.audit WHERE transition_id = '{admitted}'"),
    )
    .await;
    let found = conflicts(verdict(&pool).await);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, retracted);
    assert!(found[0].1.contains("finds no such claim"), "{found:?}");
}

/// The claims table names the transition that admitted each claim, and
/// replay must name the same one.
#[tokio::test]
async fn the_claims_table_names_the_transition_replay_names() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let admitted = act(&pool, &pg, "admit_x", "a").await;
    execute(
        &pool,
        &format!(
            "UPDATE morpholog.claims SET asserted_in = '{}'",
            Uuid::nil()
        ),
    )
    .await;
    let found = conflicts(verdict(&pool).await);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, admitted);
    assert!(found[0].1.contains(&Uuid::nil().to_string()), "{found:?}");
}

/// The activation record must be the one its legacy prefix implies: a
/// baseline edited to launder a claim away no longer matches its digest,
/// and removing the record leaves ordered rows straight after legacy
/// ones.
#[tokio::test]
async fn the_activation_record_is_the_one_its_prefix_implies() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    let admitted = legacy(&pool, "admit_x", "a", "2026-06-01T00:00:01Z").await;
    execute(
        &pool,
        &format!(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ('X', '[{{\"type\": \"subject\", \"value\": \"a\"}}]', '{admitted}')"
        ),
    )
    .await;
    testing::activate(&pool).await.unwrap();
    let pg = pg_program(program());
    act(&pool, &pg, "admit_x", "b").await;
    assert!(matches!(
        verdict(&pool).await,
        VerifyOutcome::Consistent {
            transitions: 3,
            claims: 2
        }
    ));
    let activation: Uuid = sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    execute(
        &pool,
        "BEGIN;
         UPDATE morpholog.audit SET operations = '[]'
          WHERE transformation_name = 'morpholog.activate';
         DELETE FROM morpholog.claims WHERE arguments @> '[{\"value\": \"a\"}]';
         COMMIT;",
    )
    .await;
    let found = conflicts(verdict(&pool).await);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, activation);
    assert!(found[0].1.contains("its own baseline"), "{found:?}");

    execute(
        &pool,
        "DELETE FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .await;
    let VerifyOutcome::Divergent {
        only_in_replay,
        conflicts,
        ..
    } = verdict(&pool).await
    else {
        panic!("the record is gone");
    };
    assert_eq!(only_in_replay, vec![x("a")]);
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(
        conflicts[0].detail.contains("no activation record"),
        "{conflicts:?}"
    );
}
