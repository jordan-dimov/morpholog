//! The executor's role as PostgreSQL enforces it: the process that holds
//! an external credential moves outbox rows through delivery and can do
//! nothing else with the database.
//!
//! The attacker modelled holds the executor's group role, through a login
//! granted it: a compromised or careless executor that runs any SQL its
//! role allows. It must not be able to change what an admitted intent
//! says, add or remove an intent, read or write claims or audit, or
//! propose. It can claim, deliver, retry and fail a row, which is all
//! delivery needs. Marking a row delivered without acting stays possible;
//! reconciliation against the outside system's own record exposes that
//! (the closed-loop execution example).
//!
//! Each test owns a scratch database and its roles, removed before it
//! starts and when it ends.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use std::time::Duration;

use jiff::Timestamp;
use morpholog_core::{Transformation, Transition};
use morpholog_postgres::{
    Deliverer, DeliveryOutcome, OutboxRow, PgPool, PgProgram, ProcessOutcome, apply_migrations,
    deployment_roles, process_one_outbox_row, propose_against_pg, provision_least_privilege,
};
use morpholog_test_support::{dec_str, subj};

mod common;
use common::{Deployment, attested, expect_committed, pg_program, run_sql};

const FIXTURE: &str = "\
program executor_fixture

predicate Order(order: Subject, qty: Decimal)
    unique by (order)

intent PlaceOrder(order: Subject, qty: Decimal)
intent NotifyDesk(order: Subject)

transformation place(order, qty):
    admit Order(order, qty)
    emit PlaceOrder(order, qty)
    emit NotifyDesk(order)
";

const LEASE: Duration = Duration::from_secs(30);

/// Statements the executor's role must be refused, each with what it
/// would have done. They name no row, so each is refused for privilege or
/// not at all.
const REFUSED: &[(&str, &str)] = &[
    (
        "rewrite an intent's arguments",
        "UPDATE morpholog.outbox SET arguments = '[]'::jsonb WHERE false",
    ),
    (
        "change an intent's idempotency key",
        "UPDATE morpholog.outbox SET idempotency_key = 'x' WHERE false",
    ),
    (
        "change an intent's type",
        "UPDATE morpholog.outbox SET intent_type = 'x' WHERE false",
    ),
    (
        "move an intent to another transition",
        "UPDATE morpholog.outbox SET transition_id = transition_id WHERE false",
    ),
    (
        "change an intent's identity",
        "UPDATE morpholog.outbox SET intent_id = intent_id WHERE false",
    ),
    (
        "change when an intent was enqueued",
        "UPDATE morpholog.outbox SET enqueued_at = now() WHERE false",
    ),
    (
        "record a compensation",
        "UPDATE morpholog.outbox SET compensation_transition_id = NULL WHERE false",
    ),
    (
        "add an intent",
        "INSERT INTO morpholog.outbox (intent_id) SELECT intent_id FROM morpholog.outbox WHERE false",
    ),
    (
        "remove an intent",
        "DELETE FROM morpholog.outbox WHERE false",
    ),
    ("read the claims", "SELECT 1 FROM morpholog.claims LIMIT 0"),
    (
        "admit a claim",
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in) \
         SELECT predicate_name, arguments, asserted_in FROM morpholog.claims WHERE false",
    ),
    (
        "read the audit log",
        "SELECT 1 FROM morpholog.audit LIMIT 0",
    ),
    (
        "append to the audit log",
        "INSERT INTO morpholog.audit (transition_id) \
         SELECT transition_id FROM morpholog.audit WHERE false",
    ),
    (
        "read the derived cache",
        "SELECT 1 FROM morpholog_read.derived_claims LIMIT 0",
    ),
];

/// What delivery itself does to a row, which the role must be allowed.
const ALLOWED: &[(&str, &str)] = &[
    ("read the outbox", "SELECT 1 FROM morpholog.outbox LIMIT 0"),
    (
        "move a row through delivery",
        "UPDATE morpholog.outbox SET status = status, attempt_count = attempt_count, \
         last_attempt_at = last_attempt_at, delivered_at = delivered_at, failed_at = failed_at, \
         failure_reason = failure_reason, next_attempt_at = next_attempt_at, \
         locked_by = locked_by, lock_expires_at = lock_expires_at WHERE false",
    ),
    (
        "check the schema version",
        "SELECT 1 FROM morpholog.schema_migrations LIMIT 0",
    ),
];

/// Run `sql` with `role`'s privileges; the SQLSTATE it was refused with.
async fn as_role(pool: &PgPool, role: &str, sql: &str) -> Option<String> {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE \"{role}\"")))
        .execute(&mut *conn)
        .await
        .unwrap();
    let outcome = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await;
    sqlx::raw_sql("RESET ROLE")
        .execute(&mut *conn)
        .await
        .unwrap();
    match outcome {
        Ok(_) => None,
        Err(sqlx::Error::Database(db)) => Some(db.code().unwrap_or_default().to_string()),
        Err(other) => panic!("{sql}: {other}"),
    }
}

fn program() -> PgProgram {
    pg_program(morpholog_surface::parse_program(FIXTURE).expect("the fixture parses"))
}

fn place() -> Transformation {
    morpholog_surface::parse_program(FIXTURE)
        .unwrap()
        .transformations
        .into_iter()
        .find(|t| t.name.as_str() == "place")
        .unwrap()
}

async fn place_order(pool: &PgPool, pg: &PgProgram, order: &str, qty: &str) {
    let transition = Transition {
        transformation_name: place().name,
        args: vec![subj(order), dec_str(qty)],
        actor: "desk".into(),
        decided_at: None,
    };
    expect_committed(
        propose_against_pg(pool, pg, &attested(&transition))
            .await
            .unwrap(),
    );
}

/// A deliverer that answers every row the same way.
struct Answers(DeliveryOutcome);

impl Deliverer for Answers {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        self.0.clone()
    }
}

async fn deliver_one(pool: &PgPool, intent_type: &str, outcome: DeliveryOutcome) -> ProcessOutcome {
    process_one_outbox_row(
        pool,
        "executor",
        intent_type,
        LEASE,
        &Answers(outcome),
        None,
        Timestamp::now(),
    )
    .await
    .expect("the executor's role is enough to deliver")
}

async fn audit_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM morpholog.audit")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_executor_delivers_retries_and_fails_and_touches_nothing_else() {
    let d = Deployment::new("morpholog_ci_exec_deliver").await;
    provision_least_privilege(&d.pool, &d.roles).await.unwrap();
    let pg = program();
    for (order, qty) in [("o1", "10"), ("o2", "5"), ("o3", "1")] {
        place_order(&d.pool, &pg, order, qty).await;
    }
    let audit_before = audit_rows(&d.pool).await;
    let executor = d.acting_as(d.executor()).await;

    assert!(matches!(
        deliver_one(&executor, "PlaceOrder", DeliveryOutcome::Delivered).await,
        ProcessOutcome::Delivered { .. }
    ));
    assert!(matches!(
        deliver_one(
            &executor,
            "PlaceOrder",
            DeliveryOutcome::Transient {
                next_attempt_at: Timestamp::now() + Duration::from_secs(3600),
            },
        )
        .await,
        ProcessOutcome::TransientRetry { .. }
    ));
    assert!(matches!(
        deliver_one(
            &executor,
            "PlaceOrder",
            DeliveryOutcome::NonRetryable {
                reason: "the venue refused it".to_string(),
            },
        )
        .await,
        ProcessOutcome::Failed { .. }
    ));

    // One intent type per worker: the desk's notifications were never
    // claimed by the order executor.
    let untouched: Vec<(String, i32)> = sqlx::query_as(
        "SELECT status, attempt_count FROM morpholog.outbox WHERE intent_type = 'NotifyDesk'",
    )
    .fetch_all(&d.pool)
    .await
    .unwrap();
    assert_eq!(untouched, vec![("pending".to_string(), 0); 3]);
    // A failed delivery is not a second governed decision.
    assert_eq!(audit_rows(&d.pool).await, audit_before);

    executor.close().await;
    d.finish().await;
}

#[tokio::test]
async fn the_executor_cannot_change_what_was_admitted_or_touch_the_record() {
    let d = Deployment::new("morpholog_ci_exec_refused").await;
    provision_least_privilege(&d.pool, &d.roles).await.unwrap();
    for (what, sql) in REFUSED {
        assert_eq!(
            as_role(&d.pool, d.executor(), sql).await.as_deref(),
            Some("42501"),
            "the executor could {what}, or was refused for another reason"
        );
    }
    for (what, sql) in ALLOWED {
        assert_eq!(
            as_role(&d.pool, d.executor(), sql).await,
            None,
            "the executor could not {what}"
        );
    }

    // Nor can it propose: the transaction is refused before anything is
    // recorded.
    let pg = program();
    let executor = d.acting_as(d.executor()).await;
    let transition = Transition {
        transformation_name: place().name,
        args: vec![subj("o1"), dec_str("10")],
        actor: "desk".into(),
        decided_at: None,
    };
    let refused = propose_against_pg(&executor, &pg, &attested(&transition)).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("permission denied")),
        "{refused:?}"
    );
    assert_eq!(audit_rows(&d.pool).await, 0);

    executor.close().await;
    d.finish().await;
}

#[tokio::test]
async fn the_writer_cannot_change_an_admitted_intent_either() {
    let d = Deployment::new("morpholog_ci_exec_writer").await;
    provision_least_privilege(&d.pool, &d.roles).await.unwrap();
    for (what, sql) in &REFUSED[..6] {
        assert_eq!(
            as_role(&d.pool, d.roles.writer(), sql).await.as_deref(),
            Some("42501"),
            "the writer could {what}"
        );
    }
    // The writer still enqueues intents, records compensations and runs
    // the delivery the executor does.
    for (what, sql) in [REFUSED[6], REFUSED[7], ALLOWED[1]] {
        assert_eq!(
            as_role(&d.pool, d.roles.writer(), sql).await,
            None,
            "the writer could not {what}"
        );
    }
    d.finish().await;
}

/// A deployment provisioned before the executor existed: its writer held
/// UPDATE on the whole outbox. Migrating narrows it, since grants add up
/// and only an explicit revoke takes the old one away; running
/// `init --least-privilege` again then adds the executor.
#[tokio::test]
async fn an_upgraded_deployment_loses_the_old_grant_and_gains_an_executor() {
    let d = Deployment::new("morpholog_ci_exec_upgrade").await;
    let (w, r) = (d.roles.writer(), d.roles.reader());
    // The state the previous release left: no executor column, a record of
    // two roles, and the old floor's table-wide grant.
    run_sql(
        &d.pool,
        &format!(
            "ALTER TABLE morpholog.deployment_roles DROP COLUMN executor_role;
             DELETE FROM morpholog.schema_migrations WHERE version = 29;
             CREATE ROLE {w} NOLOGIN; CREATE ROLE {r} NOLOGIN;
             INSERT INTO morpholog.deployment_roles (writer_role, reader_role) VALUES ('{w}', '{r}');
             GRANT USAGE ON SCHEMA morpholog TO {w};
             GRANT SELECT, INSERT, UPDATE ON morpholog.outbox TO {w};"
        ),
    )
    .await;
    assert_eq!(
        as_role(&d.pool, w, REFUSED[0].1).await,
        None,
        "the old floor let the writer rewrite an intent"
    );

    apply_migrations(&d.pool).await.unwrap();
    let recorded = deployment_roles(&d.pool).await.unwrap().unwrap();
    assert_eq!(recorded.executor(), None, "migrate creates no role");
    assert_eq!(
        as_role(&d.pool, w, REFUSED[0].1).await.as_deref(),
        Some("42501"),
        "the migrated floor still lets the writer rewrite an intent"
    );

    provision_least_privilege(&d.pool, &d.roles).await.unwrap();
    let recorded = deployment_roles(&d.pool).await.unwrap().unwrap();
    assert_eq!(
        recorded, d.roles,
        "the executor is recorded beside the others"
    );
    assert_eq!(
        as_role(&d.pool, d.executor(), ALLOWED[1].1).await,
        None,
        "the added executor cannot deliver"
    );
    assert_eq!(
        as_role(&d.pool, d.executor(), REFUSED[0].1)
            .await
            .as_deref(),
        Some("42501")
    );
    d.finish().await;
}
