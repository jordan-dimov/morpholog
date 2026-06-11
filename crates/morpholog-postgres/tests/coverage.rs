//! `coverage_replay` against a real audit log: the ledger history is
//! committed through the normal propose path, then coverage replays
//! it and reports which rules ever fired, which never did, and which
//! transformations were ever used.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::CoverageVerdict;
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{PgPool, PgProposalOutcome, coverage_replay};
use uuid::Uuid;

mod common;
use common::{dec, subj};

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

async fn post_entry(pool: &PgPool, entry: &str, amount: i64) -> Uuid {
    expect_committed(
        common::propose_pg_with_test_actor(
            pool,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj(entry),
                subj("d_2026_06_01"),
                subj("p_coverage"),
                subj("account_cash"),
                subj("account_revenue"),
                dec(amount),
            ],
            &double_entry_ledger::all_invariants(),
            &double_entry_ledger::definitions(),
        )
        .await
        .unwrap(),
    )
}

#[tokio::test]
async fn coverage_reports_fired_never_fired_and_usage_over_real_history() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let program = double_entry_ledger::program();

    // An empty log first: everything implication-shaped is never-fired,
    // nothing has been used, zero transitions.
    let report = coverage_replay(&pool, &program).await.unwrap();
    assert_eq!(report.transitions_replayed, 0);
    assert!(
        report
            .invariants
            .iter()
            .all(|i| i.verdict != CoverageVerdict::Fired),
        "an empty log cannot have fired anything: {report:?}"
    );
    assert!(report.transformations.iter().all(|t| t.transitions == 0));

    // Now a real history: two posted entries through the normal commit
    // path - each carrying JournalEntry + JournalLine claims, so the
    // balance and lines invariants both fire.
    let tid1 = post_entry(&pool, "entry_001", 100).await;
    let tid2 = post_entry(&pool, "entry_002", 200).await;

    let report = coverage_replay(&pool, &program).await.unwrap();
    assert_eq!(report.transitions_replayed, 2);

    let coverage = |name: &str| {
        report
            .invariants
            .iter()
            .find(|i| i.invariant == name)
            .unwrap_or_else(|| panic!("{name} missing from report"))
    };
    let balanced = coverage("balanced_posted_entry");
    assert_eq!(balanced.verdict, CoverageVerdict::Fired);
    assert_eq!(balanced.transitions_fired, 2);
    assert_eq!(
        balanced.first_fired.as_deref(),
        Some(tid1.to_string().as_str())
    );
    assert_eq!(
        balanced.last_fired.as_deref(),
        Some(tid2.to_string().as_str())
    );

    // The period was never closed, so any invariant whose antecedent
    // needs a PeriodClosed claim stayed silent - and is reported so,
    // not hidden. (Identify them structurally rather than by pinned
    // name: every implication invariant that did not fire.)
    let never_fired: Vec<&str> = report
        .invariants
        .iter()
        .filter(|i| i.verdict == CoverageVerdict::NeverFired)
        .map(|i| i.invariant.as_str())
        .collect();
    assert!(
        !never_fired.is_empty(),
        "a two-transition history cannot have exercised every rule; got none never-fired"
    );

    // Usage: the posting transformation carries both transitions; the
    // close/restate transformations were declared but never used.
    let usage = |name: &str| {
        report
            .transformations
            .iter()
            .find(|t| t.transformation == name)
            .unwrap_or_else(|| panic!("{name} missing from usage"))
    };
    assert_eq!(usage("post_simple_entry").transitions, 2);
    assert_eq!(
        usage("post_simple_entry").first.as_deref(),
        Some(tid1.to_string().as_str())
    );
    assert_eq!(usage("close_period").transitions, 0);
    assert!(!usage("close_period").not_in_programme);
}

// Closing the period makes the close-gate invariants fire too, and the
// counts stay per-transition (the balance rule does not re-count on a
// transition whose delta does not touch its antecedent).
#[tokio::test]
async fn firing_is_counted_per_relevant_transition() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let program = double_entry_ledger::program();
    post_entry(&pool, "entry_001", 100).await;
    let close_tid = expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            &double_entry_ledger::close_period(),
            vec![subj("p_coverage")],
            &double_entry_ledger::all_invariants(),
            &double_entry_ledger::definitions(),
        )
        .await
        .unwrap(),
    );

    let report = coverage_replay(&pool, &program).await.unwrap();
    assert_eq!(report.transitions_replayed, 2);
    let balanced = report
        .invariants
        .iter()
        .find(|i| i.invariant == "balanced_posted_entry")
        .unwrap();
    // The close transition asserts only PeriodClosed - outside the
    // balance rule's antecedent footprint - so the count stays 1.
    assert_eq!(balanced.transitions_fired, 1);
    assert_ne!(
        balanced.last_fired.as_deref(),
        Some(close_tid.to_string().as_str()),
        "the close transition must not re-count the balance rule"
    );
}

// The headline payoff of the rejection log: an always-on prohibition
// whose enforcement work was structurally invisible in committed
// history becomes measurable the moment it refuses a real proposal.
#[tokio::test]
async fn an_always_on_prohibition_that_refuses_shows_constrained() {
    use morpholog_surface::parse_program;

    let pool = test_pool().await;
    reset_db(&pool).await;

    let source = r#"
program custody

predicate Held(credit_id: Subject, holder_id: Subject)
predicate Retired(credit_id: Subject)

invariant retired_is_never_held:
    not (Retired(c) and Held(c, h))

transformation hold(credit_id, holder_id):
    admit Held(credit_id, holder_id)

transformation retire(credit_id):
    admit Retired(credit_id)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");

    // Sanity: with no history at all, the prohibition is always-on.
    let report = coverage_replay(&pool, &program).await.unwrap();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "retired_is_never_held")
        .unwrap();
    assert_eq!(inv.verdict, CoverageVerdict::AlwaysOn);

    // A held credit commits; retiring it while held is refused.
    expect_committed(
        common::propose_pg_with_test_actor(
            &pool,
            program.transformation("hold").unwrap(),
            vec![subj("c1"), subj("h1")],
            &program.invariants,
            &program.definitions,
        )
        .await
        .unwrap(),
    );
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        program.transformation("retire").unwrap(),
        vec![subj("c1")],
        &program.invariants,
        &program.definitions,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));

    let report = coverage_replay(&pool, &program).await.unwrap();
    assert_eq!(report.transitions_replayed, 1);
    assert_eq!(report.rejections_replayed, 1);
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "retired_is_never_held")
        .unwrap();
    assert_eq!(
        inv.verdict,
        CoverageVerdict::Constrained,
        "the prohibition's refusal is now visible: {report:?}"
    );
    assert_eq!(inv.proposals_refused, 1);
    assert!(inv.first_refused.is_some());

    let retire = report
        .transformations
        .iter()
        .find(|t| t.transformation == "retire")
        .unwrap();
    assert_eq!(retire.transitions, 0);
    assert_eq!(retire.proposals_refused, 1);
    assert!(!retire.not_in_programme);
}
