//! Integration test for the trade-lifecycle example on the
//! transaction-time axis: bitemporal replay of effective-dated terms.
//!
//! The sync suite in `morpholog-examples` covers the effective (valid)
//! axis - the cap, amendment mechanics, and a settlement surviving a
//! later amendment. What needs PostgreSQL is the *other* axis: the audit
//! log and `--as-of` replay. The headline here is that one fixed
//! effective-date question (what quantity was effective on 2026-02-20?)
//! gets two different truthful answers depending on the transaction-time
//! coordinate you ask it at, because a backdated amendment arrives between
//! them. That is bitemporality, achieved with an ordinary date-carrying
//! claim plus the append-only audit log - no valid-time columns, no
//! temporal database.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ClaimInstance, Definition, EvalValue, Invariant};
use morpholog_examples::trade_lifecycle;
use morpholog_postgres::{PgPool, PgProposalOutcome, list_derived, list_derived_at};
use rust_decimal::Decimal;
use uuid::Uuid;

mod common;
use common::{date, dec, subj};

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

async fn reset_db(pool: &PgPool) {
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.rejections CASCADE")
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

fn expect_committed(outcome: PgProposalOutcome) -> Uuid {
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => {
            panic!("expected Committed; got Rejected({reason})")
        }
    }
}

fn invariants() -> Vec<Invariant> {
    trade_lifecycle::all_invariants()
}

fn definitions() -> Vec<Definition> {
    trade_lifecycle::definitions()
}

/// The effective-time read: from a set of `TermsTimeline` rows for a
/// trade, the quantity in force on `target` - the version with the latest
/// effective date on or before it. `TermsTimeline` rows are
/// `(trade, version_id, delivery_period, effective_from, quantity)`.
fn quantity_effective_on(
    rows: &[ClaimInstance],
    trade: &EvalValue,
    target: &EvalValue,
) -> Option<Decimal> {
    let EvalValue::Date(target_date) = target else {
        panic!("target must be a date")
    };
    rows.iter()
        .filter(|r| r.predicate.as_str() == "TermsTimeline" && r.args.first() == Some(trade))
        .filter_map(|r| match (r.args.get(3), r.args.get(4)) {
            (Some(EvalValue::Date(ef)), Some(EvalValue::Decimal(qty))) if ef <= target_date => {
                Some((*ef, *qty))
            }
            _ => None,
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, qty)| qty)
}

/// Build the two-step history both tests share:
///   tid1: capture t1, terms tv1 (qty 100, effective 2026-01-15)
///   (grant confirmation authority to `mo`)
///   tid2: amend to tv2 (qty 120, effective 2026-02-01) - backdated
/// Returns (tid1, tid2).
async fn captured_then_amended(pool: &PgPool) -> (Uuid, Uuid) {
    let tid1 = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
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
            &invariants(),
            &definitions(),
        )
        .await
        .unwrap(),
    );

    let _ = expect_committed(
        common::propose_pg_with_test_actor(
            pool,
            &trade_lifecycle::grant_confirm_authority(),
            vec![subj("mo"), subj("power")],
            &invariants(),
            &definitions(),
        )
        .await
        .unwrap(),
    );

    let tid2 = expect_committed(
        common::propose_pg_as(
            pool,
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
            &invariants(),
            &definitions(),
        )
        .await
        .unwrap(),
    );

    (tid1, tid2)
}

/// The bitemporal headline: one fixed effective-date question, two
/// truthful answers across transaction time. As of tid1 the backdated
/// amendment is not yet known, so 2026-02-20 sees qty 100; as of tid2 it
/// is known, so the same date sees qty 120.
#[tokio::test]
async fn backdated_amendment_changes_the_effective_answer_across_transaction_time() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (tid1, tid2) = captured_then_amended(&pool).await;

    let timeline = trade_lifecycle::terms_timeline();
    let t1 = subj("t1");
    let target = date("2026-02-20");

    let at_tid1 = list_derived_at(&pool, &timeline, &trade_lifecycle::definitions(), tid1)
        .await
        .unwrap();
    assert_eq!(
        quantity_effective_on(&at_tid1, &t1, &target),
        Some(Decimal::new(100, 0)),
        "as of tid1 the quantity effective on 2026-02-20 was 100 - tv2 not yet recorded"
    );

    let at_tid2 = list_derived_at(&pool, &timeline, &trade_lifecycle::definitions(), tid2)
        .await
        .unwrap();
    assert_eq!(
        quantity_effective_on(&at_tid2, &t1, &target),
        Some(Decimal::new(120, 0)),
        "as of tid2 the same effective date now answers 120 - the backdated amendment is known"
    );

    // Current state agrees with the latest transaction-time coordinate.
    let current = list_derived(&pool, &timeline, &trade_lifecycle::definitions())
        .await
        .unwrap();
    assert_eq!(
        quantity_effective_on(&current, &t1, &target),
        Some(Decimal::new(120, 0)),
        "current state must agree with as-of the latest transition"
    );
}

/// The replayed timeline contains only what was known at the coordinate:
/// one version as of tid1, two as of tid2. Pins that the as-of read does
/// not leak a later amendment into an earlier knowledge state.
#[tokio::test]
async fn as_of_tid1_timeline_omits_the_later_amendment() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (tid1, tid2) = captured_then_amended(&pool).await;

    let timeline = trade_lifecycle::terms_timeline();
    let t1 = subj("t1");

    let at_tid1 = list_derived_at(&pool, &timeline, &trade_lifecycle::definitions(), tid1)
        .await
        .unwrap();
    let rows_t1: Vec<_> = at_tid1
        .iter()
        .filter(|r| r.predicate.as_str() == "TermsTimeline" && r.args.first() == Some(&t1))
        .collect();
    assert_eq!(
        rows_t1.len(),
        1,
        "as of tid1 only the original terms version exists: {at_tid1:?}"
    );

    let at_tid2 = list_derived_at(&pool, &timeline, &trade_lifecycle::definitions(), tid2)
        .await
        .unwrap();
    let rows_t2: Vec<_> = at_tid2
        .iter()
        .filter(|r| r.predicate.as_str() == "TermsTimeline" && r.args.first() == Some(&t1))
        .collect();
    assert_eq!(
        rows_t2.len(),
        2,
        "as of tid2 both terms versions exist: {at_tid2:?}"
    );
}
