//! `propose` and `propose_with_trace` must answer every gallery case the same:
//! same outcome, rejection reason, and kernel error. They share one executor,
//! but `Stmt::For` branches on `trace.is_on()` to keep per-iteration
//! allocations off the untraced path, so the paths really differ.
//!
//! Both runs take the same subjects for `new Subject()`, so they are compared
//! exactly: tracing may change what observation costs, never the outcome,
//! the error, or which subjects execution draws.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use morpholog_core::{Stmt, TraceEntry, TracedProposal, propose, propose_with_trace};
use morpholog_test_support::differential::{same_subjects, sample_args, sample_state};
use morpholog_test_support::test_transition;

/// Whether the transformation loops. `for` is the only statement
/// with a nested body, so any nested loop's outermost ancestor is
/// itself a top-level `for` - a flat scan is total.
fn contains_for(body: &[Stmt]) -> bool {
    body.iter().any(|s| matches!(s, Stmt::For { .. }))
}

/// Count `For` trace entries, descending into iteration traces so a
/// nested loop still registers.
fn count_for_entries(entries: &[TraceEntry]) -> usize {
    entries
        .iter()
        .map(|e| match e {
            TraceEntry::For { iterations, .. } => {
                1 + iterations
                    .iter()
                    .map(|i| count_for_entries(&i.trace))
                    .sum::<usize>()
            }
            _ => 0,
        })
        .sum()
}

#[test]
fn traced_and_untraced_execution_are_equivalent() {
    let mut cases = 0usize;
    let mut skipped = 0usize;
    let mut for_entries_seen = 0usize;
    let mut for_transformations_skipped: Vec<String> = Vec::new();
    for program in morpholog_examples::all_programs() {
        for t in &program.transformations {
            let mut ran_any = false;
            for salt in 0..3u64 {
                let Some(args) = sample_args(&program, t, salt) else {
                    skipped += 1;
                    continue;
                };
                ran_any = true;
                let state = sample_state(&program, 2, salt);

                let transition = test_transition(t, args);
                let untraced = propose(
                    t,
                    &transition,
                    &state,
                    &program.invariants,
                    &program.definitions,
                    &mut same_subjects(),
                );
                let traced = propose_with_trace(
                    t,
                    &transition,
                    &state,
                    &program.invariants,
                    &program.definitions,
                    &mut same_subjects(),
                );
                let traced_as_result = match traced {
                    TracedProposal::Completed { outcome, trace } => {
                        for_entries_seen += count_for_entries(&trace);
                        Ok(outcome)
                    }
                    TracedProposal::Errored { error, trace } => {
                        for_entries_seen += count_for_entries(&trace);
                        Err(error)
                    }
                };
                // Exactly, candidate state included: both runs read the
                // same full pre-state and draw the same subjects.
                assert_eq!(
                    untraced, traced_as_result,
                    "programme `{}`, transformation `{}`, salt {salt}: \
                     trace mode changed the outcome",
                    program.name, t.name
                );
                cases += 1;
            }
            if contains_for(&t.body) && !ran_any {
                for_transformations_skipped.push(format!("{}::{}", program.name, t.name));
            }
        }
    }
    // No silent caps: a generator regression that skips most of the
    // corpus must fail here, not quietly shrink coverage.
    assert!(
        cases >= 100,
        "generator collapse: only {cases} cases ran ({skipped} skipped)"
    );
    // The traced/untraced split lives in `Stmt::For`, and a total-case floor
    // cannot see loops drop out of coverage. So every looping transformation
    // must produce a case, and at least one run must enter a loop.
    assert!(
        for_transformations_skipped.is_empty(),
        "loop-bearing transformations produced no cases: \
         {for_transformations_skipped:?}"
    );
    assert!(
        for_entries_seen > 0,
        "no generated case executed a `for` body - the differential's \
         principal seam is uncovered"
    );
}

fn minting_programme() -> (morpholog_core::Transformation, morpholog_core::Program) {
    use morpholog_core::ir_builder::{
        assert_, let_new_subject, params, predicate, program, transformation, var,
    };
    let t = transformation(
        "mint",
        params(&[]),
        vec![let_new_subject("x"), assert_("Minted", vec![var("x")])],
    );
    let p = program("fresh")
        .predicates(vec![predicate("Minted").subject("x").build()])
        .transformations(vec![t.clone()])
        .build();
    (t, p)
}

/// The same subjects give the same execution: outcome and trace alike, with
/// nothing renamed away.
#[test]
fn the_same_subjects_give_the_same_execution_trace_included() {
    let (t, p) = minting_programme();
    let transition = test_transition(&t, vec![]);
    let run = || {
        format!(
            "{:?}",
            propose_with_trace(
                &t,
                &transition,
                &morpholog_core::State::default(),
                &p.invariants,
                &p.definitions,
                &mut same_subjects(),
            )
        )
    };
    assert_eq!(run(), run());
}
