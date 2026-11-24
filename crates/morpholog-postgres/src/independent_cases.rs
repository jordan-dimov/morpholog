//! Proposals on independent cases, overlapping.
//!
//! A book of resting orders on several contracts, with an invariant that
//! no contract's book is crossed. Case-local admission checks each order
//! only against its own contract's book, so an order on one contract
//! cannot change a decision on another. PostgreSQL's serializable
//! isolation tracks reads by index page, not by case, so overlapping
//! proposals on different contracts can still abort each other. What
//! holds today, and what these tests pin: such an abort is a
//! serialization failure, safe to retry, and the retry commits the same
//! claims. Proposals on one contract must conflict, and do.
//!
//! The attacker capability modelled is none: honest proposals that
//! overlap. `DATABASE_URL`-gated.

use morpholog_core::{EvalValue, PreparedProgram, Subject, TransformationName, Transition};
use morpholog_test_support::{dec, dec_str, subj};
use sqlx::PgPool;
use tokio::sync::oneshot;

use crate::propose::propose_against_pg_holding;
use crate::{PgError, PgProgram, PgProposalOutcome, Proposal, propose_against_pg};

const BOOK: &str = "program book

predicate Contract(contract: Subject)
predicate Resting(order: Subject, contract: Subject, side: Subject, price: Decimal, qty: Decimal)
    unique by (order)

invariant never_crossed:
    Resting(_, c, #buy, bid, _) and Resting(_, c, #sell, ask, _) implies bid < ask

transformation list_contract(contract):
    admit Contract(contract)

transformation rest(order, contract, side, price: Decimal, qty: Decimal):
    require listed: Contract(contract)
    admit Resting(order, contract, side, price, qty)
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
    let program = morpholog_surface::parse_program(BOOK).expect("the book parses");
    PgProgram::new(PreparedProgram::new(program).expect("the book prepares"))
}

fn proposal(act: &str, args: Vec<EvalValue>) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: TransformationName::from(act),
        args,
        actor: Subject::from("desk"),
        decided_at: None,
    })
}

fn rest(order: &str, contract: &str, side: &str, price: &str) -> Vec<EvalValue> {
    vec![
        subj(order),
        subj(contract),
        subj(side),
        dec_str(price),
        dec(1),
    ]
}

const CONTRACTS: [&str; 3] = ["contract-a", "contract-b", "contract-c"];

/// Three contracts listed, each with a book of 100 bids below 100 and 100
/// asks above 101, and indexes provisioned.
async fn set_up(pool: &PgPool, program: &PgProgram) {
    let mut acts = Vec::new();
    for contract in CONTRACTS {
        acts.push(proposal("list_contract", vec![subj(contract)]));
        for i in 0..100 {
            acts.push(proposal(
                "rest",
                rest(
                    &format!("{contract}-b{i}"),
                    contract,
                    "buy",
                    &format!("90.{i:02}"),
                ),
            ));
            acts.push(proposal(
                "rest",
                rest(
                    &format!("{contract}-s{i}"),
                    contract,
                    "sell",
                    &format!("101.{i:02}"),
                ),
            ));
        }
    }
    let seeded = crate::propose_all_against_pg(pool, program, &acts)
        .await
        .expect("the book");
    assert!(matches!(seeded, crate::PgAtomicOutcome::Committed { .. }));
    crate::provision_indexes(pool, &[program], false)
        .await
        .expect("indexes");
}

/// A new bid on each of `contracts`, at prices of its own.
fn three_bids(contracts: [&str; 3]) -> [Proposal; 3] {
    let mut bids = contracts
        .iter()
        .zip(["95.10", "95.20", "95.30"])
        .enumerate()
        .map(|(i, (contract, price))| {
            proposal("rest", rest(&format!("new-{i}"), contract, "buy", price))
        });
    [(); 3].map(|()| bids.next().unwrap())
}

/// The first two proposals each decide, so each has written its claim and
/// run every check, and are held before committing. The third runs to the
/// end. Then the second is let go, then the first. Their outcomes in that
/// order.
///
/// The compiled route writes before it checks, so two proposals alone
/// link at most one way through a shared page: the later reader finds the
/// earlier write already there. A third makes the chain: the second's
/// write lands where the first read, the third's where the second read,
/// and the third commits first, so the second is the pivot PostgreSQL
/// aborts.
async fn three_overlapping(
    pool: &PgPool,
    program: &PgProgram,
    [first, second, third]: &[Proposal; 3],
) -> [Result<PgProposalOutcome, PgError>; 3] {
    let (first_decided, first_decided_rx) = oneshot::channel::<()>();
    let (first_go, first_go_rx) = oneshot::channel::<()>();
    let (second_decided, second_decided_rx) = oneshot::channel::<()>();
    let (second_go, second_go_rx) = oneshot::channel::<()>();
    let (first_held, first_held_rx) = oneshot::channel::<()>();
    let (second_done, second_done_rx) = oneshot::channel::<()>();
    let first_act = propose_against_pg_holding(pool, program, first, async move || {
        first_decided.send(()).unwrap();
        first_go_rx.await.unwrap();
    });
    let second_act = async {
        first_held_rx.await.unwrap();
        let outcome = propose_against_pg_holding(pool, program, second, async move || {
            second_decided.send(()).unwrap();
            second_go_rx.await.unwrap();
        })
        .await;
        let _ = second_done.send(());
        outcome
    };
    let director = async {
        first_decided_rx.await.unwrap();
        first_held.send(()).unwrap();
        second_decided_rx.await.unwrap();
        let outcome = propose_against_pg(pool, program, third).await;
        second_go.send(()).unwrap();
        second_done_rx.await.unwrap();
        first_go.send(()).unwrap();
        outcome
    };
    let (first, second, third) = tokio::join!(first_act, second_act, director);
    [first, second, third]
}

/// The new bids' orders resting in the book.
async fn resting_new_bids(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT arguments -> 0 ->> 'value' FROM morpholog.claims
         WHERE predicate_name = 'Resting' AND arguments -> 0 ->> 'value' LIKE 'new-%'
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Three bids on three contracts, overlapping as above. No bid's check
/// reads another contract's book, yet PostgreSQL may abort one, since its
/// locks are per index page and the contracts' entries share pages.
/// Each bid commits or fails as a serialization failure, nothing
/// else, and a retry of each failed bid commits: all three rest. This
/// pins how a conflict is handled, not that one occurs: if PostgreSQL
/// stops linking the contracts, every bid commits and the test still
/// passes.
#[tokio::test]
async fn bids_on_independent_contracts_abort_only_retryably() {
    let pool = pool().await;
    let program = programme();
    set_up(&pool, &program).await;
    let bids = three_bids(CONTRACTS);
    let outcomes = three_overlapping(&pool, &program, &bids).await;
    for (bid, outcome) in bids.iter().zip(&outcomes) {
        match outcome {
            Ok(PgProposalOutcome::Committed { .. }) => {}
            Err(PgError::SerializationFailure) => {
                let retried = propose_against_pg(&pool, &program, bid).await;
                assert!(
                    matches!(retried, Ok(PgProposalOutcome::Committed { .. })),
                    "the retry commits: {retried:?}"
                );
            }
            other => panic!("a bid on its own contract is admitted or retryable: {other:?}"),
        }
    }
    assert_eq!(resting_new_bids(&pool).await, ["new-0", "new-1", "new-2"]);
}

/// The control: three bids on one contract, overlapping the same way. Each
/// check reads the book the others change, so one is aborted: the
/// conflict serializable isolation exists to catch.
#[tokio::test]
async fn bids_on_one_contract_conflict() {
    let pool = pool().await;
    let program = programme();
    set_up(&pool, &program).await;
    let bids = three_bids([CONTRACTS[0]; 3]);
    let outcomes = three_overlapping(&pool, &program, &bids).await;
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Err(PgError::SerializationFailure))),
        "one bid is aborted: {outcomes:?}"
    );
}

/// The control that must survive any change to how independent cases
/// run: a bid at 100.50 and an ask at 100.40 on one contract. Each is
/// admissible alone, since the book's bids stop at 90.99 and its asks
/// start at 101.00, but together they cross it. Overlapping, the first
/// decided and held before committing while the second runs to the end,
/// at most one may commit. The other is aborted, and its retry is
/// refused by the rule, so the book is never crossed.
#[tokio::test]
async fn a_bid_and_an_ask_that_cross_together_never_both_rest() {
    let pool = pool().await;
    let program = programme();
    set_up(&pool, &program).await;
    let contract = CONTRACTS[0];
    let bid = proposal("rest", rest("new-bid", contract, "buy", "100.50"));
    let ask = proposal("rest", rest("new-ask", contract, "sell", "100.40"));

    let (decided, decided_rx) = oneshot::channel::<()>();
    let (go, go_rx) = oneshot::channel::<()>();
    let held = propose_against_pg_holding(&pool, &program, &bid, async move || {
        decided.send(()).unwrap();
        go_rx.await.unwrap();
    });
    let other = async {
        decided_rx.await.unwrap();
        let outcome = propose_against_pg(&pool, &program, &ask).await;
        go.send(()).unwrap();
        outcome
    };
    let (bid_outcome, ask_outcome) = tokio::join!(held, other);

    let committed = |o: &Result<PgProposalOutcome, PgError>| {
        matches!(o, Ok(PgProposalOutcome::Committed { .. }))
    };
    assert!(
        !(committed(&bid_outcome) && committed(&ask_outcome)),
        "both crossing orders committed"
    );
    for (proposal, outcome) in [(&bid, &bid_outcome), (&ask, &ask_outcome)] {
        match outcome {
            Ok(PgProposalOutcome::Committed { .. }) => {}
            Err(PgError::SerializationFailure) => {
                let retried = propose_against_pg(&pool, &program, proposal).await;
                assert!(
                    matches!(retried, Ok(PgProposalOutcome::Rejected { .. })),
                    "the retry sees the other order and is refused: {retried:?}"
                );
            }
            other => panic!("an overlapping order commits or is aborted: {other:?}"),
        }
    }
    let resting: Vec<String> = sqlx::query_scalar(
        "SELECT arguments -> 0 ->> 'value' FROM morpholog.claims
         WHERE predicate_name = 'Resting' AND arguments -> 0 ->> 'value' LIKE 'new-%'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        resting.len(),
        1,
        "exactly one of the two rests: {resting:?}"
    );
}
