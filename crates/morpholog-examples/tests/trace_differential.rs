//! `propose` and `propose_with_trace` must answer every gallery case the same:
//! same outcome, rejection reason, and kernel error. They share one executor,
//! but `Stmt::For` branches on `trace.is_on()` to keep per-iteration
//! allocations off the untraced path, so the paths really differ.
//!
//! Fresh subjects are the one lawful difference: `new Subject()` mints a new
//! UUIDv7 per run. The comparison renames them away; the last test pins why.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use morpholog_core::{Stmt, TraceEntry, TracedProposal, propose_with_trace};
use morpholog_test_support::differential::{observable, sample_args, sample_state};
use morpholog_test_support::{propose_with_test_actor, test_transition};

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

                let untraced = propose_with_test_actor(
                    t,
                    args.clone(),
                    &state,
                    &program.invariants,
                    &program.definitions,
                );
                let transition = test_transition(t, args);
                let traced = propose_with_trace(
                    t,
                    &transition,
                    &state,
                    &program.invariants,
                    &program.definitions,
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
                assert_eq!(
                    observable(&untraced),
                    observable(&traced_as_result),
                    "programme `{}`, transformation `{}`, salt {salt}: \
                     trace mode changed the outcome",
                    program.name,
                    t.name
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

/// Two runs of a `new Subject()` transformation mint DIFFERENT identifiers,
/// so a traced dry run never predicts the identifiers of the run that
/// commits. The differential above holds only because it renames them away.
#[test]
fn fresh_subjects_differ_between_executions_by_design() {
    use morpholog_core::Outcome;
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

    let mut ids = Vec::new();
    for _ in 0..2 {
        let outcome = propose_with_test_actor(
            &t,
            vec![],
            &morpholog_core::State::default(),
            &p.invariants,
            &p.definitions,
        )
        .expect("minting evaluates");
        let Outcome::Accepted {
            asserted_claims, ..
        } = outcome
        else {
            panic!("minting is unconditional");
        };
        ids.push(format!("{:?}", asserted_claims[0].args[0]));
    }
    assert_ne!(
        ids[0], ids[1],
        "fresh subjects are minted per execution; equality here would \
         mean identifier reuse across proposals"
    );
    // And the normaliser sees through exactly this difference.
    assert_eq!(
        morpholog_test_support::differential::normalize_uuids(&ids[0]),
        morpholog_test_support::differential::normalize_uuids(&ids[1]),
    );
}
