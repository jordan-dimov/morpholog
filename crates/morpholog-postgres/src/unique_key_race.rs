//! Two admissions of one `unique by` key, overlapping.
//!
//! A uniqueness check of a key of several fields seeks one index over the
//! key. Serializable isolation tracks what a check read by the index it
//! read through, so the access path decides whether two acts that admit
//! the same key see each other. These tests hold one act after it has
//! written its row and run its checks, let another run to the end, and
//! check what serializable isolation then allows: never two rows for one
//! key, and nothing but a retryable abort between acts on different keys.
//!
//! The attacker capability modelled is none: honest acts that overlap.
//! `DATABASE_URL`-gated.

use morpholog_core::{EvalValue, PreparedProgram, Subject, TransformationName, Transition};
use morpholog_test_support::{date, dec, subj};
use sqlx::PgPool;
use tokio::sync::oneshot;

use crate::propose::propose_against_pg_holding;
use crate::{PgError, PgProgram, PgProposalOutcome, Proposal, propose_against_pg};

const PRICE_CURVE: &str = "program price_curve
predicate PriceFile(file: Subject)
predicate Price(file: Subject, date: Date, slot: Decimal, minute: Decimal, price: Decimal)
    unique by (file, date, slot)
transformation open_file(file):
    require file_new: not PriceFile(file)
    admit PriceFile(file)
transformation set_price(file, date: Date, slot: Decimal, minute: Decimal, price: Decimal):
    require file_open: PriceFile(file)
    admit Price(file, date, slot, minute, price)
";

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for these tests (e.g. postgres:///morpholog_dev)");
    let pool = PgPool::connect(&crate::with_connection_defaults(&url))
        .await
        .expect("failed to connect to PostgreSQL test database");
    crate::testing::reset_database(&pool).await.expect("reset");
    pool
}

fn programme() -> PgProgram {
    let program = morpholog_surface::parse_program(PRICE_CURVE).expect("the curve parses");
    PgProgram::new(PreparedProgram::new(program).expect("the curve prepares"))
}

fn proposal(act: &str, args: Vec<EvalValue>) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: TransformationName::from(act),
        args,
        actor: Subject::from("desk"),
        decided_at: None,
    })
}

fn set_price(day: &str, slot: i64, price: i64) -> Proposal {
    proposal(
        "set_price",
        vec![
            subj("file_1"),
            date(day),
            dec(slot),
            dec(slot * 30),
            dec(price),
        ],
    )
}

/// One file with ten days of half-hourly prices, and the indexes the
/// programme asks for: the key index and the one on `file`.
async fn set_up(pool: &PgPool, program: &PgProgram) {
    let mut acts = vec![proposal("open_file", vec![subj("file_1")])];
    for day in 1..=10 {
        for slot in 0..48 {
            acts.push(set_price(&format!("2026-04-{day:02}"), slot, 40 + slot));
        }
    }
    let seeded = crate::propose_all_against_pg(pool, program, &acts)
        .await
        .expect("the curve");
    assert!(matches!(seeded, crate::PgAtomicOutcome::Committed { .. }));
    crate::provision_indexes(pool, &[program], false)
        .await
        .expect("indexes");
}

/// `held` decides, so it has written its row and run every check, and
/// waits; `other` runs to the end; then `held` commits. Their outcomes.
async fn overlapping(
    pool: &PgPool,
    program: &PgProgram,
    held: &Proposal,
    other: &Proposal,
) -> (
    Result<PgProposalOutcome, PgError>,
    Result<PgProposalOutcome, PgError>,
) {
    let (decided, decided_rx) = oneshot::channel::<()>();
    let (go, go_rx) = oneshot::channel::<()>();
    let held = propose_against_pg_holding(pool, program, held, async move || {
        decided.send(()).unwrap();
        go_rx.await.unwrap();
    });
    let other = async {
        decided_rx.await.unwrap();
        let outcome = propose_against_pg(pool, program, other).await;
        go.send(()).unwrap();
        outcome
    };
    tokio::join!(held, other)
}

/// The prices recorded for one day and slot.
async fn prices_at(pool: &PgPool, day: &str, slot: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT arguments -> 4 ->> 'value' FROM morpholog.claims
         WHERE predicate_name = 'Price'
           AND arguments -> 1 ->> 'value' = $1 AND arguments -> 2 ->> 'value' = $2
         ORDER BY 1",
    )
    .bind(day)
    .bind(slot.to_string())
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Two prices for one new day and slot, overlapping. Each is admissible
/// alone; together they break the key. At most one commits. The other is
/// refused by the rule, or aborted and refused by it on retry, so the
/// slot never holds two prices.
#[tokio::test]
async fn two_prices_for_one_key_never_both_commit() {
    let pool = pool().await;
    let program = programme();
    set_up(&pool, &program).await;
    let first = set_price("2026-04-11", 7, 50);
    let second = set_price("2026-04-11", 7, 60);
    let (first_outcome, second_outcome) = overlapping(&pool, &program, &first, &second).await;
    let committed = |o: &Result<PgProposalOutcome, PgError>| {
        matches!(o, Ok(PgProposalOutcome::Committed { .. }))
    };
    assert!(
        !(committed(&first_outcome) && committed(&second_outcome)),
        "both prices committed"
    );
    let by_the_key = |outcome: &PgProposalOutcome| {
        matches!(outcome, PgProposalOutcome::Rejected { rule, .. }
            if rule.as_deref() == Some("price_unique_by_file_date_slot"))
    };
    for (proposal, outcome) in [(&first, &first_outcome), (&second, &second_outcome)] {
        match outcome {
            Ok(PgProposalOutcome::Committed { .. }) => {}
            Ok(refused) => assert!(by_the_key(refused), "refused by the key: {refused:?}"),
            Err(PgError::SerializationFailure) => {
                let retried = propose_against_pg(&pool, &program, proposal).await.unwrap();
                assert!(
                    by_the_key(&retried),
                    "the retry is refused by the key: {retried:?}"
                );
            }
            Err(other) => panic!("an overlapping price commits, is refused or aborts: {other:?}"),
        }
    }
    assert_eq!(prices_at(&pool, "2026-04-11", 7).await.len(), 1);
}

/// Prices for two different slots of one day, overlapping: no rule links
/// them. Each commits, or is aborted as a serialization failure, safe to
/// retry, and its retry commits: both are recorded. This pins how a
/// conflict is handled, not that one occurs.
#[tokio::test]
async fn prices_for_two_keys_abort_only_retryably() {
    let pool = pool().await;
    let program = programme();
    set_up(&pool, &program).await;
    let first = set_price("2026-04-11", 7, 50);
    let second = set_price("2026-04-11", 8, 60);
    let (first_outcome, second_outcome) = overlapping(&pool, &program, &first, &second).await;
    for (proposal, outcome) in [(&first, &first_outcome), (&second, &second_outcome)] {
        match outcome {
            Ok(PgProposalOutcome::Committed { .. }) => {}
            Err(PgError::SerializationFailure) => {
                let retried = propose_against_pg(&pool, &program, proposal).await;
                assert!(
                    matches!(retried, Ok(PgProposalOutcome::Committed { .. })),
                    "the retry commits: {retried:?}"
                );
            }
            other => panic!("a price for its own key is admitted or retryable: {other:?}"),
        }
    }
    assert_eq!(prices_at(&pool, "2026-04-11", 7).await, ["50"]);
    assert_eq!(prices_at(&pool, "2026-04-11", 8).await, ["60"]);
}
