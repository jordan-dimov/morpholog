//! Statements that repeat within one act, on every route. Attacker
//! capability: a caller who chooses a proposal's arguments, including the
//! elements of a collection, under a programme it cannot change. Repeating
//! an admission must not hide the change the first one made, so each
//! route must reach the interpreter's decision and leave the same claims.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{reset_db, seed_claims, test_pool};
use morpholog_core::{ClaimInstance, EvalValue, PreparedProgram, Subject, Transition};
use morpholog_postgres::{
    InvariantPlan, PgAtomicOutcome, PgPool, PgProgram, PgProposalOutcome, Proposal,
    propose_against_pg, propose_all_against_pg,
};
use morpholog_test_support::{coll, dec, subj};

const PROGRAMME: &str = "program repeated_statements
predicate P(k: Subject, n: Decimal)
predicate Q(k: Subject)
invariant bounded:
    P(k, n) implies n <= 10
transformation once(k, n):
    admit P(k, n)
transformation twice(k, n):
    admit P(k, n)
    admit P(k, n)
transformation each(ks, n):
    for k in ks:
        admit P(k, n)
transformation retract_twice(k, n):
    retract P(k, n)
    retract P(k, n)
transformation retract_then_admit(k, n):
    retract P(k, n)
    admit P(k, n)
transformation retract_then_admit_twice(k, n):
    retract P(k, n)
    admit P(k, n)
    admit P(k, n)
";

/// A rule outside the compiled fragment, so the programme runs mixed.
const OUTSIDE_THE_FRAGMENT: &str = "invariant no_new_q:
    Q(k) implies pre(Q(k))
";

fn routes() -> Vec<(&'static str, PgProgram)> {
    let prepared = |source: &str| {
        PreparedProgram::new(morpholog_surface::parse_program(source).unwrap()).unwrap()
    };
    let mixed_source = format!("{PROGRAMME}{OUTSIDE_THE_FRAGMENT}");
    let routes = vec![
        ("interpreted", PgProgram::interpreted(prepared(PROGRAMME))),
        ("compiled", PgProgram::new(prepared(PROGRAMME))),
        ("mixed", PgProgram::new(prepared(&mixed_source))),
    ];
    for (name, pg) in &routes {
        let plan = match pg.plan() {
            InvariantPlan::Interpreted { .. } => "interpreted",
            InvariantPlan::Compiled { .. } => "compiled",
            InvariantPlan::Mixed { .. } => "mixed",
        };
        assert_eq!(plan, *name, "the route under test");
    }
    routes
}

fn p(k: &str, n: i64) -> ClaimInstance {
    ClaimInstance {
        predicate: "P".into(),
        args: vec![subj(k), dec(n)],
    }
}

fn act(name: &str, args: Vec<EvalValue>) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: name.into(),
        args,
        actor: Subject::from("w"),
    })
}

async fn claims(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT predicate_name || arguments::text FROM morpholog.claims ORDER BY 1")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// What one route decided, and the claims it left.
async fn single(
    pool: &PgPool,
    pg: &PgProgram,
    seed: &[ClaimInstance],
    proposal: &Proposal,
) -> (String, Vec<String>) {
    reset_db(pool).await;
    seed_claims(pool, seed).await;
    let decided = match propose_against_pg(pool, pg, proposal).await.unwrap() {
        PgProposalOutcome::Committed { .. } => "committed".to_string(),
        PgProposalOutcome::Rejected { reason, .. } => format!("rejected: {reason}"),
    };
    (decided, claims(pool).await)
}

async fn batch(pool: &PgPool, pg: &PgProgram, acts: &[Proposal]) -> (String, Vec<String>) {
    reset_db(pool).await;
    let decided = match propose_all_against_pg(pool, pg, acts).await.unwrap() {
        PgAtomicOutcome::Committed { .. } => "committed".to_string(),
        PgAtomicOutcome::Rejected { act, reason, .. } => format!("act {act} rejected: {reason}"),
    };
    (decided, claims(pool).await)
}

#[tokio::test]
async fn every_route_reaches_the_interpreters_decision_on_repeated_statements() {
    let pool = test_pool().await;
    let n = |k: &str, v: i64| vec![subj(k), dec(v)];
    let cases: Vec<(&str, Vec<ClaimInstance>, Proposal)> = vec![
        (
            "admitted twice, violating",
            vec![],
            act("twice", n("d", 20)),
        ),
        ("admitted twice, lawful", vec![], act("twice", n("d", 5))),
        (
            "admitted twice, already present",
            vec![p("d", 20)],
            act("twice", n("d", 20)),
        ),
        (
            "a collection repeating an element",
            vec![],
            act("each", vec![coll(vec![subj("d"), subj("d")]), dec(20)]),
        ),
        (
            "retracted twice",
            vec![p("d", 5)],
            act("retract_twice", n("d", 5)),
        ),
        (
            "retracted twice, absent",
            vec![],
            act("retract_twice", n("d", 5)),
        ),
        (
            "retracted then admitted, absent",
            vec![],
            act("retract_then_admit", n("d", 20)),
        ),
        (
            "retracted then admitted, present",
            vec![p("d", 20)],
            act("retract_then_admit", n("d", 20)),
        ),
        (
            "retracted then admitted twice, absent",
            vec![],
            act("retract_then_admit_twice", n("d", 20)),
        ),
    ];
    let routes = routes();
    let (_, interpreter) = &routes[0];
    for (label, seed, proposal) in &cases {
        let expected = single(&pool, interpreter, seed, proposal).await;
        for (name, pg) in &routes[1..] {
            assert_eq!(
                single(&pool, pg, seed, proposal).await,
                expected,
                "{name}: {label}"
            );
        }
    }
    // The interpreter refuses every violating case outright.
    for label in [0usize, 3, 8] {
        let (_, seed, proposal) = &cases[label];
        let (decided, left) = single(&pool, interpreter, seed, proposal).await;
        assert!(
            decided.starts_with("rejected"),
            "{}: {decided}",
            cases[label].0
        );
        assert!(left.is_empty());
    }

    // A repeat inside one act of a batch is that act's own; a repeat
    // across acts is not, since the later act's state holds the earlier
    // one's claim.
    for acts in [
        vec![act("once", n("a", 1)), act("twice", n("d", 20))],
        vec![act("once", n("a", 1)), act("once", n("a", 1))],
    ] {
        let expected = batch(&pool, interpreter, &acts).await;
        for (name, pg) in &routes[1..] {
            assert_eq!(batch(&pool, pg, &acts).await, expected, "{name}: batch");
        }
    }
}
