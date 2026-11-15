//! What `evaluate`'s zero means. Over a history `audit verify` finds
//! consistent (the replay is the table), a state invariant that
//! `initially_holds` and that no commit was charged with holds on the final
//! state. The scorer judges each commit only over the cases its effective
//! change can touch; this holds the composite to the whole-state answer.
//!
//! Every gallery programme is its own candidate, over histories generated
//! from its own predicates and replayed as the scorer's driver replays the
//! log: the effective change read against the state before the row, then
//! the row applied. A history whose scoring raises is skipped, as
//! `evaluate` gives no report then; the skips are bounded. Once a history
//! is scored, every invariant must evaluate on its final state: a raise
//! there fails the test, so no case leaves the comparison unseen.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::{
    CandidateScorer, ClaimInstance, EvalValue, Program, State, invariants_using_pre,
};
use morpholog_test_support::differential::sample_value;
use morpholog_test_support::eval_invariant;

const HISTORIES: u64 = 40;
const ROWS: usize = 14;
const POOL: u64 = 4;

/// A deterministic stream of numbers from one salt.
struct Draws(u64);

impl Draws {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from((self.0 >> 33) % below.max(1) as u64).expect("small")
    }
}

/// Up to `POOL` claims of every declared predicate, the values the claims
/// of a history are drawn from.
fn pool(program: &Program, salt: u64) -> Vec<ClaimInstance> {
    let mut claims = Vec::new();
    for (p, decl) in program.predicates.iter().enumerate() {
        for w in 0..POOL {
            let args: Option<Vec<EvalValue>> = decl
                .args
                .iter()
                .enumerate()
                .map(|(a, arg)| {
                    sample_value(
                        &arg.kind,
                        salt.wrapping_mul(17)
                            .wrapping_add(p as u64 * 11)
                            .wrapping_add(w * 5)
                            .wrapping_add(a as u64),
                    )
                })
                .collect();
            if let Some(args) = args {
                claims.push(ClaimInstance {
                    predicate: decl.name.clone(),
                    args,
                });
            }
        }
    }
    claims
}

#[derive(Default)]
struct Tally {
    histories: u64,
    skipped: u64,
    zero_scores_checked: u64,
    violations_charged: u64,
}

/// One history: replay it into the scorer, then hold every uncharged,
/// initially holding invariant to the final state.
fn check_history(program: &Program, salt: u64, tally: &mut Tally) {
    let validated = morpholog_test_support::validated(program);
    let mut scorer = CandidateScorer::new(&validated).expect("no pre(...) candidates here");
    let pool = pool(program, salt);
    if pool.is_empty() {
        return;
    }
    let mut draws = Draws(salt);
    let mut state = State::from_claims(Vec::new());
    for row in 0..ROWS {
        let asserted: Vec<ClaimInstance> = (0..1 + draws.next(3))
            .map(|_| pool[draws.next(pool.len())].clone())
            .collect();
        let held: Vec<ClaimInstance> = state.claims().to_vec();
        let retracted: Vec<ClaimInstance> = if held.is_empty() {
            Vec::new()
        } else {
            (0..draws.next(3))
                .map(|_| held[draws.next(held.len())].clone())
                .collect()
        };
        let effective = morpholog_core::effective_delta(&state, &asserted, &retracted);
        state.apply_replayed(&asserted, &retracted);
        if scorer
            .observe_transition(&state, &effective, &format!("t{row}"))
            .is_err()
        {
            tally.skipped += 1;
            return;
        }
    }
    tally.histories += 1;
    let score = scorer.into_report();
    for (inv, scored) in program.invariants.iter().zip(&score.invariants) {
        let holds = eval_invariant(program, inv, &state, None).unwrap_or_else(|e| {
            panic!(
                "{}: `{}` raises on the final state of a history it was scored over (salt {salt}): {e:?}",
                program.name, inv.name
            )
        });
        if scored.initially_holds && scored.would_refuse == 0 {
            tally.zero_scores_checked += 1;
            assert!(
                holds,
                "{}: `{}` scored zero from a lawful start but fails on the final state (salt {salt})",
                program.name, inv.name
            );
        } else if !holds && scored.would_refuse > 0 {
            tally.violations_charged += 1;
        }
    }
}

#[test]
fn a_zero_score_from_a_lawful_start_means_the_final_state_holds() {
    let mut tally = Tally::default();
    for program in morpholog_examples::all_programs() {
        if !invariants_using_pre(&program).is_empty() || program.invariants.is_empty() {
            continue;
        }
        for salt in 0..HISTORIES {
            check_history(&program, salt, &mut tally);
        }
    }
    eprintln!(
        "histories {} skipped {} zero scores checked {} violations charged {}",
        tally.histories, tally.skipped, tally.zero_scores_checked, tally.violations_charged
    );
    assert!(
        tally.skipped * 4 <= tally.histories,
        "too many histories skipped to say anything: {} of {}",
        tally.skipped,
        tally.histories + tally.skipped
    );
    assert!(
        tally.zero_scores_checked > 100,
        "anti-vacuity: only {} zero scores were held to the final state",
        tally.zero_scores_checked
    );
    assert!(
        tally.violations_charged > 100,
        "anti-vacuity: the histories made only {} charged violations, so they barely test the scorer",
        tally.violations_charged
    );
}
