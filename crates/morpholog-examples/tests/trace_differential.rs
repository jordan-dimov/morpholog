//! The trace differential: `propose` and `propose_with_trace` claim
//! one executor, differing only in the sink - but `Stmt::For`
//! genuinely branches on `trace.is_on()` to keep per-iteration
//! allocations off the untraced path, so the two paths carry real
//! duplicated control flow. This test makes them answer every
//! gallery case identically: same outcome, same rejection reason,
//! same kernel error - traced or not.
//!
//! Fresh subjects are the one lawful divergence: `let x = new
//! Subject()` mints a UUIDv7 per execution, so two runs of the same
//! proposal differ in exactly those identifiers. The observable
//! alpha-normalises them, and the characterisation test at the
//! bottom pins the reason - a traced dry run must never be read as
//! predicting the fresh identifiers of another execution.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use morpholog_core::{TracedProposal, propose_with_trace};
use morpholog_test_support::differential::{observable, sample_args, sample_state};
use morpholog_test_support::{propose_with_test_actor, test_transition};

#[test]
fn traced_and_untraced_execution_are_equivalent() {
    let mut cases = 0usize;
    let mut skipped = 0usize;
    for program in morpholog_examples::all_programs() {
        for t in &program.transformations {
            for salt in 0..3u64 {
                let Some(args) = sample_args(&program, t, salt) else {
                    skipped += 1;
                    continue;
                };
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
                    TracedProposal::Completed { outcome, .. } => Ok(outcome),
                    TracedProposal::Errored { error, .. } => Err(error),
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
        }
    }
    // No silent caps: a generator regression that skips most of the
    // corpus must fail here, not quietly shrink coverage.
    assert!(
        cases >= 100,
        "generator collapse: only {cases} cases ran ({skipped} skipped)"
    );
}

/// Two executions of a `new Subject()` transformation lawfully mint
/// DIFFERENT fresh identifiers - pinned so nobody reads a traced dry
/// run as predicting the identifiers of the run that commits. The
/// differential above only holds because its observable
/// alpha-normalises these.
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
