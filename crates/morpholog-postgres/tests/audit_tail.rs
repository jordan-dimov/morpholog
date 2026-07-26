//! The audit tail's read contract: keyset pages in `(committed_at,
//! transition_id)` order, a strictly-greater resume cursor, and the
//! start-time watermark that makes resume lossless - including the
//! race test that IS the contract's proof: an in-flight writer's row
//! sorts below rows a naive pager would already have emitted, so the
//! horizon must withhold it now and surface it next time, never lose
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{reset_db, test_pool};

use chrono::{DateTime, Utc};
use common::{dec, subj};
use morpholog_core::Program;
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, audit_cursor_for, audit_resume_watermark,
    list_audit_rows_page,
};
use morpholog_surface::parse_program;
use uuid::Uuid;

const FIXTURE: &str = r#"
program audit_tail_fixture

predicate Entry(entry_id: Subject, amount: Decimal)

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)
"#;

fn fixture() -> Program {
    let p = parse_program(FIXTURE).expect("parses");
    p.validate().expect("validates");
    p
}

async fn post(pool: &PgPool, p: &Program, entry: &str) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(
        pool,
        &common::compiled(p.clone()),
        p.transformation("post").unwrap(),
        vec![subj(entry), dec(1)],
    )
    .await
    .expect("post commits");
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => panic!("unexpected rejection: {reason}"),
    }
}

#[tokio::test]
async fn pages_are_ordered_and_the_cursor_is_strictly_greater() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;
    let t2 = post(&pool, &p, "e2").await;
    let t3 = post(&pool, &p, "e3").await;

    let mut conn = pool.acquire().await.unwrap();

    // Limit 2 yields the first two in commit order; resuming from the
    // second's cursor yields exactly the third - the cursor row
    // itself is excluded.
    let page = list_audit_rows_page(&mut conn, None, None, 2)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t1, t2]
    );
    let cursor = audit_cursor_for(&mut conn, t2).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), None, 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t3]
    );

    // Resuming from the last row is an empty page, not an error.
    let cursor = audit_cursor_for(&mut conn, t3).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), None, 10)
        .await
        .unwrap();
    assert!(page.is_empty());
}

#[tokio::test]
async fn an_unknown_cursor_is_an_error_never_a_silent_restart() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let unknown = Uuid::now_v7();
    let err = audit_cursor_for(&mut conn, unknown)
        .await
        .expect_err("an unknown transition id must error");
    assert!(
        matches!(err, PgError::TransitionNotFound(id) if id == unknown),
        "got {err:?}"
    );
}

// THE RACE TEST - the proof-in-code behind the lossless-resume
// guarantee. `committed_at` is the WRITER's transaction start, while
// visibility follows commit order: a writer that started before the
// reader's snapshot but commits after it leaves a row that sorts
// BELOW rows the reader emits. Without the horizon, a resume cursor
// would skip that row forever. With it, the row is withheld now and
// surfaced by the next invocation's fresh horizon - no loss, no skip.
#[tokio::test]
async fn the_watermark_withholds_an_in_flight_writers_row_instead_of_losing_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;

    // Writer A: an open transaction whose start time is pinned before
    // the horizon is computed. The hand-written audit row stands in
    // for a propose whose SERIALIZABLE transaction is still in
    // flight; committed_at takes the schema default, A's now() = A's
    // transaction start.
    let mut writer = pool.begin().await.unwrap();
    let (writer_start,): (DateTime<Utc>,) = sqlx::query_as("SELECT transaction_timestamp()")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    common::insert_in_flight_audit_row(&mut writer, Uuid::now_v7()).await;

    // The horizon, computed while A is in flight, clamps at or below
    // A's start.
    let horizon = audit_resume_watermark(&pool, None).await.unwrap();
    assert!(
        horizon <= writer_start,
        "the horizon must trail the in-flight writer: {horizon} > {writer_start}"
    );

    // A commits AFTER the horizon was computed - the naive-pager
    // poison: its row's committed_at sorts at A's start, below
    // anything a horizon-free reader would now emit.
    writer.commit().await.unwrap();

    // First invocation: reads under the horizon. A's row is
    // withheld; t1 (committed long before) is emitted.
    let mut conn = pool.acquire().await.unwrap();
    let page = list_audit_rows_page(&mut conn, None, Some(horizon), 10)
        .await
        .unwrap();
    let emitted: Vec<Uuid> = page.iter().map(|r| r.transition_id).collect();
    assert_eq!(
        emitted,
        vec![t1],
        "the in-flight row is withheld, t1 emitted"
    );

    // Second invocation: a fresh horizon (no open transactions now)
    // surfaces A's row after the resume cursor. Nothing was lost.
    let fresh = audit_resume_watermark(&pool, None).await.unwrap();
    let cursor = audit_cursor_for(&mut conn, t1).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), Some(fresh), 10)
        .await
        .unwrap();
    assert_eq!(
        page.len(),
        1,
        "the withheld row surfaces under the next horizon"
    );
    assert_eq!(page[0].actor.as_str(), "in_flight");
}

// ============================================================
// The writer assertion - the managed-Postgres opt-in. The horizon is
// computed over the asserted roles' sessions only, after a same-
// statement catalog census verifies the assertion covers every
// non-superuser role that can write audit. The genuinely-hidden-
// session path cannot be reproduced here (it needs a second
// authenticated connection the dev/CI setup cannot make); the live
// managed deployment is that path's acceptance test.
// ============================================================

async fn session_is_superuser(pool: &PgPool) -> bool {
    let (rolsuper,): (bool,) =
        sqlx::query_as("SELECT rolsuper FROM pg_roles WHERE rolname = session_user")
            .fetch_one(pool)
            .await
            .unwrap();
    rolsuper
}

/// Roles are cluster-global and survive `reset_db`, so each test
/// drops-then-creates its own uniquely named roles (privileges first:
/// a role with a table grant refuses a bare DROP ROLE).
async fn recreate_roles(pool: &PgPool, roles: &[&str], setup: &[&str]) {
    for role in roles {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DO $$ BEGIN
                IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN
                    EXECUTE 'DROP OWNED BY {role}';
                    EXECUTE 'DROP ROLE {role}';
                END IF;
            END $$"
        )))
        .execute(pool)
        .await
        .unwrap();
    }
    for statement in setup {
        // Audited: `setup` is a literal slice each caller writes inline.
        sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_string()))
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn drop_roles(pool: &PgPool, roles: &[&str]) {
    for role in roles {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP OWNED BY {role}; DROP ROLE {role}"
        )))
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn an_unknown_asserted_role_is_refused_as_a_probable_typo() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let err = audit_resume_watermark(&pool, Some(&["no_such_role_209".to_string()]))
        .await
        .expect_err("an unknown role must refuse");
    assert!(
        matches!(&err, PgError::WriterRoleUnknown { roles } if roles == &vec!["no_such_role_209".to_string()]),
        "got {err:?}"
    );
}

#[tokio::test]
async fn an_empty_assertion_is_vacuous_and_refused() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let err = audit_resume_watermark(&pool, Some(&[]))
        .await
        .expect_err("an empty assertion must refuse");
    assert!(matches!(err, PgError::WriterAssertionEmpty), "got {err:?}");
}

#[tokio::test]
async fn the_census_names_every_unasserted_writer_and_a_complete_assertion_passes() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let me = common::session_user(&pool).await;
    // Three writer paths the census must catch: a direct grant, an
    // inherited membership, and a SET-ROLE-only membership (INHERIT
    // FALSE, SET TRUE). The NOLOGIN group itself holds no sessions and
    // must NOT be demanded.
    let roles = [
        "mtest209_direct",
        "mtest209_group",
        "mtest209_inheritor",
        "mtest209_setter",
        "mtest209_bystander",
    ];
    recreate_roles(
        &pool,
        &roles,
        &[
            "CREATE ROLE mtest209_direct LOGIN",
            "GRANT INSERT ON morpholog.audit TO mtest209_direct",
            "CREATE ROLE mtest209_group NOLOGIN",
            "GRANT INSERT ON morpholog.audit TO mtest209_group",
            "CREATE ROLE mtest209_inheritor LOGIN",
            "GRANT mtest209_group TO mtest209_inheritor",
            "CREATE ROLE mtest209_setter LOGIN",
            "GRANT mtest209_group TO mtest209_setter WITH INHERIT FALSE, SET TRUE",
            "CREATE ROLE mtest209_bystander LOGIN",
            "GRANT mtest209_group TO mtest209_bystander WITH INHERIT FALSE, SET FALSE",
        ],
    )
    .await;

    let err = audit_resume_watermark(&pool, Some(std::slice::from_ref(&me)))
        .await
        .expect_err("unasserted writers must refuse");
    let missing = match &err {
        PgError::WriterAssertionIncomplete { missing } => missing.clone(),
        other => panic!("expected WriterAssertionIncomplete, got {other:?}"),
    };
    for required in ["mtest209_direct", "mtest209_inheritor", "mtest209_setter"] {
        assert!(
            missing.iter().any(|m| m == required),
            "census must name {required}: {missing:?}"
        );
    }
    assert!(
        !missing.iter().any(|m| m == "mtest209_group"),
        "a NOLOGIN group with no session is not a session role: {missing:?}"
    );
    assert!(
        !missing.iter().any(|m| m == "mtest209_bystander"),
        "an INHERIT FALSE, SET FALSE membership confers no usable write \
         path and must not be demanded: {missing:?}"
    );

    // Asserting exactly what the census demanded (plus ourselves)
    // passes - built from the refusal so the test holds on databases
    // with pre-existing writer roles too.
    let mut complete = missing;
    complete.push(me);
    audit_resume_watermark(&pool, Some(&complete))
        .await
        .expect("the complete assertion passes");

    // Duplicates are deterministic and harmless.
    let mut doubled = complete.clone();
    doubled.extend(complete.clone());
    audit_resume_watermark(&pool, Some(&doubled))
        .await
        .expect("a duplicated assertion behaves identically");

    drop_roles(&pool, &roles).await;
}

#[tokio::test]
async fn the_asserted_horizon_ignores_sessions_outside_the_assertion() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        // A non-superuser test role would itself be in the census and
        // the unasserted-session setup below could not exist. The
        // superuser residue this test rides on is the documented one.
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let roles = ["mtest209_idle"];
    recreate_roles(&pool, &roles, &["CREATE ROLE mtest209_idle LOGIN"]).await;

    // Our own (superuser, therefore census-exempt and unasserted)
    // session holds an open transaction. The all-sessions horizon
    // would trail it; the asserted horizon must ignore it - that is
    // exactly the managed-host shape, where the ignored sessions are
    // the platform's.
    let mut writer = pool.begin().await.unwrap();
    let (writer_start,): (DateTime<Utc>,) = sqlx::query_as("SELECT transaction_timestamp()")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    let horizon = audit_resume_watermark(&pool, Some(&["mtest209_idle".to_string()]))
        .await
        .expect("an idle asserted role with no unasserted census entries passes");
    assert!(
        horizon > writer_start,
        "the unasserted session's transaction must not lower the horizon: \
         {horizon} <= {writer_start}"
    );
    writer.rollback().await.unwrap();

    drop_roles(&pool, &roles).await;
}

// The race test, replayed through the assertion: sessions OF the
// asserted role still constrain the horizon, so withhold-then-surface
// holds exactly as in the unasserted form above.
#[tokio::test]
async fn the_asserted_watermark_still_withholds_the_asserted_writers_in_flight_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;
    let me = common::session_user(&pool).await;

    let mut writer = pool.begin().await.unwrap();
    let (writer_start,): (DateTime<Utc>,) = sqlx::query_as("SELECT transaction_timestamp()")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    common::insert_in_flight_audit_row(&mut writer, Uuid::now_v7()).await;

    let horizon = audit_resume_watermark(&pool, Some(std::slice::from_ref(&me)))
        .await
        .expect("asserting the connecting role passes");
    assert!(
        horizon <= writer_start,
        "an asserted writer's in-flight transaction must trail the horizon"
    );
    writer.commit().await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let page = list_audit_rows_page(&mut conn, None, Some(horizon), 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t1],
        "the in-flight row is withheld under the asserted horizon"
    );
    let fresh = audit_resume_watermark(&pool, Some(std::slice::from_ref(&me)))
        .await
        .unwrap();
    let cursor = audit_cursor_for(&mut conn, t1).await.unwrap();
    let page = list_audit_rows_page(&mut conn, Some(cursor), Some(fresh), 10)
        .await
        .unwrap();
    assert_eq!(page.len(), 1, "the withheld row surfaces next invocation");
}

#[tokio::test]
async fn with_no_open_transactions_the_watermark_emits_everything_committed() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let t1 = post(&pool, &p, "e1").await;
    let t2 = post(&pool, &p, "e2").await;

    let horizon = audit_resume_watermark(&pool, None).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let page = list_audit_rows_page(&mut conn, None, Some(horizon), 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|r| r.transition_id).collect::<Vec<_>>(),
        vec![t1, t2],
        "a quiescent database's whole history sits below the horizon"
    );
}
