//! The closed-loop execution example (`examples/21_closed_loop_execution/`)
//! end to end: an agent's order commits and enqueues one intent, a
//! custodied executor consumes it through the outbox lease and places it
//! at a venue, the venue's own record comes back in as claims, and the
//! reconciliation reads name what matched and what did not.
//!
//! The attacker modelled holds a venue credential outside the executor
//! (an old deployment's key, a login used by hand) and can place orders
//! the record never admitted; or runs a tampered executor that sends the
//! right reference with its own terms; or holds a database login of its
//! own and proposes under the operator's or the reporter's name. The
//! record cannot prevent the first two; it must expose them. The third it
//! refuses before anything is admitted.
//!
//! The venue is a fixture: a ledger of client references and the venue's
//! own order identifiers. It does not deduplicate on the client
//! reference, which is what makes a lease-loss redelivery act twice.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use morpholog_core::{EvalValue, Subject, Transformation};
use morpholog_examples::closed_loop_execution as cle;
use morpholog_postgres::{
    Deliverer, DeliveryOutcome, OutboxRow, PgError, PgPool, PgProgram, PgProposalOutcome,
    ProcessOutcome, list_derived, process_one_outbox_row, propose_against_pg,
};
use morpholog_test_support::{dec, dec_str, subj, validated};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

mod common;
use common::{
    attested, drop_roles_if_present, expect_committed, pg_program, propose_pg_as, recreate_roles,
    reset_db, session_is_superuser, session_user, test_pool,
};

const OPERATOR: &str = "ops";
const AGENT: &str = "agent";
const REPORTER: &str = "reporter";
const VENUE: &str = "epex_dayahead";
const INSTRUMENT: &str = "power_q1";
const INTENT_TYPE: &str = "PlaceOrder";
const LEASE: Duration = Duration::from_secs(30);

fn program() -> PgProgram {
    pg_program(cle::program())
}

// ============================================================
// The venue fixture
// ============================================================

/// One order the venue accepted, with the client reference the sender
/// supplied and the identifier the venue gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VenueOrder {
    client_ref: String,
    venue_order_id: String,
    instrument: String,
    side: String,
    qty: String,
    price: String,
}

/// The venue's own book, outside the record.
#[derive(Default)]
struct Venue {
    orders: Mutex<Vec<VenueOrder>>,
}

impl Venue {
    /// Accept an order under `client_ref` and hand back the venue's
    /// identifier. Every acceptance is a new order; the venue does not
    /// deduplicate on the client reference.
    fn accept(&self, client_ref: &str, terms: [&str; 4]) -> String {
        let mut orders = self.orders.lock().unwrap();
        let venue_order_id = format!("V-{}", 1001 + orders.len());
        orders.push(VenueOrder {
            client_ref: client_ref.to_string(),
            venue_order_id: venue_order_id.clone(),
            instrument: terms[0].to_string(),
            side: terms[1].to_string(),
            qty: terms[2].to_string(),
            price: terms[3].to_string(),
        });
        venue_order_id
    }

    fn orders(&self) -> Vec<VenueOrder> {
        self.orders.lock().unwrap().clone()
    }
}

/// One delivery attempt as the executor saw it: the row's stable
/// identities, so a redelivery can be held to the first attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attempt {
    intent_id: Uuid,
    transition_id: Uuid,
    idempotency_key: String,
    client_ref: String,
}

/// The custodied executor: the only holder of the venue credential. It
/// sends each admitted intent as a client order whose reference is the
/// order's own subject, and decides nothing.
///
/// `crash_after_accept` models the ambiguous-success window once: the
/// venue accepts, then the worker loses its lease before the record
/// notes delivery. `scale_qty` models a tampered executor.
struct Executor {
    venue: Arc<Venue>,
    attempts: Mutex<Vec<Attempt>>,
    crash_after_accept: Mutex<Option<PgPool>>,
    scale_qty: i64,
}

impl Executor {
    fn new(venue: &Arc<Venue>) -> Self {
        Self {
            venue: Arc::clone(venue),
            attempts: Mutex::new(Vec::new()),
            crash_after_accept: Mutex::new(None),
            scale_qty: 1,
        }
    }

    fn attempts(&self) -> Vec<Attempt> {
        self.attempts.lock().unwrap().clone()
    }
}

fn text(value: &EvalValue) -> String {
    match value {
        EvalValue::Subject(s) => s.as_str().to_string(),
        EvalValue::Decimal(d) => d.normalize().to_string(),
        other => panic!("the intent carries subjects and decimals only, got {other:?}"),
    }
}

impl Deliverer for Executor {
    async fn deliver(&self, row: &OutboxRow) -> DeliveryOutcome {
        assert_eq!(row.intent_type, INTENT_TYPE);
        let [order, _venue, instrument, side, qty, price] = row.arguments.as_slice() else {
            panic!("PlaceOrder carries six arguments, got {:?}", row.arguments);
        };
        let client_ref = text(order);
        let qty = match qty {
            EvalValue::Decimal(d) => (d * rust_decimal::Decimal::from(self.scale_qty))
                .normalize()
                .to_string(),
            other => panic!("qty is a decimal, got {other:?}"),
        };
        self.venue.accept(
            &client_ref,
            [&text(instrument), &text(side), &qty, &text(price)],
        );
        self.attempts.lock().unwrap().push(Attempt {
            intent_id: row.intent_id,
            transition_id: row.transition_id,
            idempotency_key: row.idempotency_key.clone(),
            client_ref,
        });
        let crash = self.crash_after_accept.lock().unwrap().take();
        if let Some(pool) = crash {
            sqlx::query(
                "UPDATE morpholog.outbox SET lock_expires_at = now() - interval '1 second'
                 WHERE intent_id = $1",
            )
            .bind(row.intent_id)
            .execute(&pool)
            .await
            .unwrap();
        }
        DeliveryOutcome::Delivered
    }
}

/// Run one worker over the outbox once.
async fn run_executor(pool: &PgPool, worker: &str, executor: &Executor) -> ProcessOutcome {
    process_one_outbox_row(
        pool,
        worker,
        INTENT_TYPE,
        LEASE,
        executor,
        None,
        Timestamp::now(),
    )
    .await
    .unwrap()
}

// ============================================================
// The record
// ============================================================

async fn commit_as(pool: &PgPool, t: &Transformation, args: Vec<EvalValue>, actor: &str) -> Uuid {
    expect_committed(
        propose_pg_as(pool, &program(), t, args, actor)
            .await
            .unwrap(),
    )
}

/// The desk on the record: the operator bound to this pool's login, the
/// trader and the reporter enrolled under the same login, one venue, a
/// mandate of fifty, the reporter speaking for the venue.
async fn desk(pool: &PgPool) {
    let login = subj(&session_user(pool).await);
    commit_as(
        pool,
        &cle::appoint_operator(),
        vec![subj(OPERATOR), login.clone()],
        OPERATOR,
    )
    .await;
    for principal in [AGENT, REPORTER] {
        commit_as(
            pool,
            &cle::enrol_login(),
            vec![subj(principal), login.clone()],
            OPERATOR,
        )
        .await;
    }
    commit_as(pool, &cle::declare_venue(), vec![subj(VENUE)], OPERATOR).await;
    commit_as(
        pool,
        &cle::grant_mandate(),
        vec![subj(AGENT), subj(INSTRUMENT), dec(50)],
        OPERATOR,
    )
    .await;
    commit_as(
        pool,
        &cle::grant_feed(),
        vec![subj(REPORTER), subj(VENUE)],
        OPERATOR,
    )
    .await;
}

/// The agent places an order; returns the order's subject.
async fn place(pool: &PgPool, side: &str, qty: i64, price: &str) -> EvalValue {
    let outcome = propose_pg_as(
        pool,
        &program(),
        &cle::place_order(),
        vec![
            subj(VENUE),
            subj(INSTRUMENT),
            subj(side),
            dec(qty),
            dec_str(price),
        ],
        AGENT,
    )
    .await
    .unwrap();
    match outcome {
        PgProposalOutcome::Committed {
            asserted_claims, ..
        } => asserted_claims
            .iter()
            .find(|c| c.predicate.as_str() == "OrderAuthorised")
            .expect("the act admits the authorisation")
            .args[0]
            .clone(),
        PgProposalOutcome::Rejected { reason, .. } => panic!("place_order refused: {reason}"),
    }
}

/// The reporter brings in every order on the venue's book, numbering the
/// messages from `first_sequence`.
async fn ingest(pool: &PgPool, venue: &Venue, first_sequence: i64) {
    for (i, order) in venue.orders().iter().enumerate() {
        commit_as(
            pool,
            &cle::observe_venue_report(),
            vec![
                subj(VENUE),
                dec(first_sequence + i as i64),
                subj(&order.client_ref),
                subj(&order.venue_order_id),
                subj(&order.instrument),
                subj(&order.side),
                dec_str(&order.qty),
                dec_str(&order.price),
            ],
            REPORTER,
        )
        .await;
    }
}

async fn finding(pool: &PgPool, predicate: &str) -> Vec<Vec<EvalValue>> {
    let p = cle::program();
    list_derived(pool, validated(&p), predicate)
        .await
        .unwrap()
        .expect("the programme derives it")
        .into_iter()
        .map(|c| c.args)
        .collect()
}

async fn audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM morpholog.audit")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn outbox_status(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT status FROM morpholog.outbox ORDER BY enqueued_at")
        .fetch_all(pool)
        .await
        .unwrap()
}

// ============================================================
// The loop closes
// ============================================================

#[tokio::test]
async fn an_admitted_order_is_placed_under_its_own_reference_and_the_venues_report_matches_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    desk(&pool).await;
    let venue = Arc::new(Venue::default());
    let executor = Executor::new(&venue);

    let order = place(&pool, "buy", 10, "48.5").await;
    assert_eq!(
        finding(&pool, "Unobserved").await,
        vec![vec![order.clone(), dec(10)]],
        "admitted and not yet at the venue"
    );

    let outcome = run_executor(&pool, "executor_a", &executor).await;
    assert!(
        matches!(outcome, ProcessOutcome::Delivered { .. }),
        "{outcome:?}"
    );
    assert_eq!(outbox_status(&pool).await, vec!["delivered".to_string()]);
    let book = venue.orders();
    assert_eq!(book.len(), 1);
    assert_eq!(
        book[0].client_ref,
        text(&order),
        "the venue's client reference is the order's own subject"
    );
    assert_eq!(
        (
            book[0].instrument.as_str(),
            book[0].side.as_str(),
            book[0].qty.as_str(),
            book[0].price.as_str()
        ),
        (INSTRUMENT, "buy", "10", "48.5")
    );

    ingest(&pool, &venue, 1).await;
    let matched = finding(&pool, "Matched").await;
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0][0], order);
    assert_eq!(matched[0][2], dec(10));
    for other in ["Unobserved", "Unauthorised", "Mismatched", "Ambiguous"] {
        assert!(finding(&pool, other).await.is_empty(), "{other}");
    }
}

#[tokio::test]
async fn an_order_placed_with_a_credential_outside_the_executor_is_the_unauthorised_finding() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    desk(&pool).await;
    let venue = Arc::new(Venue::default());
    let executor = Executor::new(&venue);

    let order = place(&pool, "buy", 10, "48.5").await;
    run_executor(&pool, "executor_a", &executor).await;
    // Somebody with the venue's credential, and no proposal here.
    let by_hand = venue.accept("manual-20261008-1", [INSTRUMENT, "sell", "100", "47"]);

    ingest(&pool, &venue, 1).await;
    let unauthorised = finding(&pool, "Unauthorised").await;
    assert_eq!(unauthorised.len(), 1, "{unauthorised:?}");
    assert_eq!(
        unauthorised[0][1..],
        [subj(&by_hand), dec(100), dec_str("47")],
        "the finding names the venue's identifier so someone can go and look"
    );
    let matched = finding(&pool, "Matched").await;
    assert_eq!(matched.len(), 1, "the lawful order still matches");
    assert_eq!(matched[0][0], order);
    assert!(finding(&pool, "Mismatched").await.is_empty());
    assert!(finding(&pool, "Unobserved").await.is_empty());
}

#[tokio::test]
async fn a_tampered_executor_that_keeps_the_reference_and_changes_the_size_is_mismatched() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    desk(&pool).await;
    let venue = Arc::new(Venue::default());
    let mut executor = Executor::new(&venue);
    executor.scale_qty = 10;

    let order = place(&pool, "buy", 10, "48.5").await;
    run_executor(&pool, "executor_a", &executor).await;
    assert_eq!(venue.orders()[0].qty, "100");

    ingest(&pool, &venue, 1).await;
    let mismatched = finding(&pool, "Mismatched").await;
    assert_eq!(mismatched.len(), 1, "{mismatched:?}");
    assert_eq!(mismatched[0][1..], [order.clone(), dec(100), dec(10)]);
    assert!(
        finding(&pool, "Matched").await.is_empty(),
        "the reference agreeing is not a match"
    );
    assert!(finding(&pool, "Unauthorised").await.is_empty());
}

#[tokio::test]
async fn a_lease_lost_after_the_venue_accepted_is_redelivered_with_the_same_identity_and_read_as_ambiguous()
 {
    let pool = test_pool().await;
    reset_db(&pool).await;
    desk(&pool).await;
    let venue = Arc::new(Venue::default());
    let executor = Executor::new(&venue);

    let order = place(&pool, "buy", 10, "48.5").await;
    let audit_before = audit_count(&pool).await;

    // First worker: the venue accepts, then the lease is gone before the
    // record can note delivery.
    *executor.crash_after_accept.lock().unwrap() = Some(pool.clone());
    let first = run_executor(&pool, "executor_a", &executor).await;
    assert!(
        matches!(first, ProcessOutcome::LeaseLost { .. }),
        "{first:?}"
    );
    assert_eq!(outbox_status(&pool).await, vec!["in_progress".to_string()]);
    assert_eq!(venue.orders().len(), 1);

    // Second worker reclaims the expired lease and delivers again; the
    // venue, which does not deduplicate, now holds two orders.
    let second = run_executor(&pool, "executor_b", &executor).await;
    assert!(
        matches!(second, ProcessOutcome::Delivered { .. }),
        "{second:?}"
    );
    assert_eq!(outbox_status(&pool).await, vec!["delivered".to_string()]);
    let attempts = executor.attempts();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0], attempts[1],
        "both attempts carry the same intent, transition, idempotency key and reference"
    );
    assert_eq!(attempts[0].client_ref, text(&order));
    let book = venue.orders();
    assert_eq!(book.len(), 2);
    assert_eq!(book[0].client_ref, book[1].client_ref);
    assert_ne!(book[0].venue_order_id, book[1].venue_order_id);
    assert_eq!(
        audit_count(&pool).await,
        audit_before,
        "redelivery is not a transition"
    );

    ingest(&pool, &venue, 1).await;
    assert_eq!(
        finding(&pool, "Ambiguous").await,
        vec![vec![order.clone(), dec(2)]]
    );
    assert_eq!(finding(&pool, "Matched").await.len(), 2);
    assert!(finding(&pool, "Unauthorised").await.is_empty());
}

// ============================================================
// A login cannot speak another party's name
// ============================================================

/// A pool that presents itself as `role`: one simulated gateway.
async fn gateway_pool(role: &str) -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let url = morpholog_postgres::with_default_user(&url);
    let role = role.to_string();
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _| {
            let role = role.clone();
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "SET SESSION AUTHORIZATION {role}"
                )))
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap()
}

async fn recreate_gateway_roles(pool: &PgPool, roles: &[&str]) {
    let statements: Vec<String> = roles
        .iter()
        .flat_map(|r| {
            [
                format!("CREATE ROLE {r} LOGIN"),
                format!("GRANT USAGE ON SCHEMA morpholog TO {r}"),
                format!("GRANT ALL ON ALL TABLES IN SCHEMA morpholog TO {r}"),
            ]
        })
        .collect();
    let borrowed: Vec<&str> = statements.iter().map(String::as_str).collect();
    recreate_roles(pool, roles, &borrowed).await;
}

#[tokio::test]
async fn the_agents_login_cannot_act_as_the_operator_or_the_reporter() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let roles = ["mtest_cle_ops", "mtest_cle_agent", "mtest_cle_reporter"];
    recreate_gateway_roles(&pool, &roles).await;
    let ops = gateway_pool("mtest_cle_ops").await;
    let agent = gateway_pool("mtest_cle_agent").await;

    // The operator appoints itself under its own login, then enrols the
    // other two under theirs.
    commit_as(
        &ops,
        &cle::appoint_operator(),
        vec![subj(OPERATOR), subj("mtest_cle_ops")],
        OPERATOR,
    )
    .await;
    for (principal, login) in [(AGENT, "mtest_cle_agent"), (REPORTER, "mtest_cle_reporter")] {
        commit_as(
            &ops,
            &cle::enrol_login(),
            vec![subj(principal), subj(login)],
            OPERATOR,
        )
        .await;
    }
    commit_as(&ops, &cle::declare_venue(), vec![subj(VENUE)], OPERATOR).await;
    commit_as(
        &ops,
        &cle::grant_mandate(),
        vec![subj(AGENT), subj(INSTRUMENT), dec(50)],
        OPERATOR,
    )
    .await;
    commit_as(
        &ops,
        &cle::grant_feed(),
        vec![subj(REPORTER), subj(VENUE)],
        OPERATOR,
    )
    .await;
    let audit_before = audit_count(&pool).await;

    // The agent's login, speaking as the operator, tries to raise its
    // own mandate; and speaking as the reporter, to plant a report.
    let as_operator = cle::grant_mandate();
    let as_reporter = cle::observe_venue_report();
    for (t, args, name) in [
        (
            &as_operator,
            vec![subj(AGENT), subj("gas_q1"), dec(500)],
            OPERATOR,
        ),
        (
            &as_reporter,
            vec![
                subj(VENUE),
                dec(1),
                subj("ref-of-my-choosing"),
                subj("V-9999"),
                subj(INSTRUMENT),
                subj("buy"),
                dec(10),
                dec_str("48.5"),
            ],
            REPORTER,
        ),
    ] {
        let transition = morpholog_core::Transition {
            transformation_name: t.name.clone(),
            args,
            actor: Subject::from(name),
        };
        let err = propose_against_pg(&agent, &program(), &attested(&transition))
            .await
            .expect_err("the agent's login may not speak for an armed name");
        assert!(
            matches!(&err, PgError::ActorAssertionUnauthorised { actor, login_role }
                if actor.as_str() == name && login_role == "mtest_cle_agent"),
            "{err:?}"
        );
    }
    assert_eq!(
        audit_count(&pool).await,
        audit_before,
        "a refused assertion records nothing"
    );
    assert!(finding(&pool, "Unauthorised").await.is_empty());

    // Under its own name the same login trades as granted.
    let outcome = propose_pg_as(
        &agent,
        &program(),
        &cle::place_order(),
        vec![
            subj(VENUE),
            subj(INSTRUMENT),
            subj("buy"),
            dec(10),
            dec_str("48.5"),
        ],
        AGENT,
    )
    .await
    .unwrap();
    expect_committed(outcome);

    drop(agent);
    drop(ops);
    drop_roles_if_present(&pool, &roles).await;
}
