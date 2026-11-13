//! A batch loads once, keyed by every act's own arguments, and later acts
//! see the earlier acts' effects through the kernel's candidate state,
//! never a reread. Both directions, on both routes: a claim admitted by
//! the first act satisfies the second's gate although no row existed at
//! the load; a claim retracted by the first act no longer blocks the
//! second's negative gate although its row was loaded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{reset_db, seed_claims, test_pool};
use morpholog_core::{PreparedProgram, Subject, Transition};
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
        decided_at: None,
    })
}

fn routes() -> [PgProgram; 2] {
    [
        PgProgram::interpreted(PreparedProgram::new(program()).unwrap()),
        PgProgram::new(PreparedProgram::new(program()).unwrap()),
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

fn tree() -> morpholog_core::Program {
    morpholog_surface::parse_program(
        "program variant_tree
predicate VariantOf(part: Subject, parent: Subject)
    unique by (part)
define descends_from(part, ancestor):
    VariantOf(part, ancestor)
    or (VariantOf(part, between) and descends_from(between, ancestor))
transformation set_parent(part, parent):
    require not_its_own_parent: part != parent
    require no_loop: not descends_from(parent, part)
    retract VariantOf(part, _)
    admit VariantOf(part, parent)
",
    )
    .unwrap()
}

fn set_parent(part: &str, parent: &str) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: "set_parent".into(),
        args: vec![subj(part), subj(parent)],
        actor: Subject::from("visibility_test"),
        decided_at: None,
    })
}

/// Two steps that are each safe against the record as loaded, and close
/// a loop together. The gate that keeps the tree free of loops follows
/// the tree through a condition that calls itself, and must walk the
/// step the first act staged.
#[tokio::test]
async fn a_loop_two_acts_would_close_together_is_refused_at_the_second() {
    let pool = test_pool().await;
    for route in [
        PgProgram::interpreted(PreparedProgram::new(tree()).unwrap()),
        PgProgram::new(PreparedProgram::new(tree()).unwrap()),
    ] {
        reset_db(&pool).await;
        let outcome =
            propose_all_against_pg(&pool, &route, &[set_parent("b", "a"), set_parent("a", "b")])
                .await
                .unwrap();
        assert!(
            matches!(
                &outcome,
                PgAtomicOutcome::Rejected { act: 2, rule: Some(rule), .. } if rule == "no_loop"
            ),
            "{outcome:?}"
        );

        // A chain built in one batch, each step reading the one before.
        let outcome = propose_all_against_pg(
            &pool,
            &route,
            &[
                set_parent("b", "a"),
                set_parent("c", "b"),
                set_parent("d", "c"),
            ],
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, PgAtomicOutcome::Committed { .. }),
            "{outcome:?}"
        );
        // The loop three steps round, against the record now stored, and
        // closed by a batch whose first act looks harmless.
        let outcome =
            propose_all_against_pg(&pool, &route, &[set_parent("e", "d"), set_parent("a", "e")])
                .await
                .unwrap();
        assert!(
            matches!(
                &outcome,
                PgAtomicOutcome::Rejected { act: 2, rule: Some(rule), .. } if rule == "no_loop"
            ),
            "{outcome:?}"
        );
    }
}
