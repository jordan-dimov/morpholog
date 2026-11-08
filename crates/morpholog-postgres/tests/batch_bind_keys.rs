//! An atomic batch loads once. An act's read keyed by a value an earlier
//! `bind` finds is keyed through the snapshot, so a row an earlier act of
//! the same batch admits can name a dependant the keyed load never
//! fetched. The batch drops such a key, and the act decides as it would
//! over the whole state. `DATABASE_URL`-gated.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{reset_db, seed_claims, test_pool};
use morpholog_core::{PreparedProgram, Transition};
use morpholog_postgres::{PgAtomicOutcome, PgPool, PgProgram, Proposal, propose_all_against_pg};
use morpholog_test_support::{claim_instance, dec, subj, test_actor};

fn program() -> morpholog_core::Program {
    morpholog_surface::parse_program(
        "program batch_keys
predicate Allocation(a: Subject, item: Subject)
predicate Stock(item: Subject, on_hand: Decimal)
predicate Shipped(a: Subject)
transformation allocate(a, item):
    admit Allocation(a, item)
transformation unallocate(a):
    bind Allocation(a, item)
    retract Allocation(a, item)
transformation ship(a):
    bind Allocation(a, item)
    bind Stock(item, on_hand)
    require on_hand >= 1
    admit Shipped(a)
",
    )
    .unwrap()
}

fn act(name: &str, args: Vec<morpholog_core::EvalValue>) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: name.into(),
        args,
        actor: test_actor(),
        decided_at: None,
    })
}

/// Both evaluators: the interpreter loads the admits too, and must drop
/// the same key.
fn routes() -> [PgProgram; 2] {
    [
        PgProgram::interpreted(PreparedProgram::new(program()).unwrap()),
        PgProgram::new(PreparedProgram::new(program()).unwrap()),
    ]
}

async fn seed(pool: &PgPool) {
    reset_db(pool).await;
    seed_claims(
        pool,
        &[
            claim_instance("Stock", &[subj("i1"), dec(5)]),
            claim_instance("Allocation", &[subj("a_old"), subj("i9")]),
            claim_instance("Stock", &[subj("i9"), dec(5)]),
            claim_instance("Allocation", &[subj("a2"), subj("i2")]),
            claim_instance("Stock", &[subj("i2"), dec(5)]),
        ],
    )
    .await;
}

fn committed(outcome: &PgAtomicOutcome, acts: usize) {
    match outcome {
        PgAtomicOutcome::Committed { acts: receipts } => assert_eq!(receipts.len(), acts),
        rejected @ PgAtomicOutcome::Rejected { .. } => {
            panic!("the batch should commit: {rejected:?}")
        }
    }
}

/// Act 1 allocates an item no stored allocation names; act 2 ships that
/// allocation. The stock row act 2 binds is reachable only through act
/// 1's row, so act 2's stock read cannot stay keyed through the snapshot.
#[tokio::test]
async fn an_act_after_one_admitting_its_parent_still_finds_the_dependant() {
    let pool = test_pool().await;
    for route in routes() {
        seed(&pool).await;
        let outcome = propose_all_against_pg(
            &pool,
            &route,
            &[
                act("allocate", vec![subj("a_new"), subj("i1")]),
                act("ship", vec![subj("a_new")]),
            ],
        )
        .await
        .unwrap();
        committed(&outcome, 2);
        let shipped: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM morpholog.claims WHERE predicate_name = 'Shipped'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(shipped, 1);
    }
}

/// An earlier act that only retracts an allocation removes rows; the
/// shipment after it is decided over the keyed load as before.
#[tokio::test]
async fn an_act_after_one_retracting_a_parent_decides_over_the_keyed_load() {
    let pool = test_pool().await;
    for route in routes() {
        seed(&pool).await;
        let outcome = propose_all_against_pg(
            &pool,
            &route,
            &[
                act("unallocate", vec![subj("a_old")]),
                act("ship", vec![subj("a2")]),
            ],
        )
        .await
        .unwrap();
        committed(&outcome, 2);
    }
}
