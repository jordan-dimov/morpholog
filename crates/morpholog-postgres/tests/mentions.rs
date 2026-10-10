//! `--mentions`: the audit tail and the rejection log restricted to the
//! rows naming one subject. The subject is matched as a tagged value at
//! any depth, never as a substring; the actor is not searched; the
//! cursor and the horizon apply exactly as on the whole tail.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{dec, reset_db, subj, test_pool};

use jiff::Timestamp;
use morpholog_core::{EvalValue, Program};
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, audit_cursor_for, audit_resume_watermark, begin_audit_tail,
    list_audit_rows_page_mentioning, list_rejection_rows,
};
use morpholog_surface::parse_program;
use uuid::Uuid;

const FIXTURE: &str = r#"
program mentions_fixture

predicate Entry(entry_id: Subject, amount: Decimal)
predicate Tagged(tag: Subject, entry_id: Subject)
predicate Basket(basket: Subject, items: Collection)
predicate Limit(limit_id: Subject, cap: Decimal)
predicate Owner(entry_id: Subject, owner: Subject)

intent Notify(ref: Subject)

invariant within_limit:
    Entry(e, a) and Limit(l, cap) implies a <= cap

invariant owned_by_boss:
    Owner(e, o) implies o = #boss

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)

transformation tag(entry_id):
    let t = new Subject()
    admit Tagged(t, entry_id)

transformation notify(entry_id):
    let n = new Subject()
    emit Notify(n)

transformation bundle(basket, items):
    admit Basket(basket, items)

transformation set_limit(limit_id, cap):
    admit Limit(limit_id, cap)

transformation own(entry_id, owner):
    admit Owner(entry_id, owner)
"#;

fn fixture() -> Program {
    let p = parse_program(FIXTURE).expect("parses");
    p.validate().expect("validates");
    p
}

async fn run(pool: &PgPool, p: &Program, name: &str, args: Vec<EvalValue>) -> PgProposalOutcome {
    common::propose_pg_with_test_actor(
        pool,
        &common::pg_program(p.clone()),
        p.transformation(name).unwrap(),
        args,
    )
    .await
    .expect("no adapter error")
}

async fn commit(pool: &PgPool, p: &Program, name: &str, args: Vec<EvalValue>) -> Uuid {
    match run(pool, p, name, args).await {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => panic!("unexpected rejection: {reason}"),
    }
}

async fn refuse(pool: &PgPool, p: &Program, name: &str, args: Vec<EvalValue>) {
    match run(pool, p, name, args).await {
        PgProposalOutcome::Rejected { .. } => {}
        PgProposalOutcome::Committed { .. } => panic!("expected a refusal"),
    }
}

async fn mentioning(pool: &PgPool, subject: &str, after: Option<Uuid>) -> Vec<Uuid> {
    let mut tail = begin_audit_tail(pool, after, None)
        .await
        .unwrap()
        .mentioning(subject)
        .unwrap();
    let mut out = Vec::new();
    loop {
        let page = tail.next_page().await.unwrap();
        if page.is_empty() {
            break;
        }
        out.extend(page.iter().map(|r| r.transition_id));
    }
    out
}

#[tokio::test]
async fn a_subject_is_found_in_arguments_claims_intents_and_collections_and_nowhere_else() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let in_args = commit(&pool, &p, "post", vec![subj("e1"), dec(1)]).await;
    let in_claim = commit(&pool, &p, "tag", vec![subj("e1")]).await;
    let in_intent = commit(&pool, &p, "notify", vec![subj("e1")]).await;
    let in_collection = commit(
        &pool,
        &p,
        "bundle",
        vec![
            subj("b1"),
            EvalValue::Collection(vec![subj("e2"), subj("e1")]),
        ],
    )
    .await;
    let substring = commit(&pool, &p, "post", vec![subj("e10"), dec(1)]).await;
    let other = commit(&pool, &p, "post", vec![subj("z"), dec(1)]).await;

    assert_eq!(
        mentioning(&pool, "e1", None).await,
        vec![in_args, in_claim, in_intent, in_collection],
        "every row naming e1 as a tagged subject, in audit order; e10 is not e1"
    );
    assert!(mentioning(&pool, "nobody", None).await.is_empty());
    assert_eq!(mentioning(&pool, "e10", None).await, vec![substring]);
    assert_eq!(mentioning(&pool, "z", None).await, vec![other]);

    // The drawn subjects: named only in a claim, only in an intent.
    let mut conn = pool.acquire().await.unwrap();
    let rows = morpholog_postgres::list_audit_rows_page(&mut conn, None, None, 100)
        .await
        .unwrap();
    let tagged = rows.iter().find(|r| r.transition_id == in_claim).unwrap();
    let t = match &tagged.asserted_claims[0].args[0] {
        EvalValue::Subject(s) => s.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(mentioning(&pool, &t, None).await, vec![in_claim]);
    let notified = rows.iter().find(|r| r.transition_id == in_intent).unwrap();
    let n = match &notified.emitted_intents[0].args[0] {
        EvalValue::Subject(s) => s.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(mentioning(&pool, &n, None).await, vec![in_intent]);
}

#[tokio::test]
async fn the_actor_is_not_searched_and_the_subject_is_bound_not_spliced() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    // The test actor names itself in every row's actor column and in no
    // argument; the actor is a column of its own.
    let _ = commit(&pool, &p, "post", vec![subj("e1"), dec(1)]).await;
    let mut conn = pool.acquire().await.unwrap();
    let rows = morpholog_postgres::list_audit_rows_page(&mut conn, None, None, 1)
        .await
        .unwrap();
    let actor = rows[0].actor.to_string();
    assert!(mentioning(&pool, &actor, None).await.is_empty());

    let awkward = r#"o"1\2 ' $s"#;
    let row = commit(&pool, &p, "post", vec![subj(awkward), dec(1)]).await;
    assert_eq!(mentioning(&pool, awkward, None).await, vec![row]);
    assert!(mentioning(&pool, r#"o"1"#, None).await.is_empty());
}

#[tokio::test]
async fn the_cursor_may_name_a_transition_that_does_not_match() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let first = commit(&pool, &p, "post", vec![subj("e1"), dec(1)]).await;
    let unrelated = commit(&pool, &p, "post", vec![subj("z"), dec(1)]).await;
    let third = commit(&pool, &p, "tag", vec![subj("e1")]).await;
    assert_eq!(mentioning(&pool, "e1", None).await, vec![first, third]);
    assert_eq!(mentioning(&pool, "e1", Some(unrelated)).await, vec![third]);
    assert_eq!(
        mentioning(&pool, "e1", Some(third)).await,
        Vec::<Uuid>::new()
    );
}

/// The in-flight writer's row names the subject; the filtered tail
/// withholds it under the horizon and surfaces it under the next, as
/// the whole tail does.
#[tokio::test]
async fn the_filtered_tail_withholds_an_in_flight_writers_row_instead_of_losing_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = commit(&pool, &p, "post", vec![subj("s"), dec(1)]).await;

    let mut writer = pool.begin().await.unwrap();
    let writer_start: Timestamp =
        sqlx::query_scalar::<_, jiff_sqlx::Timestamp>("SELECT transaction_timestamp()")
            .fetch_one(&mut *writer)
            .await
            .unwrap()
            .to_jiff();
    common::insert_in_flight_audit_row_mentioning(&mut writer, Uuid::now_v7(), "s").await;
    let horizon = audit_resume_watermark(&pool, None).await.unwrap();
    assert!(horizon <= writer_start);
    writer.commit().await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let page = list_audit_rows_page_mentioning(&mut conn, None, Some(horizon), 10, "s")
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t1],
        "the in-flight row is withheld, t1 emitted"
    );
    let fresh = audit_resume_watermark(&pool, None).await.unwrap();
    let cursor = audit_cursor_for(&mut conn, t1).await.unwrap();
    let page = list_audit_rows_page_mentioning(&mut conn, Some(cursor), Some(fresh), 10, "s")
        .await
        .unwrap();
    assert_eq!(
        page.len(),
        1,
        "the withheld row surfaces under the next horizon"
    );
    assert_eq!(page[0].actor.as_str(), "in_flight");
}

#[tokio::test]
async fn a_refusal_is_found_by_its_arguments_its_witness_or_its_compared_values() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let _ = commit(&pool, &p, "set_limit", vec![subj("lim1"), dec(10)]).await;
    // Refused by within_limit: lim1 is in the witness only.
    refuse(&pool, &p, "post", vec![subj("e9"), dec(20)]).await;
    // Refused by owned_by_boss: boss is in the compared values only.
    refuse(&pool, &p, "own", vec![subj("e5"), subj("alice")]).await;

    let names = |rows: &[morpholog_postgres::RejectionRow]| {
        rows.iter()
            .map(|r| r.transformation_name.to_string())
            .collect::<Vec<_>>()
    };
    let all = list_rejection_rows(&pool, 10, None).await.unwrap();
    assert_eq!(names(&all), vec!["own", "post"], "newest first");
    let by_args = list_rejection_rows(&pool, 10, Some("e9")).await.unwrap();
    assert_eq!(names(&by_args), vec!["post"]);
    let by_witness = list_rejection_rows(&pool, 10, Some("lim1")).await.unwrap();
    assert_eq!(
        names(&by_witness),
        vec!["post"],
        "named in the witness only"
    );
    let by_compared = list_rejection_rows(&pool, 10, Some("boss")).await.unwrap();
    assert_eq!(
        names(&by_compared),
        vec!["own"],
        "named in the compared values only"
    );
    let by_actor = list_rejection_rows(&pool, 10, Some(&all[0].actor.to_string()))
        .await
        .unwrap();
    assert!(by_actor.is_empty(), "the actor is not searched");
    assert!(
        list_rejection_rows(&pool, 10, Some("e"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_empty_subject_is_refused_on_both_reads() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let tail = begin_audit_tail(&pool, None, None).await.unwrap();
    assert!(matches!(tail.mentioning(""), Err(PgError::InvalidState(_))));
    let mut conn = pool.acquire().await.unwrap();
    assert!(matches!(
        list_audit_rows_page_mentioning(&mut conn, None, None, 10, "").await,
        Err(PgError::InvalidState(_))
    ));
    assert!(matches!(
        list_rejection_rows(&pool, 10, Some("")).await,
        Err(PgError::InvalidState(_))
    ));
}
