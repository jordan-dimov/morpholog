//! Integration test for the trade-lifecycle example against PostgreSQL:
//! what the backdated-amendment scenario leaves on the record, and how
//! the record answers on the transaction-time axis.
//!
//! The in-memory suite in `morpholog-examples` decides the scenario; the
//! differentials hold PostgreSQL to the kernel's decisions in general.
//! This one covers what only the database has: the rejection log, the
//! audit, the outbox, and as-of replay. One question - what quantity was
//! effective on 2026-02-20? - has two true answers depending on when you
//! ask, because a backdated amendment arrives in between. A date-carrying
//! claim plus the audit log is enough; no temporal database is needed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ClaimInstance, EvalValue, State, Transformation};
use morpholog_examples::trade_lifecycle;
use morpholog_postgres::{
    PgPool, PgProgram, PgProposalOutcome, list_derived, list_derived_at, list_outbox_rows,
    list_rejection_rows,
};
use rust_decimal::Decimal;
use uuid::Uuid;

mod common;
use common::routes::{AUDIT_ROWS, CLAIM_ROWS, OUTBOX_ROWS, count, persisted};
use common::{date, dec, expect_committed, has_claim, reset_db, subj, test_pool};

async fn act(
    pool: &PgPool,
    pg: &PgProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    actor: &str,
) -> PgProposalOutcome {
    common::propose_pg_as(pool, pg, transformation, args, actor)
        .await
        .unwrap()
}

/// The audit, claims and outbox, as sorted rows.
async fn record(pool: &PgPool) -> [Vec<String>; 3] {
    [
        persisted(pool, AUDIT_ROWS).await,
        persisted(pool, CLAIM_ROWS).await,
        persisted(pool, OUTBOX_ROWS).await,
    ]
}

struct Scenario {
    tid1: Uuid,
    tid2: Uuid,
    /// The record just before and just after the refused slice.
    around_refusal: ([Vec<String>; 3], [Vec<String>; 3]),
}

/// The backdated-amendment scenario, on the record:
///   tid1: capture t1, terms tv1 (qty 100, effective 2026-01-15)
///   grant `mo` authority for power, confirm at 52 under op1
///   a slice of 110 effective 2026-02-20 is refused against qty 100
///   tid2: amend to tv2 (qty 120, effective 2026-02-01) - backdated
///   the same slice is admitted
async fn backdated_amendment(pool: &PgPool) -> Scenario {
    reset_db(pool).await;
    let pg = common::pg_program(trade_lifecycle::program());
    let settle = vec![
        subj("t1"),
        dec(110),
        subj("s1"),
        subj("op1"),
        date("2026-02-20"),
    ];
    let tid1 = expect_committed(
        act(
            pool,
            &pg,
            &trade_lifecycle::capture_trade(),
            vec![
                subj("t1"),
                subj("power"),
                subj("buy"),
                subj("tv1"),
                dec(100),
                subj("cal26"),
                date("2026-01-15"),
                dec(50),
            ],
            "desk",
        )
        .await,
    );
    expect_committed(
        act(
            pool,
            &pg,
            &trade_lifecycle::grant_confirm_authority(),
            vec![subj("mo"), subj("power")],
            "desk",
        )
        .await,
    );
    expect_committed(
        act(
            pool,
            &pg,
            &trade_lifecycle::confirm_trade(),
            vec![subj("t1"), subj("cp1"), subj("conf1"), subj("op1"), dec(52)],
            "mo",
        )
        .await,
    );

    let before = record(pool).await;
    let refused = act(
        pool,
        &pg,
        &trade_lifecycle::settle_trade(),
        settle.clone(),
        "desk",
    )
    .await;
    assert!(
        matches!(&refused, PgProposalOutcome::Rejected { rule: Some(r), .. }
            if r == "settled_within_effective_terms"),
        "110 over the original 100 is refused by the cap: {refused:?}"
    );
    let after = record(pool).await;

    let tid2 = expect_committed(
        act(
            pool,
            &pg,
            &trade_lifecycle::amend_trade_terms(),
            vec![
                subj("t1"),
                subj("tv1"),
                subj("tv2"),
                dec(120),
                subj("cal26"),
                date("2026-02-01"),
            ],
            "mo",
        )
        .await,
    );
    let admitted = act(
        pool,
        &pg,
        &trade_lifecycle::settle_trade(),
        settle.clone(),
        "desk",
    )
    .await;
    let PgProposalOutcome::Committed {
        asserted_claims, ..
    } = &admitted
    else {
        panic!("after the amendment the same slice is admitted: {admitted:?}");
    };
    assert!(
        has_claim(
            &State::from_claims(asserted_claims.clone()),
            "TradeSettled",
            &settle
        ),
        "the admitted slice asserts its settlement: {asserted_claims:?}"
    );
    Scenario {
        tid1,
        tid2,
        around_refusal: (before, after),
    }
}

/// The terms timeline as of `at`, or as it stands now.
async fn timeline(pool: &PgPool, at: Option<Uuid>) -> Vec<ClaimInstance> {
    let source = trade_lifecycle::program();
    let program = morpholog_test_support::validated(&source);
    let predicate = trade_lifecycle::terms_timeline().predicate;
    let rows = match at {
        Some(tid) => list_derived_at(pool, &program, predicate.as_str(), tid).await,
        None => list_derived(pool, &program, predicate.as_str()).await,
    };
    rows.unwrap()
        .expect("the programme derives it")
        .into_iter()
        .filter(|r| r.args.first() == Some(&subj("t1")))
        .collect()
}

/// From `TermsTimeline` rows `(trade, version_id, delivery_period,
/// effective_from, quantity)`, the quantity in force on 2026-02-20: the
/// version with the latest effective date on or before it.
fn quantity_on_feb_20(rows: &[ClaimInstance]) -> Option<Decimal> {
    let EvalValue::Date(target) = date("2026-02-20") else {
        unreachable!()
    };
    rows.iter()
        .filter_map(|r| match (r.args.get(3), r.args.get(4)) {
            (Some(EvalValue::Date(ef)), Some(EvalValue::Decimal(qty))) if *ef <= target => {
                Some((*ef, *qty))
            }
            _ => None,
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, qty)| qty)
}

/// As of tid1 the backdated amendment is not yet known: one version, and
/// 2026-02-20 sees qty 100. As of tid2 it is: two versions, and the same
/// date sees 120. A later amendment does not leak backwards, and the
/// current state agrees with the latest coordinate.
#[tokio::test]
async fn backdated_amendment_changes_the_effective_answer_across_transaction_time() {
    let pool = test_pool().await;
    let Scenario { tid1, tid2, .. } = backdated_amendment(&pool).await;

    let at_tid1 = timeline(&pool, Some(tid1)).await;
    assert_eq!(at_tid1.len(), 1, "only the original version: {at_tid1:?}");
    assert_eq!(quantity_on_feb_20(&at_tid1), Some(Decimal::new(100, 0)));

    let at_tid2 = timeline(&pool, Some(tid2)).await;
    assert_eq!(at_tid2.len(), 2, "both versions: {at_tid2:?}");
    assert_eq!(quantity_on_feb_20(&at_tid2), Some(Decimal::new(120, 0)));

    assert_eq!(
        quantity_on_feb_20(&timeline(&pool, None).await),
        Some(Decimal::new(120, 0))
    );
}

/// The refused slice changed nothing on the record but the rejection
/// log, where it left one row under its rule. The admitted slice, the
/// same proposal after the amendment, left its claim and exactly one
/// settlement request.
#[tokio::test]
async fn a_refused_slice_leaves_only_its_log_row_and_the_admitted_one_its_request() {
    let pool = test_pool().await;
    let scenario = backdated_amendment(&pool).await;
    let (before, after) = &scenario.around_refusal;
    for (table, (b, a)) in ["audit", "claims", "outbox"]
        .iter()
        .zip(before.iter().zip(after))
    {
        assert_eq!(b, a, "the refused slice changed {table}");
    }

    let rejections = list_rejection_rows(&pool, 10, None).await.unwrap();
    assert_eq!(rejections.len(), 1, "{rejections:?}");
    assert_eq!(
        (
            rejections[0].transformation_name.as_str(),
            rejections[0].kind.as_str(),
            rejections[0].rule.as_str()
        ),
        (
            "settle_trade",
            "invariant",
            "settled_within_effective_terms"
        )
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM morpholog.claims WHERE predicate_name = 'TradeSettled'"
        )
        .await,
        1
    );
    let requests = list_outbox_rows(&pool, None, Some("TradeSettlementRequested"))
        .await
        .unwrap();
    let payloads: Vec<_> = requests.iter().map(|r| r.arguments.clone()).collect();
    assert_eq!(payloads, [vec![subj("s1"), subj("t1"), dec(110)]]);
}
