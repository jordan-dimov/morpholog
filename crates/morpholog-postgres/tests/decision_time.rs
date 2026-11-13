//! `decided_at`: the instant a transformation body is decided at, given
//! by the adapter alone. Attacker capability: a caller who controls every
//! field of the proposal it submits, including a `Transition` carrying an
//! instant of its choosing, and who can make the deciding transaction wait.
//! The claims: the kernel sees the transaction's start, which is the audit
//! row's `committed_at`; one transaction gives one instant to every act; a
//! transaction that waits after it began is still decided as of its start;
//! and a replay, live or from a pack, decides each row at its recorded
//! instant, not at the time of the replay.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use std::time::Duration;

use jiff::Timestamp;
use morpholog_core::{EvalValue, PreparedProgram, Program, Subject, Transition};
use morpholog_postgres::{
    PgAtomicOutcome, PgPool, PgProposalOutcome, Proposal, create_checkpoint, export_pack,
    list_audit_rows, list_claims, propose_against_pg, propose_all_against_pg, rehearse_candidate,
    rehearse_candidate_against_pack,
};
use morpholog_surface::parse_program;
use morpholog_test_support::subj;
use uuid::Uuid;

mod common;
use common::{reset_db, test_pool};

const FIXTURE: &str = r"
program decision_time_fixture

predicate Stamped(item: Subject, at: Timestamp)
predicate Deadline(lot: Subject, at: Timestamp)
predicate Bid(lot: Subject, at: Timestamp)

transformation stamp(item):
    admit Stamped(item, decided_at)

transformation set_deadline(lot, at):
    admit Deadline(lot, at)

transformation bid(lot):
    bind Deadline(lot, closes)
    require decided_at strictly_before closes
    admit Bid(lot, decided_at)
";

fn fixture() -> Program {
    let p = parse_program(FIXTURE).expect("parses");
    p.validate().expect("validates");
    p
}

/// A proposal whose transition names an instant of the caller's choosing.
/// Nothing the adapter accepts carries it on.
fn act(transformation: &str, args: Vec<EvalValue>) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: transformation.into(),
        args,
        actor: Subject::from("clerk"),
        decided_at: Some("1999-01-01T00:00:00Z".parse().unwrap()),
    })
}

async fn commit(pool: &PgPool, transformation: &str, args: Vec<EvalValue>) -> Uuid {
    let pg = common::pg_program(fixture());
    match propose_against_pg(pool, &pg, &act(transformation, args))
        .await
        .unwrap()
    {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        other @ PgProposalOutcome::Rejected { .. } => {
            panic!("`{transformation}` must commit: {other:?}")
        }
    }
}

/// The instant a claim of `predicate` about `subject` was admitted with.
async fn instant_of(pool: &PgPool, predicate: &str, subject: &str) -> Timestamp {
    let claims = list_claims(pool).await.unwrap();
    let found: Vec<_> = claims
        .iter()
        .filter(|c| c.predicate.as_str() == predicate && c.args[0] == subj(subject))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one {predicate} about {subject}: {claims:?}"
    );
    let EvalValue::Timestamp(at) = found[0].args[1] else {
        panic!("{predicate} carries an instant: {:?}", found[0]);
    };
    at
}

async fn committed_at(pool: &PgPool, transition_id: Uuid) -> (Timestamp, Timestamp) {
    let rows = list_audit_rows(pool).await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.transition_id == transition_id)
        .expect("the row is on the record");
    (
        row.committed_at,
        row.ordered_at.expect("a new row has an order time"),
    )
}

#[tokio::test]
async fn the_body_reads_the_rows_committed_at_never_the_callers_instant() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let id = commit(&pool, "stamp", vec![subj("a")]).await;
    let (recorded, _) = committed_at(&pool, id).await;
    assert_eq!(
        instant_of(&pool, "Stamped", "a").await,
        recorded,
        "the kernel was given the transaction's start, which the row records"
    );
    assert_ne!(
        recorded,
        "1999-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
        "the instant the caller put on its transition never reached the body"
    );

    // A second transaction is a second decision, at its own later start.
    let second = commit(&pool, "stamp", vec![subj("b")]).await;
    let (later, _) = committed_at(&pool, second).await;
    assert_eq!(instant_of(&pool, "Stamped", "b").await, later);
    assert!(
        later > recorded,
        "each transaction decides at its own start"
    );
}

#[tokio::test]
async fn every_act_of_one_transaction_is_decided_at_one_instant() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let pg = common::pg_program(fixture());
    let outcome = propose_all_against_pg(
        &pool,
        &pg,
        &[
            act("stamp", vec![subj("a")]),
            act("stamp", vec![subj("b")]),
            act("stamp", vec![subj("c")]),
        ],
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, PgAtomicOutcome::Committed { .. }),
        "{outcome:?}"
    );
    let rows = list_audit_rows(&pool).await.unwrap();
    assert_eq!(rows.len(), 3);
    for (row, item) in rows.iter().zip(["a", "b", "c"]) {
        assert_eq!(
            row.committed_at, rows[0].committed_at,
            "one transaction start"
        );
        assert_eq!(instant_of(&pool, "Stamped", item).await, row.committed_at);
    }
}

/// A transaction that begins before a deadline and evaluates after it is
/// decided as of its start: the gate passes, the row's `committed_at` is
/// before the deadline, and its order time after. A claim of the close,
/// not a time gate, is what orders a bid against a close.
#[tokio::test]
async fn a_transaction_that_waits_is_decided_as_of_its_start() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let closes: Timestamp = {
        let at: jiff_sqlx::Timestamp = sqlx::query_scalar("SELECT now() + interval '2 seconds'")
            .fetch_one(&pool)
            .await
            .unwrap();
        at.to_jiff()
    };
    commit(
        &pool,
        "set_deadline",
        vec![subj("lot"), EvalValue::Timestamp(closes)],
    )
    .await;

    // Hold every claim read until after the deadline. The bid's
    // transaction begins now and then waits on its first read of state.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE morpholog.claims IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let bidding = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let pg = common::pg_program(fixture());
            propose_against_pg(&pool, &pg, &act("bid", vec![subj("lot")])).await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE wait_event_type = 'Lock' AND datname = current_database()",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the bid's transaction reaches the lock");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let past: bool = sqlx::query_scalar("SELECT clock_timestamp() > $1::timestamptz")
                .bind(jiff_sqlx::ToSqlx::to_sqlx(closes))
                .fetch_one(&pool)
                .await
                .unwrap();
            if past {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the deadline passes");
    holder.commit().await.unwrap();

    let outcome = bidding.await.unwrap().unwrap();
    let PgProposalOutcome::Committed { transition_id, .. } = outcome else {
        panic!("a bid begun before the close is decided before it: {outcome:?}");
    };
    let (began, ordered) = committed_at(&pool, transition_id).await;
    assert!(
        began < closes,
        "committed_at {began} is before the close {closes}"
    );
    assert!(
        ordered > closes,
        "ordered_at {ordered} is after the close {closes}"
    );
    assert_eq!(instant_of(&pool, "Bid", "lot").await, began);

    // Replayed after the deadline, live and from a pack, the bid is
    // decided at its recorded instant again, and still admitted.
    let prepared = PreparedProgram::new(fixture()).unwrap();
    let live = rehearse_candidate(&pool, &prepared, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(live.totals.same, 2, "{live:?}");
    assert_eq!(live.totals.would_reject + live.totals.would_error, 0);
    create_checkpoint(&pool, None, None).await.unwrap();
    let pack = export_pack(&pool, None).await.unwrap();
    let offline = rehearse_candidate_against_pack(&prepared, &pack, None, &mut |_| {}).unwrap();
    assert_eq!(offline.totals.same, 2, "{offline:?}");
}

/// A refusal's explanation is of the execution that refused, so it is
/// decided at that execution's instant: the gate on time is what it
/// names, not a missing decision time.
#[tokio::test]
async fn a_refusal_explains_itself_at_the_instant_it_was_decided() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit(
        &pool,
        "set_deadline",
        vec![
            subj("lot"),
            EvalValue::Timestamp("2000-01-01T00:00:00Z".parse().unwrap()),
        ],
    )
    .await;
    let pg = common::pg_program(fixture());
    let proposal = act("bid", vec![subj("lot")]);
    let refused =
        morpholog_postgres::propose_against_pg_with_rejection_state(&pool, &pg, &proposal)
            .await
            .unwrap();
    assert!(
        matches!(refused.outcome, PgProposalOutcome::Rejected { .. }),
        "{:?}",
        refused.outcome
    );
    let undated = Transition {
        transformation_name: "bid".into(),
        args: vec![subj("lot")],
        actor: Subject::from("clerk"),
        decided_at: None,
    };
    let explanation = refused
        .explanation(pg.prepared(), &undated)
        .expect("a refusal is explained");
    let morpholog_core::Verdict::Rejected(morpholog_core::Rejection::Gate(gate)) =
        &explanation.verdict
    else {
        panic!("the time gate refused: {:?}", explanation.verdict);
    };
    assert!(gate.gate.contains("decided_at"), "{}", gate.gate);
}
