//! A rule that ranges over a grid, against PostgreSQL: the compiled route
//! reports the rule interpreted by a typed reason, and both routes decide
//! a full day and a day with an hour unpriced as the kernel does over the
//! whole record.

#![expect(
    clippy::unwrap_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::{date, dec, reset_db, seed_claims, test_pool};
use morpholog_core::{
    ClaimInstance, EvalValue, Outcome, PreparedProgram, Program, State, TracedProposal,
    Transformation,
};
use morpholog_postgres::{
    InvariantPlan, PgPool, PgProgram, PgProposalOutcome, PgTracedOutcome, list_claims,
};
use morpholog_test_support::differential::same_subjects;
use morpholog_test_support::ts;
use morpholog_test_support::{propose_with_trace, test_transition};

const PRICED: &str = "
program priced

predicate Day(day: Date, starts_at: Timestamp, ends_at: Timestamp)
    unique by (day)
predicate Published(day: Date)
    append only
predicate Price(day: Date, hour: Timestamp, price: Decimal)
    unique by (day, hour)

invariant a_published_day_is_priced:
    Published(day)
    implies Day(day, starts_at, ends_at)
    and ends_at - starts_at no_longer_than duration(PT25H)
    and (forall h in periods(starts_at, ends_at, duration(PT1H)): exists p: Price(day, h, p))

transformation publish(day):
    admit Published(day)
";

#[derive(Debug, PartialEq, Eq)]
struct Decision {
    verdict: String,
    asserted: Vec<String>,
}

fn claims(cs: &[ClaimInstance]) -> Vec<String> {
    let mut out: Vec<String> = cs.iter().map(|c| format!("{c:?}")).collect();
    out.sort();
    out
}

async fn by_the_kernel(
    pool: &PgPool,
    program: &Program,
    t: &Transformation,
    args: Vec<EvalValue>,
) -> Decision {
    let state = State::from_claims(list_claims(pool).await.unwrap());
    let transition = test_transition(t, args);
    match propose_with_trace(program, &transition, &state, &mut same_subjects()) {
        TracedProposal::Completed {
            outcome: Outcome::Accepted {
                asserted_claims, ..
            },
            ..
        } => Decision {
            verdict: "committed".into(),
            asserted: claims(&asserted_claims),
        },
        TracedProposal::Completed {
            outcome: Outcome::Rejected { reason },
            ..
        } => Decision {
            verdict: format!("rejected: {reason}"),
            asserted: vec![],
        },
        TracedProposal::Errored { error, .. } => panic!("kernel error: {error}"),
    }
}

async fn by_the_database(
    pool: &PgPool,
    pg: &PgProgram,
    t: &Transformation,
    args: Vec<EvalValue>,
) -> Decision {
    match common::propose_pg_with_trace_using_test_actor(pool, pg, t, args)
        .await
        .unwrap()
    {
        PgTracedOutcome::Outcome {
            outcome:
                PgProposalOutcome::Committed {
                    asserted_claims, ..
                },
            ..
        } => Decision {
            verdict: "committed".into(),
            asserted: claims(&asserted_claims),
        },
        PgTracedOutcome::Outcome {
            outcome: PgProposalOutcome::Rejected { reason, .. },
            ..
        } => Decision {
            verdict: format!("rejected: {reason}"),
            asserted: vec![],
        },
        PgTracedOutcome::KernelErrored { error, .. } => panic!("kernel error: {error}"),
    }
}

#[tokio::test]
async fn a_grid_rule_runs_interpreted_and_decides_as_the_kernel_does() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(PRICED).unwrap();
    let publish = program.transformation("publish").unwrap().clone();
    let compiled = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    // The rule is kept off the compiled route by a typed reason.
    match compiled.plan() {
        InvariantPlan::Mixed { refusals, .. } | InvariantPlan::Interpreted { refusals } => {
            assert!(
                refusals
                    .iter()
                    .any(|r| r.invariant.as_str() == "a_published_day_is_priced"),
                "{refusals:?}"
            );
        }
        InvariantPlan::Compiled => panic!("a grid rule does not compile"),
    }
    let day = |from: &str, until: &str| ClaimInstance {
        predicate: "Day".into(),
        args: vec![date("2026-06-01"), ts(from), ts(until)],
    };
    let price = |hour: &str| ClaimInstance {
        predicate: "Price".into(),
        args: vec![date("2026-06-01"), ts(hour), dec(5)],
    };
    for (route, pg) in [
        ("compiled", compiled),
        (
            "interpreted",
            PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap()),
        ),
    ] {
        for (case, claims, verdict) in [
            (
                "missing hour",
                vec![
                    day("2026-06-01T00:00:00Z", "2026-06-01T03:00:00Z"),
                    price("2026-06-01T00:00:00Z"),
                    price("2026-06-01T01:00:00Z"),
                ],
                "rejected",
            ),
            (
                "full day",
                vec![
                    day("2026-06-01T00:00:00Z", "2026-06-01T03:00:00Z"),
                    price("2026-06-01T00:00:00Z"),
                    price("2026-06-01T01:00:00Z"),
                    price("2026-06-01T02:00:00Z"),
                ],
                "committed",
            ),
        ] {
            reset_db(&pool).await;
            seed_claims(&pool, &claims).await;
            let expected = by_the_kernel(&pool, &program, &publish, vec![date("2026-06-01")]).await;
            assert!(
                expected.verdict.starts_with(verdict),
                "{route}/{case}: the kernel decides {expected:?}"
            );
            let got = by_the_database(&pool, &pg, &publish, vec![date("2026-06-01")]).await;
            assert_eq!(got, expected, "{route}/{case}");
        }
    }
}
