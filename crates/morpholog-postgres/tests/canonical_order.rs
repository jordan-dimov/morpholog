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
use morpholog_postgres::{
    ClaimRead, PgError, PgPool, PgProgram, VerifyOutcome, begin_audit_tail, read_claims_at,
    read_claims_at_for_predicates, testing, verify_replay,
};
use morpholog_test_support::subj;
use uuid::Uuid;

const PROGRAMME: &str = "program canonical_order
predicate X(k: Subject)
transformation admit_x(k):
    admit X(k)
transformation retract_x(k):
    require X(k)
    retract X(k)
predicate Y(k: Subject)
transformation admit_y(k):
    require X(k)
    admit Y(k)
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

fn read(claim: ClaimInstance, asserted_in: Uuid) -> ClaimRead {
    ClaimRead { claim, asserted_in }
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
    let no_op = legacy(&pool, "admit_x", "a", "2026-06-01T00:00:03Z").await;
    testing::activate(&pool).await.unwrap();

    let indeterminate = |outcome: VerifyOutcome, rows: i64| match outcome {
        VerifyOutcome::Indeterminate {
            transitions,
            claims,
            legacy_only_in_baseline,
            legacy_only_in_replay,
        } => {
            assert_eq!(transitions, rows);
            assert_eq!(legacy_only_in_replay, vec![read(x("a"), no_op)]);
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

/// The same overlap, then a later legacy admit the table did make: both
/// sides hold the claim, but the replay says the no-op admitted it, so
/// the history is still uncertain.
#[tokio::test]
async fn legacy_uncertainty_includes_who_admitted_a_claim() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    legacy(&pool, "admit_x", "a", "2026-06-01T00:00:01Z").await;
    legacy(&pool, "retract_x", "a", "2026-06-01T00:00:02Z").await;
    let no_op = legacy(&pool, "admit_x", "a", "2026-06-01T00:00:03Z").await;
    let readmit = legacy(&pool, "admit_x", "a", "2026-06-01T00:00:04Z").await;
    execute(
        &pool,
        &format!(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ('X', '[{{\"type\": \"subject\", \"value\": \"a\"}}]', '{readmit}')"
        ),
    )
    .await;
    testing::activate(&pool).await.unwrap();
    match verdict(&pool).await {
        VerifyOutcome::Indeterminate {
            legacy_only_in_baseline,
            legacy_only_in_replay,
            ..
        } => {
            assert_eq!(legacy_only_in_baseline, vec![read(x("a"), readmit)]);
            assert_eq!(legacy_only_in_replay, vec![read(x("a"), no_op)]);
        }
        other => panic!("expected indeterminate: {other:?}"),
    }
}

/// A row whose operations are not the ones its proposal implies: it says
/// it admitted one claim and records inserting another. The replay still
/// matches the table, so only this check sees it.
#[tokio::test]
async fn operations_follow_the_claims_a_row_proposed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let admitted = act(&pool, &pg, "admit_x", "a").await;
    execute(
        &pool,
        &format!(
            "UPDATE morpholog.audit
                SET asserted_claims = '[{{\"predicate\": \"X\", \"args\": [{{\"type\": \"subject\", \"value\": \"b\"}}]}}]'
              WHERE transition_id = '{admitted}'"
        ),
    )
    .await;
    let found = conflicts(verdict(&pool).await);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, admitted);
    assert!(found[0].1.contains("do not follow the claims"), "{found:?}");
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

/// What the order does not promise. A reads X and passes its gate; B
/// retracts X and commits; A commits after it. The database accepts that
/// as A before B, since A read X before B removed it, but the order is
/// taken at the audit write, so the record puts B first. Replaying the
/// recorded operations still rebuilds the table exactly. Re-deciding A
/// against the prefix the order gives finds X gone, so `rehearse` reports
/// a refusal A never met: the order is the order of effects, not the
/// state each decision saw.
#[tokio::test]
async fn the_order_is_of_effects_not_of_what_each_decision_saw() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = std::sync::Arc::new(pg_program(program()));
    // A deployment's shape: the programme's indexes, and enough claims
    // between X and Y in key order that B's delete and A's insert touch
    // different index pages. On a few rows they share one, and the
    // false conflict aborts A instead.
    morpholog_postgres::provision_indexes(&pool, &[&pg], false)
        .await
        .unwrap();
    execute(
        &pool,
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'XPad', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', i::text)),
                '00000000-0000-0000-0000-000000000000'
           FROM generate_series(1, 5000) AS i;
         ANALYZE morpholog.claims;",
    )
    .await;
    act(&pool, &pg, "admit_x", "a").await;

    // An uncommitted row on A's key holds A after its read, before its
    // write.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         VALUES ('Y', '[{\"type\": \"subject\", \"value\": \"a\"}]', $1)",
    )
    .bind(Uuid::nil())
    .execute(&mut *holder)
    .await
    .unwrap();
    let a = {
        let (pool, pg) = (pool.clone(), pg.clone());
        tokio::spawn(async move { act(&pool, &pg, "admit_y", "a").await })
    };
    for waited in 0.. {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND query LIKE 'INSERT INTO morpholog.claims%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if blocked > 0 {
            break;
        }
        assert!(waited < 400, "A never blocked");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let b = act(&pool, &pg, "retract_x", "a").await;
    holder.rollback().await.unwrap();
    let a = a.await.unwrap();

    let order: Vec<Uuid> = sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit
         ORDER BY audit_order, act_ordinal, committed_at, transition_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(&order[1..], &[b, a], "B takes the earlier order");
    execute(
        &pool,
        "DELETE FROM morpholog.claims WHERE predicate_name = 'XPad'",
    )
    .await;
    assert!(matches!(
        verdict(&pool).await,
        VerifyOutcome::Consistent {
            transitions: 3,
            claims: 1
        }
    ));
    let rehearsal = morpholog_postgres::rehearse_candidate(&pool, pg.prepared())
        .await
        .unwrap();
    assert_eq!(rehearsal.totals.would_reject, 1, "{rehearsal:?}");
    assert_eq!(rehearsal.differences[0].transition_id, a.to_string());
}

/// Two transactions that write one audit row each and nothing else,
/// queued on the ordering lock after their triggers have begun: both
/// commit. A trigger that searched its table for the transaction's row
/// made each conflict with the other's insert, and one was aborted every
/// time. Plain rows keep every other read out of the picture.
#[tokio::test]
async fn writers_queued_on_the_ordering_lock_do_not_conflict() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let mut holder = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(625626, 0)")
        .execute(&mut *holder)
        .await
        .unwrap();
    let writers: Vec<_> = (0..2)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::raw_sql(sqlx::AssertSqlSafe(ORDERED_ROW))
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await
            })
        })
        .collect();
    for waited in 0.. {
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks
              WHERE locktype = 'advisory' AND classid = 625626 AND objid = 0 AND NOT granted",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if queued == 2 {
            break;
        }
        assert!(
            waited < 400,
            "the writers never queued on the ordering lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    sqlx::query("SELECT pg_advisory_unlock(625626, 0)")
        .execute(&mut *holder)
        .await
        .unwrap();
    for writer in writers {
        let outcome = writer.await.unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
    }
}

/// A transition is a fixed boundary only once every transaction ordered
/// before it has finished. A commit envelope hands its caller a
/// transition while an earlier-ordered writer may still be open; a resume
/// after it, or a read as of it, would then change its answer, or skip
/// the earlier rows for good, once that writer commits.
#[tokio::test]
async fn a_transition_is_a_boundary_only_once_the_orders_before_it_are_final() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let a = act(&pool, &pg, "admit_x", "a").await;
    let mut early = pool.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(EARLY_ROW))
        .execute(&mut *early)
        .await
        .unwrap();
    let b = act(&pool, &pg, "admit_x", "b").await;

    let unsettled = |e: PgError| matches!(e, PgError::TransitionNotSettled(id) if id == b);
    assert!(unsettled(read_claims_at(&pool, b).await.unwrap_err()));
    assert!(unsettled(
        read_claims_at_for_predicates(&pool, b, &[])
            .await
            .unwrap_err()
    ));
    assert!(unsettled(
        begin_audit_tail(&pool, Some(b)).await.err().unwrap()
    ));
    assert_eq!(read_claims_at(&pool, a).await.unwrap().len(), 1);
    assert!(begin_audit_tail(&pool, Some(a)).await.is_ok());

    early.commit().await.unwrap();
    let claims: Vec<ClaimInstance> = read_claims_at(&pool, b)
        .await
        .unwrap()
        .into_iter()
        .map(|read| read.claim)
        .collect();
    assert_eq!(claims, [x("a"), x("e"), x("b")]);
    let mut tail = begin_audit_tail(&pool, Some(a)).await.unwrap();
    let after_a: Vec<String> = tail
        .next_page()
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.transformation_name.to_string())
        .collect();
    assert_eq!(after_a, ["early", "admit_x"]);
}

/// An ordered row admitting `X(e)`, with the claim it inserts.
const EARLY_ROW: &str = "INSERT INTO morpholog.audit (
        transition_id, transformation_name, arguments, actor, invariant_epoch,
        invariants_checked, asserted_claims, retracted_claims, emitted_intents, attestation,
        parameters, model_hash, semantics_version, drawn_subjects, operations)
     VALUES (gen_random_uuid(), 'early', '[]', '{\"type\": \"subject\", \"value\": \"w\"}', 1,
        '[]', '[{\"predicate\": \"X\", \"args\": [{\"type\": \"subject\", \"value\": \"e\"}]}]',
        '[]', '[]', '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[]',
        'sha256:' || repeat('d', 64), 1, '[]',
        '[{\"op\": \"ins\", \"predicate\": \"X\", \"args\": [{\"type\": \"subject\", \"value\": \"e\"}]}]');
     INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
     SELECT 'X', '[{\"type\": \"subject\", \"value\": \"e\"}]', transition_id
       FROM morpholog.audit WHERE transformation_name = 'early'";

const ORDERED_ROW: &str = "INSERT INTO morpholog.audit (
        transition_id, transformation_name, arguments, actor, invariant_epoch,
        invariants_checked, asserted_claims, retracted_claims, emitted_intents, attestation,
        parameters, model_hash, semantics_version, drawn_subjects, operations)
     VALUES (gen_random_uuid(), 'probe', '[]', '{\"type\": \"subject\", \"value\": \"w\"}', 1,
        '[]', '[]', '[]', '[]', '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[]',
        'sha256:' || repeat('d', 64), 1, '[]', '[]')";

/// The trigger remembers its transaction's row in a setting the session
/// can also write. Attacker capability: a writer setting it by hand, to
/// borrow a committed transaction's order or to draw a second one.
#[tokio::test]
async fn a_transaction_cannot_choose_its_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let mut committed = pool.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(ORDERED_ROW))
        .execute(&mut *committed)
        .await
        .unwrap();
    let borrowed: String =
        sqlx::query_scalar("SELECT current_setting('morpholog.audit_order_row')")
            .fetch_one(&mut *committed)
            .await
            .unwrap();
    committed.commit().await.unwrap();

    let mut forger = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('morpholog.audit_order_row', $1, true)")
        .bind(&borrowed)
        .execute(&mut *forger)
        .await
        .unwrap();
    let err = sqlx::raw_sql(sqlx::AssertSqlSafe(ORDERED_ROW))
        .execute(&mut *forger)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not its own"), "{err}");
    forger.rollback().await.unwrap();

    // Forgetting its row would draw a second order, and the table holds
    // one per transaction.
    let mut twice = pool.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "{ORDERED_ROW}; SELECT set_config('morpholog.audit_order_row', '', true)"
    )))
    .execute(&mut *twice)
    .await
    .unwrap();
    let err = sqlx::raw_sql(sqlx::AssertSqlSafe(ORDERED_ROW))
        .execute(&mut *twice)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("audit_order_txn_pkey"), "{err}");
    twice.rollback().await.unwrap();
}
