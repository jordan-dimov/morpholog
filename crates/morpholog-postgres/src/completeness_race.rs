//! A receipt racing a completeness report, in example 27's programme.
//!
//! The network's system records receipts and says how far its record is
//! complete. A receipt may not be recorded before an instant the record is
//! already said to be complete through (`record_open`), and a trade
//! nomination expires only once the record is complete past its deadline and
//! every receipt by then is read (`every_receipt_read`). Each is a gate on
//! one act. These tests hold one act between reading the record and deciding,
//! while others commit, and check what serializable isolation then allows.
//! The attacker capability modelled is none: an honest system whose acts
//! overlap. `DATABASE_URL`-gated.

use std::future::Future;

use morpholog_core::{EvalValue, PreparedProgram, Subject, TransformationName, Transition};
use morpholog_test_support::{date, dec, subj, ts};
use sqlx::PgPool;
use tokio::sync::oneshot;

use crate::propose::propose_against_pg_pausing;
use crate::{PgError, PgProgram, PgProposalOutcome, Proposal, propose_against_pg};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the race tests (e.g. postgres:///morpholog_dev)");
    let pool = PgPool::connect(&crate::with_connection_defaults(&url))
        .await
        .expect("failed to connect to PostgreSQL test database");
    crate::testing::reset_database(&pool).await.expect("reset");
    pool
}

fn programme() -> PgProgram {
    let program = morpholog_examples::gas_day_cash_out::program();
    PgProgram::new(PreparedProgram::new(program).expect("example 27 prepares"))
}

fn proposal(act: &str, actor: &str, args: Vec<EvalValue>) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: TransformationName::from(act),
        args,
        actor: Subject::from(actor),
        decided_at: None,
    })
}

async fn commit(pool: &PgPool, program: &PgProgram, act: &str, actor: &str, args: Vec<EvalValue>) {
    let outcome = propose_against_pg(pool, program, &proposal(act, actor, args))
        .await
        .unwrap_or_else(|e| panic!("{act}: {e}"));
    assert!(
        matches!(outcome, PgProposalOutcome::Committed { .. }),
        "{act} commits: {outcome:?}"
    );
}

/// The rule that refused, or `None` for a commit.
fn refused_by(outcome: &PgProposalOutcome) -> Option<&str> {
    match outcome {
        PgProposalOutcome::Committed { .. } => None,
        PgProposalOutcome::Rejected { rule, .. } => Some(rule.as_deref().unwrap_or("?")),
    }
}

fn receipt(message: &str, at: &str) -> Proposal {
    proposal(
        "receive",
        "gemini_receiver",
        vec![subj(message), subj("gemini"), ts(at)],
    )
}

fn completeness(until: &str) -> Proposal {
    proposal(
        "report_complete_through",
        "gemini_receiver",
        vec![subj("gemini"), ts(until)],
    )
}

fn expiry(message: &str) -> Proposal {
    proposal("expire_trade", "gemini_receiver", vec![subj(message)])
}

/// The network's receiver, and the calendar rows a trade nomination for the
/// gas day of 24 October 2026 reads.
///
/// PostgreSQL's predicate locks cover index pages, not keys. Every claim
/// insert first probes the claims table's key, which leads with the
/// predicate's name, and a nearly empty table keeps every key on one page:
/// two overlapping writers would each read the page the other writes, and
/// one would abort whatever their acts read. So the fixture admits three
/// hundred calendar rows of its own, whose predicate sorts between
/// `CompleteThrough` and `Received`, putting those two on different pages as
/// a real record would, and then plans its reads as a real record would:
/// indexes provisioned, statistics taken. Were the two ever to share a page
/// again, the receipt in the two-act race would abort and its test fail.
async fn set_up(pool: &PgPool, program: &PgProgram) {
    commit(
        pool,
        program,
        "appoint_operator",
        "operator",
        vec![subj("operator")],
    )
    .await;
    commit(
        pool,
        program,
        "appoint_receipt_authority",
        "operator",
        vec![subj("gemini"), subj("gemini_receiver")],
    )
    .await;
    for calendar in [
        "gas_day",
        "renomination_end",
        "trade_counterpart_deadline",
        "filler",
    ] {
        commit(
            pool,
            program,
            "appoint_calendar_publisher",
            "operator",
            vec![subj(calendar), subj("calendar_publisher")],
        )
        .await;
    }
    for shipper in ["shipper_a", "shipper_b"] {
        commit(
            pool,
            program,
            "grant_nomination",
            "operator",
            vec![subj("nominator"), subj(shipper)],
        )
        .await;
    }
    let rows = [
        (
            "gas_day",
            "2026-10-24",
            "2026-10-24T04:00:00Z",
            "2026-10-25T05:00:00Z",
        ),
        (
            "gas_day",
            "2026-09-24",
            "2026-09-24T04:00:00Z",
            "2026-09-25T04:00:00Z",
        ),
        (
            "renomination_end",
            "2026-10-25",
            "2026-10-25T03:00:00Z",
            "2026-10-26T03:00:00Z",
        ),
        (
            "trade_counterpart_deadline",
            "2026-10-24",
            "2026-10-24T05:00:00Z",
            "2026-10-25T06:00:00Z",
        ),
    ];
    let filler: Vec<Proposal> = (0..300)
        .map(|day| {
            let local_date = jiff::civil::date(2000, 1, 1) + jiff::Span::new().days(day);
            proposal(
                "publish_local_day",
                "calendar_publisher",
                vec![
                    subj("filler"),
                    date(&local_date.to_string()),
                    ts("2000-01-01T00:00:00Z"),
                    ts("2000-01-02T00:00:00Z"),
                    subj("2026e"),
                    subj("sha256:zone"),
                ],
            )
        })
        .collect();
    let padded = crate::propose_all_against_pg(pool, program, &filler)
        .await
        .expect("the filler");
    assert!(
        matches!(padded, crate::PgAtomicOutcome::Committed { .. }),
        "the filler commits"
    );
    for (calendar, local_date, starts_at, ends_at) in rows {
        commit(
            pool,
            program,
            "publish_local_day",
            "calendar_publisher",
            vec![
                subj(calendar),
                date(local_date),
                ts(starts_at),
                ts(ends_at),
                subj("2026e"),
                subj("sha256:zone"),
            ],
        )
        .await;
    }
    crate::provision_indexes(pool, &[program], false)
        .await
        .expect("indexes");
}

/// Run `paused` up to the point it has read the record, then `between`,
/// then let `paused` decide and commit.
async fn with_paused<T>(
    pool: &PgPool,
    program: &PgProgram,
    paused: &Proposal,
    between: impl Future<Output = T>,
) -> (Result<PgProposalOutcome, PgError>, T) {
    let (loaded, loaded_rx) = oneshot::channel::<()>();
    let (go, go_rx) = oneshot::channel::<()>();
    let held = propose_against_pg_pausing(pool, program, paused, async move || {
        loaded.send(()).expect("the test is waiting");
        go_rx.await.expect("the test releases it");
    });
    let director = async {
        loaded_rx.await.expect("the act read the record");
        let answer = between.await;
        go.send(()).expect("the act is waiting");
        answer
    };
    tokio::join!(held, director)
}

/// The acts in the order the audit log places them.
async fn audit_order(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT transformation_name FROM morpholog.audit
         WHERE transformation_name IN ('receive', 'report_complete_through', 'expire_trade')
         ORDER BY audit_order, act_ordinal",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Once the record is said to be complete through an instant, a receipt
/// before it is refused.
#[tokio::test]
async fn a_receipt_after_the_report_is_refused() {
    let pool = pool().await;
    let program = programme();
    set_up(&pool, &program).await;
    commit(
        &pool,
        &program,
        "report_complete_through",
        "gemini_receiver",
        vec![subj("gemini"), ts("2026-10-24T11:01:00Z")],
    )
    .await;
    let late = propose_against_pg(&pool, &program, &receipt("m", "2026-10-24T10:30:00Z"))
        .await
        .unwrap();
    assert_eq!(refused_by(&late), Some("record_open"));
}

/// A receipt that read the record open, before a report covering it
/// committed, is placed before the report by serializable isolation: it read
/// the absence the report then filled. Neither reads what the other writes
/// the other way, so both commit. But the receipt writes its audit row last,
/// so the log, which is in effect order, places it after the report. The
/// record still replays exactly. Rehearsing the unchanged programme decides
/// the receipt against the log's prefix, which holds the report, and so
/// reports it refused by `record_open`: the prefix is not the state the
/// receipt was decided on. That is #644's limitation, pinned here, not a
/// sign the receipt was wrongly admitted.
#[tokio::test]
async fn a_receipt_that_read_the_record_open_commits_before_the_report_it_precedes() {
    let pool = pool().await;
    let program = programme();
    set_up(&pool, &program).await;
    let (receipt_outcome, ()) = with_paused(
        &pool,
        &program,
        &receipt("m", "2026-10-24T10:30:00Z"),
        async {
            let report = propose_against_pg(&pool, &program, &completeness("2026-10-24T11:01:00Z"))
                .await
                .unwrap();
            assert_eq!(refused_by(&report), None, "the report commits");
        },
    )
    .await;
    let receipt_outcome = receipt_outcome.expect("the receipt is not a serialization failure");
    assert_eq!(refused_by(&receipt_outcome), None, "the receipt commits");
    assert_eq!(
        audit_order(&pool).await,
        ["report_complete_through", "receive"],
        "the log is in effect order"
    );

    let verified = crate::verify_replay(&pool, &mut |_| {}).await.unwrap();
    assert!(
        matches!(verified, crate::VerifyOutcome::Consistent { .. }),
        "the record replays: {verified:?}"
    );
    let report = crate::rehearse_candidate(&pool, program.prepared(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(report.totals.would_reject, 1, "{:?}", report.differences);
    let refused: Vec<_> = report
        .refusals
        .iter()
        .map(|r| (r.rule.as_str(), r.would_refuse))
        .collect();
    assert_eq!(refused, [("record_open", 1)]);
}

/// A side waiting, received at 10:00 within the day so its counterpart is
/// due by 11:00. Then three acts: the counterpart's receipt at 10:30 reads
/// the record open and waits; a report that the record is complete through
/// 11:01 commits; `third` reads the record with that report and waits. Then
/// the receipt and `third` are let go, the receipt first or `third` first,
/// each finishing before the other starts. Their outcomes, receipt first.
async fn three_acts(
    pool: &PgPool,
    program: &PgProgram,
    third: &Proposal,
    receipt_first: bool,
) -> (
    Result<PgProposalOutcome, PgError>,
    Result<PgProposalOutcome, PgError>,
) {
    commit(
        pool,
        program,
        "receive",
        "gemini_receiver",
        vec![subj("tn_a"), subj("gemini"), ts("2026-10-24T10:00:00Z")],
    )
    .await;
    commit(
        pool,
        program,
        "trade_nominate",
        "nominator",
        vec![
            subj("tn_a"),
            subj("shipper_a"),
            date("2026-10-24"),
            subj("disposing"),
            subj("shipper_b"),
            dec(1_000),
        ],
    )
    .await;

    let (receipt_loaded, receipt_loaded_rx) = oneshot::channel::<()>();
    let (receipt_go, receipt_go_rx) = oneshot::channel::<()>();
    let (third_loaded, third_loaded_rx) = oneshot::channel::<()>();
    let (third_go, third_go_rx) = oneshot::channel::<()>();
    let (report_done, report_done_rx) = oneshot::channel::<()>();
    let (receipt_done, receipt_done_rx) = oneshot::channel::<()>();
    let (third_done, third_done_rx) = oneshot::channel::<()>();
    let counterpart = receipt("tn_b", "2026-10-24T10:30:00Z");

    let receipt_act = async {
        let outcome = propose_against_pg_pausing(pool, program, &counterpart, async move || {
            receipt_loaded.send(()).unwrap();
            receipt_go_rx.await.unwrap();
        })
        .await;
        let _ = receipt_done.send(());
        outcome
    };
    let third_act = async {
        report_done_rx.await.unwrap();
        let outcome = propose_against_pg_pausing(pool, program, third, async move || {
            third_loaded.send(()).unwrap();
            third_go_rx.await.unwrap();
        })
        .await;
        let _ = third_done.send(());
        outcome
    };
    let director = async {
        receipt_loaded_rx.await.unwrap();
        let report = propose_against_pg(pool, program, &completeness("2026-10-24T11:01:00Z"))
            .await
            .unwrap();
        assert_eq!(refused_by(&report), None, "the report commits");
        report_done.send(()).unwrap();
        third_loaded_rx.await.unwrap();
        if receipt_first {
            receipt_go.send(()).unwrap();
            receipt_done_rx.await.unwrap();
            third_go.send(()).unwrap();
        } else {
            third_go.send(()).unwrap();
            third_done_rx.await.unwrap();
            receipt_go.send(()).unwrap();
        }
    };
    let (receipt_outcome, third_outcome, ()) = tokio::join!(receipt_act, third_act, director);
    (receipt_outcome, third_outcome)
}

fn committed(outcome: &Result<PgProposalOutcome, PgError>) -> bool {
    matches!(outcome, Ok(PgProposalOutcome::Committed { .. }))
}

/// The expiry reads the report but not the receipt. The receipt must
/// precede the report (it read the report's absence), the report the
/// expiry (the expiry read it), and the expiry the receipt (it read the
/// receipts, and this one was not there): no serial order has all three,
/// so they cannot all commit. Which of the receipt and the expiry is
/// aborted is PostgreSQL's choice, so both finishing orders are run and
/// neither is required to lose. What must never happen is a receipt and an
/// expiry that did not see it both standing. Retried, the aborted act meets
/// the other's evidence and is refused by the rule the example states.
///
/// The abort comes from what the expiry reads, not from how its rule uses
/// it: serializable isolation tracks reads. `every_receipt_read` and the
/// search for a counterpart in time both read the receipts; the functional
/// test of the example shows the rule itself is needed.
#[tokio::test]
async fn an_expiry_and_a_timely_receipt_it_did_not_see_never_both_commit() {
    for receipt_first in [true, false] {
        let pool = pool().await;
        let program = programme();
        set_up(&pool, &program).await;
        let (receipt_outcome, expiry_outcome) =
            three_acts(&pool, &program, &expiry("tn_a"), receipt_first).await;
        assert!(
            !(committed(&receipt_outcome) && committed(&expiry_outcome)),
            "receipt first: {receipt_first}; both committed: {receipt_outcome:?} {expiry_outcome:?}"
        );
        for (what, outcome) in [("receipt", &receipt_outcome), ("expiry", &expiry_outcome)] {
            assert!(
                committed(outcome) || matches!(outcome, Err(PgError::SerializationFailure)),
                "receipt first: {receipt_first}; the {what} commits or is aborted: {outcome:?}"
            );
        }
        if committed(&receipt_outcome) {
            let again = propose_against_pg(&pool, &program, &expiry("tn_a"))
                .await
                .unwrap();
            assert_eq!(refused_by(&again), Some("every_receipt_read"));
        } else {
            let again =
                propose_against_pg(&pool, &program, &receipt("tn_b", "2026-10-24T10:30:00Z"))
                    .await
                    .unwrap();
            assert_eq!(refused_by(&again), Some("record_open"));
        }
    }
}

/// The same schedule with an act that reads the report but not the
/// receipts, a later report: nothing links it back to the receipt, so both
/// commit, in either finishing order. So the schedule above aborts one act
/// because of the cycle, not because overlapping acts always conflict here.
#[tokio::test]
async fn an_act_that_does_not_read_the_receipts_commits_beside_the_receipt() {
    for receipt_first in [true, false] {
        let pool = pool().await;
        let program = programme();
        set_up(&pool, &program).await;
        let (receipt_outcome, later_outcome) = three_acts(
            &pool,
            &program,
            &completeness("2026-10-24T12:00:00Z"),
            receipt_first,
        )
        .await;
        assert!(
            committed(&receipt_outcome) && committed(&later_outcome),
            "receipt first: {receipt_first}: {receipt_outcome:?} {later_outcome:?}"
        );
    }
}
