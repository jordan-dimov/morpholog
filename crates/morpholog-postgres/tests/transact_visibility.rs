//! A batch loads once, keyed by every act's own arguments, and later acts
//! see the earlier acts' effects through the kernel's candidate state,
//! never a reread. Both directions, on both routes: a claim admitted by
//! the first act satisfies the second's gate although no row existed at
//! the load; a claim retracted by the first act no longer blocks the
//! second's negative gate although its row was loaded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{reset_db, seed_claims, test_pool};
use morpholog_core::{CompiledProgram, Subject, Transition};
use morpholog_postgres::{PgAtomicOutcome, PgProgram, Proposal, propose_all_against_pg};
use morpholog_test_support::{claim_instance, subj};

fn program() -> morpholog_core::Program {
    morpholog_surface::parse_program(
        "program visibility
predicate P(k: Subject)
predicate Seen(k: Subject)
transformation admit_p(k):
    admit P(k)
transformation retract_p(k):
    retract P(k)
transformation need_p(k):
    require P(k)
    admit Seen(k)
transformation need_not_p(k):
    require not P(k)
    admit Seen(k)
",
    )
    .unwrap()
}

fn act(name: &str, k: &str) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: name.into(),
        args: vec![subj(k)],
        actor: Subject::from("visibility_test"),
    })
}

fn routes() -> [PgProgram; 2] {
    [
        PgProgram::interpreted(CompiledProgram::new(program()).unwrap()),
        PgProgram::new(CompiledProgram::new(program()).unwrap()),
    ]
}

#[tokio::test]
async fn a_claim_the_first_act_admits_satisfies_the_second_acts_gate() {
    let pool = test_pool().await;
    for route in routes() {
        reset_db(&pool).await;
        let outcome =
            propose_all_against_pg(&pool, &route, &[act("admit_p", "k"), act("need_p", "k")])
                .await
                .unwrap();
        assert!(
            matches!(outcome, PgAtomicOutcome::Committed { .. }),
            "{outcome:?}"
        );
    }
}

#[tokio::test]
async fn a_claim_the_first_act_retracts_no_longer_blocks_the_second_acts_gate() {
    let pool = test_pool().await;
    for route in routes() {
        reset_db(&pool).await;
        seed_claims(&pool, &[claim_instance("P", &[subj("k")])]).await;
        let outcome = propose_all_against_pg(
            &pool,
            &route,
            &[act("retract_p", "k"), act("need_not_p", "k")],
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, PgAtomicOutcome::Committed { .. }),
            "{outcome:?}"
        );
    }
}
