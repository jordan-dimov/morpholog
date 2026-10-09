//! The mixed route beside the interpreter, on the production path: a
//! programme with an invariant outside the compiled fragment runs the
//! rest of its invariants in the database and that one in the kernel, in
//! programme order, inside one transaction. Each such gallery programme
//! is proposed through both from the same state; they must agree on
//! outcome, reason, refusing rule and version, witness variables,
//! rejection-log fields, and persisted rows, as the compiled route does
//! in `compiled_route`. Then the seams that harness cannot see: the order
//! between the two evaluators, whether a violation or an error, and the
//! batch that carries both parts across its acts.
//!
//! The proposition, on top of `compiled_route`'s: composing SQL and the
//! kernel in declaration order is kernel-equivalent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::routes::{RouteObservation, count, observe};
use common::{reset_db, test_pool};
use morpholog_core::{ClaimInstance, EvalValue, PreparedProgram, Program, Subject, Transition};
use morpholog_postgres::{
    InvariantPlan, PgAtomicOutcome, PgError, PgPool, PgProgram, PgProposalOutcome, Proposal,
    propose_against_pg, propose_all_against_pg,
};
use morpholog_test_support::differential::{sample_args, sample_state};
use morpholog_test_support::{dec_str as dec, subj, test_actor};

fn mixed_gallery() -> Vec<Program> {
    morpholog_examples::all_programs()
        .into_iter()
        .filter(|p| {
            let program = PgProgram::new(PreparedProgram::new(p.clone()).unwrap());
            matches!(program.plan(), InvariantPlan::Mixed { .. })
        })
        .collect()
}

#[tokio::test]
async fn the_gallery_still_has_mixed_programmes() {
    // The harness below proves nothing over an empty corpus.
    let names: Vec<String> = mixed_gallery().iter().map(|p| p.name.clone()).collect();
    assert!(names.len() >= 3, "mixed gallery: {names:?}");
}

#[tokio::test]
async fn the_mixed_route_reaches_the_kernels_decision_over_the_gallery() {
    let pool = test_pool().await;
    let (mut cases, mut commits, mut refusals, mut errors, mut skipped) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for program in mixed_gallery() {
        let mixed = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
        let interpreted = PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap());
        for t in &program.transformations {
            for salt in 0..2u64 {
                let Some(args) = sample_args(&program, t, salt) else {
                    skipped += 1;
                    continue;
                };
                let seeded: Vec<ClaimInstance> = sample_state(&program, 2, salt)
                    .claims()
                    .iter()
                    .cloned()
                    .collect();
                let transition = Transition {
                    transformation_name: t.name.clone(),
                    args,
                    actor: Subject::from("route_test"),
                };
                let spec = observe(&pool, &interpreted, &seeded, &transition).await;
                let real = observe(&pool, &mixed, &seeded, &transition).await;
                assert_eq!(
                    real, spec,
                    "programme `{}`, transformation `{}`, salt {salt}",
                    program.name, t.name
                );
                match &spec {
                    RouteObservation::Decided(o) if o.outcome.starts_with("committed") => {
                        commits += 1;
                    }
                    RouteObservation::Decided(_) => refusals += 1,
                    RouteObservation::Kernel(_) | RouteObservation::Operational(_) => errors += 1,
                }
                cases += 1;
            }
        }
    }
    assert!(
        cases >= 30,
        "generator collapse: only {cases} cases ran ({skipped} skipped)"
    );
    assert!(
        commits > 0 && refusals > 0,
        "both decisions must occur for the comparison to mean anything: \
         {commits} commits, {refusals} refusals, {errors} errors"
    );
}

// ------------------------------------------------------------
// The order between the two evaluators
// ------------------------------------------------------------

/// A programme with the interpreter's invariants and the database's
/// interleaved as `order` says: `c` compiles, `i` does not (arithmetic
/// keeps it out). `c_viol` and `i_viol` refuse an amount over ten;
/// `c_err` and `i_err` raise on a figure at the decimal ceiling, in the
/// database (a sum past the range) and in the kernel (an addition past
/// it). One act admits both figures, so every invariant is touched.
fn interleaved(order: &[&str]) -> Program {
    let mut text = String::from(
        "program precedence\n\
         predicate Amount(id: Subject, v: Decimal)\n\
         predicate Big(id: Subject, v: Decimal)\n",
    );
    for name in order {
        let body = match *name {
            "c_viol" => "forall a in Amount(id, v): v <= 10",
            "i_viol" => "forall a in Amount(id, v): v * 2 <= 20",
            "c_err" => "sum(v | Big(id, v)) <= 0",
            "i_err" => "forall b in Big(id, v): v + 1 <= v",
            other => panic!("{other}"),
        };
        text.push_str(&format!("invariant {name}: {body}\n"));
    }
    text.push_str(
        "transformation both(id, amount, big):\n\
         \x20   admit Amount(id, amount)\n\
         \x20   admit Big(id, big)\n",
    );
    morpholog_surface::parse_program(&text).unwrap_or_else(|e| panic!("{e:?}"))
}

const MAX: &str = "79228162514264337593543950335";

/// Decides a proposal on both routes from the same seeded state and
/// returns the two observations.
async fn both(
    pool: &PgPool,
    program: &Program,
    seeded: &[ClaimInstance],
    args: Vec<EvalValue>,
) -> (RouteObservation, RouteObservation) {
    let mixed = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    assert!(
        matches!(mixed.plan(), InvariantPlan::Mixed { .. }),
        "the fixture must run mixed: {:?}",
        mixed.plan()
    );
    let interpreted = PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap());
    let transition = Transition {
        transformation_name: "both".into(),
        args,
        actor: test_actor(),
    };
    (
        observe(pool, &interpreted, seeded, &transition).await,
        observe(pool, &mixed, seeded, &transition).await,
    )
}

fn big(id: &str, v: &str) -> ClaimInstance {
    ClaimInstance {
        predicate: "Big".into(),
        args: vec![subj(id), dec(v)],
    }
}

/// Two invariants violated by one act, one per evaluator, in both
/// declaration orders: the named rule is the kernel's, the earlier one.
#[tokio::test]
async fn the_first_violation_in_programme_order_is_named_whichever_evaluator_owns_it() {
    let pool = test_pool().await;
    for order in [["c_viol", "i_viol"], ["i_viol", "c_viol"]] {
        let program = interleaved(&order);
        let (spec, real) = both(&pool, &program, &[], vec![subj("a"), dec("11"), dec("1")]).await;
        assert_eq!(real, spec, "{order:?}");
        match &real {
            RouteObservation::Decided(o) => assert!(
                o.outcome.contains(&format!("rule Some(\"{}\")", order[0])),
                "{order:?}: {}",
                o.outcome
            ),
            other => panic!("{order:?}: {other:?}"),
        }
    }
}

/// An error in one evaluator and a violation in the other, in both
/// declaration orders: the earlier wins, error or rejection, as in the
/// kernel. The seeded ceiling figures make the whole-state sum raise;
/// the act's own ceiling figure makes the addition raise, since the
/// addition is checked over the act's case alone; the act's amount makes
/// the violations fire.
#[tokio::test]
async fn the_first_error_or_violation_in_programme_order_wins_across_evaluators() {
    let pool = test_pool().await;
    let seeded = vec![big("x", MAX), big("y", MAX)];
    let args = vec![subj("a"), dec("11"), dec(MAX)];
    for order in [
        ["c_err", "i_viol"],
        ["i_viol", "c_err"],
        ["i_err", "c_viol"],
        ["c_viol", "i_err"],
    ] {
        let program = interleaved(&order);
        let (spec, real) = both(&pool, &program, &seeded, args.clone()).await;
        assert_eq!(real, spec, "{order:?}");
        match (&real, order[0]) {
            (RouteObservation::Kernel(_), "c_err" | "i_err") => {}
            (RouteObservation::Decided(o), first @ ("c_viol" | "i_viol")) => assert!(
                o.outcome.contains(&format!("rule Some(\"{first}\")")),
                "{order:?}: {}",
                o.outcome
            ),
            other => panic!(
                "{order:?}: an error must win when it comes first, a violation when it does: {other:?}"
            ),
        }
    }
}

/// A kernel error in an interpreted run comes after the claim delta is
/// written, so the mixed route relies on the transaction: the error is
/// reported, and the claims, the audit, the outbox and the rejection log
/// are as they were.
#[tokio::test]
async fn a_kernel_error_after_the_write_leaves_nothing_behind() {
    let pool = test_pool().await;
    let program = interleaved(&["c_viol", "i_err"]);
    let mixed = PgProgram::new(PreparedProgram::new(program).unwrap());
    let seeded = vec![big("x", MAX)];
    reset_db(&pool).await;
    common::seed_claims(&pool, &seeded).await;
    let before = (
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        count(&pool, "SELECT count(*) FROM morpholog.outbox").await,
        count(&pool, "SELECT count(*) FROM morpholog.rejections").await,
    );
    assert_eq!(before, (1, 0, 0, 0));
    // Within the compiled bound; the act's own ceiling figure makes the
    // interpreted run raise, after the write.
    let transition = Transition {
        transformation_name: "both".into(),
        args: vec![subj("a"), dec("1"), dec(MAX)],
        actor: test_actor(),
    };
    let outcome = propose_against_pg(&pool, &mixed, &Proposal::gateway(&transition)).await;
    assert!(
        matches!(outcome, Err(PgError::Kernel(_))),
        "the interpreted run's error is the proposal's: {outcome:?}"
    );
    let after = (
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        count(&pool, "SELECT count(*) FROM morpholog.outbox").await,
        count(&pool, "SELECT count(*) FROM morpholog.rejections").await,
    );
    assert_eq!(after, before, "rolled back whole");
}

// ------------------------------------------------------------
// The batch carries both parts across its acts
// ------------------------------------------------------------

/// A batch on a mixed programme: act two depends on act one's admit
/// through a gate the interpreter runs, and is refused by a compiled
/// invariant in one batch and by an interpreted one in another, each
/// named at act two, as the kernel names them.
#[tokio::test]
async fn a_mixed_batch_carries_both_parts_across_acts() {
    let pool = test_pool().await;
    let text = "program carried\n\
                predicate Amount(id: Subject, v: Decimal)\n\
                predicate Seen(id: Subject)\n\
                invariant c_viol: forall a in Amount(id, v): v <= 10\n\
                invariant i_viol: forall a in Amount(id, v): v * 2 <= 18\n\
                transformation first(id, v):\n\
                \x20   admit Seen(id)\n\
                \x20   admit Amount(id, v)\n\
                transformation second(id, v):\n\
                \x20   require Seen(id)\n\
                \x20   admit Amount(id, v)\n";
    let program: Program = morpholog_surface::parse_program(text).unwrap();
    let mixed = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    assert!(matches!(mixed.plan(), InvariantPlan::Mixed { .. }));
    let interpreted = PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap());
    let act = |name: &str, v: &str| Transition {
        transformation_name: name.into(),
        args: vec![subj("a"), dec(v)],
        actor: test_actor(),
    };
    // 9.5 passes c_viol (<= 10) and fails i_viol (19 > 18); 11 fails
    // c_viol first.
    for (second, rule) in [("9.5", "i_viol"), ("11", "c_viol")] {
        let acts = [act("first", "1"), act("second", second)];
        let proposals: Vec<Proposal> = acts.iter().map(Proposal::gateway).collect();
        let mut outcomes = Vec::new();
        for program in [&interpreted, &mixed] {
            reset_db(&pool).await;
            let outcome = propose_all_against_pg(&pool, program, &proposals).await;
            outcomes.push(match outcome {
                Ok(PgAtomicOutcome::Rejected { act, rule, .. }) => {
                    format!("rejected act {act} by {rule:?}")
                }
                Ok(PgAtomicOutcome::Committed { acts }) => format!("committed {} acts", acts.len()),
                Err(PgError::Kernel(e)) => format!("kernel error {e:?}"),
                Err(e) => format!("operational {e:?}"),
            });
        }
        assert_eq!(outcomes[0], outcomes[1], "second = {second}");
        assert_eq!(outcomes[1], format!("rejected act 2 by Some(\"{rule}\")"));
    }
    // And the batch commits whole when both parts hold.
    let acts = [act("first", "1"), act("second", "2")];
    let proposals: Vec<Proposal> = acts.iter().map(Proposal::gateway).collect();
    reset_db(&pool).await;
    let outcome = propose_all_against_pg(&pool, &mixed, &proposals)
        .await
        .unwrap();
    assert!(
        matches!(outcome, PgAtomicOutcome::Committed { .. }),
        "{outcome:?}"
    );
    // The single proposal path agrees too.
    reset_db(&pool).await;
    let one = propose_against_pg(&pool, &mixed, &Proposal::gateway(&act("first", "11")))
        .await
        .unwrap();
    assert!(matches!(one, PgProposalOutcome::Rejected { .. }), "{one:?}");
}
