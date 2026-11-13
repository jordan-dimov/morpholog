//! A definition that names a figure against PostgreSQL: the compiled
//! route renders the body under the call's frame and the interpreted
//! route evaluates it, and both must decide as the kernel does over the
//! whole record: a gate within and beyond the figure, an empty sum taking
//! its seed, arithmetic over two calls, a rule's refusal with the
//! operands it compared, and a division by zero in an argument blamed
//! the same way on every route.

#![expect(
    clippy::unwrap_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::{dec, reset_db, seed_claims, subj, test_pool};
use morpholog_core::{
    ClaimInstance, EvalValue, Outcome, PreparedProgram, Program, State, TracedProposal,
    Transformation,
};
use morpholog_postgres::{
    InvariantPlan, PgPool, PgProgram, PgProposalOutcome, PgTracedOutcome, list_claims,
};
use morpholog_test_support::differential::same_subjects;
use morpholog_test_support::{propose_with_trace, test_transition};

const FIGURES: &str = "
program figures

predicate Stock(item: Subject, quantity: Decimal)
    unique by (item)
predicate Allocation(allocation: Subject, item: Subject, quantity: Decimal)
    unique by (allocation)

define allocated(of) = (sum(q | Allocation(item: of, quantity: q, ..)))

define headroom(of, held) = (held - allocated(of))

invariant allocations_fit:
    Stock(item, on_hand) implies headroom(item, on_hand) >= 0

transformation allocate(allocation, item, quantity):
    bind Stock(item, on_hand)
    require fits: quantity <= headroom(item, on_hand)
    admit Allocation(allocation, item, quantity)

transformation allocate_both(allocation, item, other, quantity):
    bind Stock(item, on_hand)
    bind Stock(other, on_hand_other)
    require fits: quantity <= headroom(item, on_hand) + headroom(other, on_hand_other)
    admit Allocation(allocation, item, quantity)

transformation force(allocation, item, quantity):
    admit Allocation(allocation, item, quantity)

transformation share(allocation, item, quantity, parts):
    bind Stock(item, on_hand)
    require fits: quantity <= headroom(item, on_hand / parts)
    admit Allocation(allocation, item, quantity)
";

/// What a decision looks like, with no fresh subjects to name: the
/// verdict, what was admitted, the operands a refusal compared, or the
/// error.
#[derive(Debug, PartialEq, Eq)]
struct Decision {
    verdict: String,
    asserted: Vec<String>,
    compared: String,
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
            compared: String::new(),
        },
        TracedProposal::Completed {
            outcome: Outcome::Rejected { reason },
            ..
        } => {
            let (rule, compared) = match &reason {
                morpholog_core::RejectionReason::Invariant { name, compared, .. } => {
                    (name.to_string(), format!("{compared:?}"))
                }
                morpholog_core::RejectionReason::Require { name, compared, .. }
                | morpholog_core::RejectionReason::BindNone { name, compared, .. } => {
                    (name.as_ref().unwrap().to_string(), format!("{compared:?}"))
                }
            };
            Decision {
                verdict: format!("rejected by {rule}"),
                asserted: vec![],
                compared,
            }
        }
        TracedProposal::Errored { error, .. } => Decision {
            verdict: format!("error: {error}"),
            asserted: vec![],
            compared: String::new(),
        },
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
            compared: String::new(),
        },
        PgTracedOutcome::Outcome {
            outcome: PgProposalOutcome::Rejected { rule, compared, .. },
            ..
        } => Decision {
            verdict: format!("rejected by {}", rule.unwrap()),
            asserted: vec![],
            compared: format!("{compared:?}"),
        },
        PgTracedOutcome::KernelErrored { error, .. } => Decision {
            verdict: format!("error: {error}"),
            asserted: vec![],
            compared: String::new(),
        },
    }
}

#[tokio::test]
async fn a_figure_decides_as_the_kernel_does_on_both_routes() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(FIGURES).unwrap();
    let act = |name: &str| program.transformation(name).unwrap().clone();
    let (allocate, allocate_both, force, share) = (
        act("allocate"),
        act("allocate_both"),
        act("force"),
        act("share"),
    );
    let stock = |item: &str, q: i64| ClaimInstance {
        predicate: "Stock".into(),
        args: vec![subj(item), dec(q)],
    };
    let allocation = |a: &str, item: &str, q: i64| ClaimInstance {
        predicate: "Allocation".into(),
        args: vec![subj(a), subj(item), dec(q)],
    };
    // The rule over the figure compiles: a body the compiler could not
    // render would run interpreted and agree with the kernel for free.
    let compiled = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    assert!(
        matches!(compiled.plan(), InvariantPlan::Compiled),
        "the figure's rule compiles: {:?}",
        compiled.plan()
    );
    for (route, pg) in [
        ("compiled", compiled),
        (
            "interpreted",
            PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap()),
        ),
    ] {
        reset_db(&pool).await;
        seed_claims(
            &pool,
            &[
                stock("pallet", 10),
                stock("crate", 5),
                allocation("a1", "pallet", 4),
            ],
        )
        .await;
        // Four of ten on the pallet, nothing on the crate. Each case
        // leaves the record for the next.
        let cases: Vec<(&str, &Transformation, Vec<EvalValue>, &str)> = vec![
            // Within the figure: two more on the pallet.
            (
                "within",
                &allocate,
                vec![subj("a2"), subj("pallet"), dec(2)],
                "committed",
            ),
            // Arithmetic over two calls: the gate passes only through the
            // crate's headroom, then the rule refuses the pallet.
            (
                "two calls, gate passes by the second call",
                &allocate_both,
                vec![subj("a3"), subj("pallet"), subj("crate"), dec(5)],
                "rejected by allocations_fit",
            ),
            (
                "two calls within",
                &allocate_both,
                vec![subj("a3"), subj("pallet"), subj("crate"), dec(4)],
                "committed",
            ),
            (
                "two calls beyond",
                &allocate_both,
                vec![subj("a4"), subj("pallet"), subj("crate"), dec(6)],
                "rejected by fits",
            ),
            // An empty sum takes its seed: nothing is allocated on the crate.
            (
                "empty sum",
                &allocate,
                vec![subj("c1"), subj("crate"), dec(5)],
                "committed",
            ),
            // Beyond the figure, at the gate and at the rule, with the
            // operands each compared.
            (
                "beyond",
                &allocate,
                vec![subj("a4"), subj("pallet"), dec(1)],
                "rejected by fits",
            ),
            (
                "rule",
                &force,
                vec![subj("a4"), subj("pallet"), dec(1)],
                "rejected by allocations_fit",
            ),
            // The rule holds for the crate, whatever the pallet holds: the
            // figure is the crate's own, not every allocation's.
            (
                "rule holds for the other item",
                &force,
                vec![subj("c2"), subj("crate"), dec(0)],
                "committed",
            ),
            // A division by zero in an argument: the error, blamed the same way.
            (
                "division by zero",
                &share,
                vec![subj("a4"), subj("pallet"), dec(1), dec(0)],
                "error: ",
            ),
        ];
        for (case, t, args, verdict) in cases {
            let expected = by_the_kernel(&pool, &program, t, args.clone()).await;
            assert!(
                expected.verdict.starts_with(verdict),
                "{route}/{case}: the kernel decides {expected:?}"
            );
            let got = by_the_database(&pool, &pg, t, args).await;
            assert_eq!(got, expected, "{route}/{case}");
        }
    }
}
