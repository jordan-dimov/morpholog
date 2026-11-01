//! What an as-of timestamp answers after the canonical order: the state
//! after every transition whose instant is at or before it, a row's
//! instant being when its transaction took its place in the order (its
//! transaction start for a row from before the order), and only once that
//! answer is final. The histories follow the real chronology: legacy
//! rows, the activation, ordered rows. `DATABASE_URL`-gated.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    drain_open_transactions, expect_committed, pg_program, propose_pg_as, reset_db, test_pool,
};
use jiff::{SignedDuration, Timestamp};
use morpholog_core::Program;
use morpholog_postgres::{
    PgError, PgPool, PgProgram, TreeVerification, VerifyOutcome, create_checkpoint, export_pack,
    list_audit_rows, read_claims_at, resolve_settled_instant, testing, verify_pack, verify_replay,
};
use morpholog_test_support::subj;
use uuid::Uuid;

const PROGRAMME: &str = "program as_of_instant
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

async fn act(pool: &PgPool, pg: &PgProgram, k: &str) -> Uuid {
    let t = program().transformation("admit_x").unwrap().clone();
    expect_committed(
        propose_pg_as(pool, pg, &t, vec![subj(k)], "w")
            .await
            .unwrap(),
    )
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

/// The claim a legacy admission left in the table.
async fn live_claim(pool: &PgPool, k: &str, asserted_in: Uuid) {
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         VALUES ('X', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $1::text)), $2)",
    )
    .bind(k)
    .bind(asserted_in)
    .execute(pool)
    .await
    .unwrap();
}

/// An ordered row admitting `X(k)`, placed by the trigger, with the claim
/// it inserts. On a transaction, part of that transaction's batch.
async fn ordered_row(executor: impl sqlx::PgExecutor<'_>, k: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "WITH placed AS (
            INSERT INTO morpholog.audit (
                transition_id, transformation_name, arguments, actor, invariant_epoch,
                invariants_checked, asserted_claims, retracted_claims, emitted_intents,
                attestation, parameters, model_hash, semantics_version, drawn_subjects, operations)
            VALUES ($1, 'admit_x', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)),
                '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]',
                jsonb_build_array(jsonb_build_object('predicate', 'X', 'args',
                    jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)))),
                '[]', '[]', '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[\"k\"]',
                'sha256:' || repeat('d', 64), 1, '[]',
                jsonb_build_array(jsonb_build_object('op', 'ins', 'predicate', 'X', 'args',
                    jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)))))
            RETURNING transition_id)
         INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'X', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)),
                transition_id
           FROM placed",
    )
    .bind(id)
    .bind(k)
    .execute(executor)
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

async fn row_of(pool: &PgPool, id: Uuid) -> morpholog_postgres::AuditRow {
    list_audit_rows(pool)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.transition_id == id)
        .unwrap_or_else(|| panic!("no audit row {id}"))
}

async fn instant_of(pool: &PgPool, id: Uuid) -> Timestamp {
    row_of(pool, id).await.instant()
}

async fn resolve(pool: &PgPool, at: Timestamp) -> Result<Uuid, PgError> {
    resolve_settled_instant(pool, at).await
}

/// The subjects of the `X` claims as of `id`, sorted.
async fn subjects_at(pool: &PgPool, id: Uuid) -> Vec<String> {
    let mut ks: Vec<String> = read_claims_at(pool, id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| match &c.claim.args[0] {
            morpholog_core::EvalValue::Subject(s) => s.to_string(),
            other => panic!("not a subject: {other:?}"),
        })
        .collect();
    ks.sort();
    ks
}

fn names(ks: &[&str]) -> Vec<String> {
    ks.iter().map(|k| (*k).to_string()).collect()
}

fn unsettled(result: &Result<Uuid, PgError>, at: Timestamp) -> bool {
    matches!(result, Err(PgError::InstantNotSettled(t)) if *t == at)
}

const US: SignedDuration = SignedDuration::from_micros(1);

#[tokio::test]
async fn an_instant_selects_the_log_prefix_at_or_before_it_across_the_activation() {
    let pool = test_pool().await;
    drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    let a = legacy(&pool, "admit_x", "a", "2026-06-01T00:00:01Z").await;
    let b = legacy(&pool, "admit_x", "b", "2026-06-01T00:00:02Z").await;
    let c = legacy(&pool, "admit_x", "c", "2026-06-01T00:00:02Z").await;
    assert!(c > b, "the tie is broken by id, so c must be the later id");
    for (k, id) in [("a", a), ("b", b), ("c", c)] {
        live_claim(&pool, k, id).await;
    }
    let at = |s: &str| -> Timestamp { s.parse().unwrap() };

    assert!(matches!(
        resolve(&pool, at("2026-06-01T00:00:00Z")).await,
        Err(PgError::NoTransitionAtOrBefore(_))
    ));
    assert_eq!(resolve(&pool, at("2026-06-01T00:00:01Z")).await.unwrap(), a);
    assert_eq!(resolve(&pool, at("2026-06-01T00:00:02Z")).await.unwrap(), c);
    assert_eq!(resolve(&pool, at("2026-06-01T00:00:03Z")).await.unwrap(), c);

    testing::activate(&pool).await.unwrap();
    let record: Uuid = sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let activated_at = instant_of(&pool, record).await;
    assert_eq!(resolve(&pool, activated_at - US).await.unwrap(), c);
    assert_eq!(resolve(&pool, activated_at).await.unwrap(), record);
    assert_eq!(subjects_at(&pool, record).await, names(&["a", "b", "c"]));

    let pg = pg_program(program());
    let d = act(&pool, &pg, "d").await;
    let e = act(&pool, &pg, "e").await;
    let (td, te) = (instant_of(&pool, d).await, instant_of(&pool, e).await);
    assert!(activated_at < td && td < te);
    assert_eq!(resolve(&pool, td).await.unwrap(), d);
    assert_eq!(resolve(&pool, te - US).await.unwrap(), d);
    assert_eq!(resolve(&pool, te).await.unwrap(), e);
    assert_eq!(
        subjects_at(&pool, e).await,
        names(&["a", "b", "c", "d", "e"])
    );
    assert!(matches!(
        verify_replay(&pool, &mut |_| {}).await.unwrap(),
        VerifyOutcome::Consistent { .. }
    ));
}

/// Every act of one transaction shares the transaction's instant, so an
/// instant selects a whole batch or none of it.
#[tokio::test]
async fn a_batch_has_one_instant_and_is_never_split() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let r = ordered_row(&pool, "r").await;
    let mut batch = pool.begin().await.unwrap();
    let p = ordered_row(&mut *batch, "p").await;
    let q = ordered_row(&mut *batch, "q").await;
    batch.commit().await.unwrap();

    let (rp, rq) = (row_of(&pool, p).await, row_of(&pool, q).await);
    assert_eq!((rp.audit_order, rq.audit_order), (Some(2), Some(2)));
    assert_eq!((rp.act_ordinal, rq.act_ordinal), (Some(0), Some(1)));
    assert_eq!(rp.instant(), rq.instant());
    let at = rp.instant();
    assert_eq!(resolve(&pool, at).await.unwrap(), q);
    assert_eq!(subjects_at(&pool, q).await, names(&["p", "q", "r"]));
    assert_eq!(resolve(&pool, at - US).await.unwrap(), r);
    assert_eq!(subjects_at(&pool, r).await, names(&["r"]));
}

/// A writer holding an order below rows already committed. Its instant is
/// above the last final row's and unknown until it commits, so only
/// instants up to that row's are answered; once it commits, the refused
/// instant answers and includes it, and the earlier answer stands.
#[tokio::test]
async fn an_instant_is_answered_only_once_no_open_order_can_precede_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let a = act(&pool, &pg, "a").await;
    let mut writer = pool.begin().await.unwrap();
    ordered_row(&mut *writer, "w").await;
    let b = act(&pool, &pg, "b").await;
    let (ta, tb) = (instant_of(&pool, a).await, instant_of(&pool, b).await);

    assert_eq!(resolve(&pool, ta).await.unwrap(), a);
    assert!(unsettled(&resolve(&pool, tb).await, tb));
    let now = Timestamp::now();
    assert!(unsettled(&resolve(&pool, now).await, now));

    writer.commit().await.unwrap();
    assert_eq!(resolve(&pool, tb).await.unwrap(), b);
    assert_eq!(subjects_at(&pool, b).await, names(&["a", "b", "w"]));
    assert_eq!(resolve(&pool, ta).await.unwrap(), a);
    assert_eq!(subjects_at(&pool, a).await, names(&["a"]));
}

/// An empty log with its first writer open has no final row yet, so no
/// instant is answered: not even as absent, since the writer's row may
/// land at or before it.
#[tokio::test]
async fn an_empty_log_with_a_writer_open_is_unsettled_not_absent() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let mut writer = pool.begin().await.unwrap();
    let w = ordered_row(&mut *writer, "w").await;
    let now = Timestamp::now();
    assert!(unsettled(&resolve(&pool, now).await, now));
    assert!(unsettled(
        &resolve(&pool, Timestamp::UNIX_EPOCH).await,
        Timestamp::UNIX_EPOCH
    ));

    writer.commit().await.unwrap();
    assert!(matches!(
        resolve(&pool, Timestamp::UNIX_EPOCH).await,
        Err(PgError::NoTransitionAtOrBefore(_))
    ));
    assert_eq!(resolve(&pool, Timestamp::now()).await.unwrap(), w);
}

/// A quiet log answers every instant up to its horizon's clock, and
/// refuses one that has not passed: a later writer could still be placed
/// there.
#[tokio::test]
async fn a_quiet_log_answers_up_to_now_and_refuses_the_future() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let a = act(&pool, &pg, "a").await;
    let now = Timestamp::now();
    assert_eq!(resolve(&pool, now).await.unwrap(), a);
    let later = now + SignedDuration::from_secs(3600);
    assert!(unsettled(&resolve(&pool, later).await, later));
}

async fn ordering_clock_us(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT last_value FROM morpholog.audit_ordered_at_us")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `quiet_at` in microseconds since the epoch, as the function returns it.
async fn quiet_horizon_us(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT (extract(epoch FROM quiet_at) * 1000000)::bigint FROM morpholog.audit_horizon()",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A quiet horizon persists the clock it certifies: the ordering clock is
/// moved up to it, never down, so a clock that steps back afterwards
/// cannot place a later order at or before an answer already given as
/// final. The sequence set an hour ahead stands in for a clock an hour
/// behind the fence.
#[tokio::test]
async fn a_quiet_horizon_fences_the_ordering_clock() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let a = act(&pool, &pg, "a").await;

    execute(
        &pool,
        "SELECT setval('morpholog.audit_ordered_at_us', 1000)",
    )
    .await;
    let before = Timestamp::now().as_microsecond();
    let quiet = quiet_horizon_us(&pool).await;
    assert!(
        quiet >= before,
        "the horizon is at the server clock, {quiet} < {before}"
    );
    assert_eq!(
        ordering_clock_us(&pool).await,
        quiet,
        "the clock is fenced at the horizon"
    );

    let ahead = before + 3_600_000_000;
    execute(
        &pool,
        &format!("SELECT setval('morpholog.audit_ordered_at_us', {ahead})"),
    )
    .await;
    let quiet = quiet_horizon_us(&pool).await;
    assert_eq!(quiet, ahead, "a fence never lowers the clock");
    let quiet_at = Timestamp::from_microsecond(quiet).unwrap();
    assert_eq!(resolve(&pool, quiet_at).await.unwrap(), a);

    let c = act(&pool, &pg, "c").await;
    let tc = instant_of(&pool, c).await;
    assert!(
        tc > quiet_at,
        "an order drawn after the horizon is above it: {tc} <= {quiet_at}"
    );
    assert_eq!(
        resolve(&pool, quiet_at).await.unwrap(),
        a,
        "the answer given as final stands"
    );
    assert_eq!(subjects_at(&pool, a).await, names(&["a"]));
    assert!(ordering_clock_us(&pool).await >= ahead);
}

/// The rule this replaces. Two writers whose transaction starts order the
/// other way round from their places in the log: an instant between the
/// two starts used to resolve to the earlier-started one, whose prefix in
/// the log holds the other, placed and started after the instant. It now
/// resolves to the row before both.
#[tokio::test]
async fn an_instant_between_two_starts_no_longer_selects_a_row_placed_after_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let r0 = act(&pool, &pg, "r0").await;
    let mut first_started = pool.begin().await.unwrap();
    sqlx::query("SELECT now()")
        .execute(&mut *first_started)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut second_started = pool.begin().await.unwrap();
    let b = ordered_row(&mut *second_started, "b").await;
    second_started.commit().await.unwrap();
    let a = ordered_row(&mut *first_started, "a").await;
    first_started.commit().await.unwrap();

    let (ra, rb) = (row_of(&pool, a).await, row_of(&pool, b).await);
    assert!(ra.committed_at < rb.committed_at, "a started first");
    assert!(rb.position() < ra.position(), "b is placed first");
    let between = ra.committed_at + rb.committed_at.duration_since(ra.committed_at) / 2;
    assert!(rb.instant() > between && ra.instant() > between);

    let old_rule: Uuid = sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit
         WHERE committed_at <= $1
         ORDER BY committed_at DESC, transition_id DESC
         LIMIT 1",
    )
    .bind(jiff_sqlx::ToSqlx::to_sqlx(between))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old_rule, a, "the old rule picked the earlier start");
    assert_eq!(subjects_at(&pool, a).await, names(&["a", "b", "r0"]));

    assert_eq!(resolve(&pool, between).await.unwrap(), r0);
    assert_eq!(subjects_at(&pool, r0).await, names(&["r0"]));
}

/// The live conflicts, with the two records otherwise agreeing.
async fn live_faults(pool: &PgPool) -> Vec<String> {
    match verify_replay(pool, &mut |_| {}).await.unwrap() {
        VerifyOutcome::Divergent {
            only_in_claims_table,
            only_in_replay,
            conflicts,
        } if only_in_claims_table.is_empty() && only_in_replay.is_empty() => {
            conflicts.into_iter().map(|c| c.detail).collect()
        }
        VerifyOutcome::Consistent { .. } => Vec::new(),
        other => panic!("expected conflicts alone: {other:?}"),
    }
}

/// The offline verdict of a pack exported now, under a checkpoint taken
/// now, so the mutation is inside the pack rather than a tamper against
/// an earlier root.
async fn offline_fault(pool: &PgPool) -> Option<String> {
    execute(pool, "DELETE FROM morpholog.audit_checkpoints").await;
    create_checkpoint(pool, None, None).await.unwrap();
    let pack = export_pack(pool, None).await.unwrap();
    match verify_pack(&pack, None).unwrap() {
        TreeVerification::Intact { .. } => None,
        TreeVerification::ActivationFaulty { detail } => Some(detail),
        other => panic!("expected intact or activation_faulty: {other:?}"),
    }
}

async fn assert_both_name(pool: &PgPool, fault: &str) {
    let live = live_faults(pool).await;
    assert!(
        live.iter().any(|f| f.contains(fault)),
        "live {fault}: {live:?}"
    );
    let offline = offline_fault(pool).await;
    assert!(
        offline.as_deref().is_some_and(|f| f.contains(fault)),
        "offline {fault}: {offline:?}"
    );
}

/// The chronology both verifiers hold, one definition: one instant per
/// order, strictly increasing across orders, the first at or after the
/// last legacy row's. Attacker capability: a role that rewrites
/// `ordered_at` in place, with no checkpoint over the rows it touches.
#[tokio::test]
async fn the_verifiers_hold_the_chronology_live_and_offline() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = pg_program(program());
    let mut batch = pool.begin().await.unwrap();
    let p = ordered_row(&mut *batch, "p").await;
    ordered_row(&mut *batch, "q").await;
    batch.commit().await.unwrap();
    let d = act(&pool, &pg, "d").await;
    assert!(live_faults(&pool).await.is_empty());

    // An act of the batch at another instant than its order's.
    execute(
        &pool,
        &format!("UPDATE morpholog.audit SET ordered_at = ordered_at + interval '1 second' WHERE transition_id = '{p}'"),
    )
    .await;
    assert_both_name(&pool, "differs from its order's").await;
    execute(
        &pool,
        &format!("UPDATE morpholog.audit SET ordered_at = ordered_at - interval '1 second' WHERE transition_id = '{p}'"),
    )
    .await;

    // A later order at the previous order's instant.
    execute(
        &pool,
        &format!(
            "UPDATE morpholog.audit SET ordered_at = (SELECT ordered_at FROM morpholog.audit WHERE transition_id = '{p}')
             WHERE transition_id = '{d}'"
        ),
    )
    .await;
    assert_both_name(&pool, "is not after order 1's").await;

    // The activation placed before the last legacy row.
    drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    legacy(&pool, "admit_x", "a", "2026-06-01T00:00:01Z").await;
    legacy(&pool, "retract_x", "a", "2026-06-01T00:00:02Z").await;
    testing::activate(&pool).await.unwrap();
    act(&pool, &pg, "e").await;
    assert!(live_faults(&pool).await.is_empty());
    execute(
        &pool,
        "UPDATE morpholog.audit SET ordered_at = '2026-06-01T00:00:01Z'
          WHERE transformation_name = 'morpholog.activate'",
    )
    .await;
    assert_both_name(&pool, "before the last legacy row's committed_at").await;
}
