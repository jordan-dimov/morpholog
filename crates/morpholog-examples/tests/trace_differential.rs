//! `propose` and `propose_with_trace` must answer every gallery case the same:
//! same outcome, rejection reason, and kernel error. They share one executor,
//! but `Stmt::For` branches on `trace.is_on()` to keep per-iteration
//! allocations off the untraced path, so the paths really differ.
//!
//! Both runs take the same subjects for `new Subject()`, so they are compared
//! exactly: tracing may change what observation costs, never the outcome,
//! the error, or which subjects execution draws.
//!
//! An explanation is a projection of the same traced execution, never a
//! second evaluator: the same inputs give the same explanation, and it
//! names the decision the proposal actually reached.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use morpholog_core::{
    EvalError, Explanation, GateKind, Outcome, Rejection, RejectionReason, Stmt, TraceEntry,
    TracedProposal, Verdict,
};
use morpholog_test_support::differential::{same_subjects, sample_args, sample_state};
use morpholog_test_support::test_transition;
use morpholog_test_support::{explain, propose, propose_with_trace};

/// The verdict `explanation` gives, once it is checked to name the decision
/// the same execution reached; `Err` says how the two disagree.
fn agreeing_verdict(
    outcome: &Result<Outcome, EvalError>,
    explanation: &Explanation,
) -> Result<&'static str, String> {
    let gate = |kind: GateKind, name: &Option<morpholog_core::RuleName>, rendered: &str| {
        matches!(
            &explanation.verdict,
            Verdict::Rejected(Rejection::Gate(g))
                if g.statement_kind == kind
                    && g.rule.as_deref() == name.as_ref().map(morpholog_core::RuleName::as_str)
                    && g.gate == rendered
        )
    };
    let agrees = match outcome {
        Ok(Outcome::Accepted { .. }) => {
            matches!(explanation.verdict, Verdict::Admissible).then_some("admissible")
        }
        Ok(Outcome::Rejected {
            reason: RejectionReason::Invariant { name, .. },
        }) => matches!(
            &explanation.verdict,
            Verdict::Rejected(Rejection::Invariant(r)) if r.name == name.as_str()
        )
        .then_some("invariant"),
        Ok(Outcome::Rejected {
            reason: RejectionReason::Require { name, rendered, .. },
        }) => gate(GateKind::Require, name, rendered).then_some("require"),
        Ok(Outcome::Rejected {
            reason: RejectionReason::BindNone { name, rendered, .. },
        }) => gate(GateKind::BindOne, name, rendered).then_some("bind"),
        Err(e) => matches!(
            &explanation.verdict,
            Verdict::Rejected(Rejection::Error(r)) if r.message == e.to_string()
        )
        .then_some("error"),
    };
    agrees.ok_or_else(|| format!("{outcome:?} explained as {:?}", explanation.verdict))
}

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
    let mut verdicts: BTreeSet<&'static str> = BTreeSet::new();
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
                let untraced = propose(&program, &transition, &state, &mut same_subjects());
                let traced =
                    propose_with_trace(&program, &transition, &state, &mut same_subjects());
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
                // Prepared separately, so each run's hash maps are seeded
                // differently.
                let explanation = explain(&program, &transition, &state, &mut same_subjects());
                assert_eq!(
                    explanation,
                    explain(&program, &transition, &state, &mut same_subjects()),
                    "programme `{}`, transformation `{}`, salt {salt}: \
                     the same inputs gave a different explanation",
                    program.name,
                    t.name
                );
                match agreeing_verdict(&untraced, &explanation) {
                    Ok(verdict) => {
                        verdicts.insert(verdict);
                    }
                    Err(why) => panic!(
                        "programme `{}`, transformation `{}`, salt {salt}: \
                         the explanation does not name the decision: {why}",
                        program.name, t.name
                    ),
                }
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
    // The gallery reaches every verdict but a kernel error, which
    // `explanation_fixtures` supplies.
    let missing: Vec<_> = ["admissible", "bind", "invariant", "require"]
        .into_iter()
        .filter(|v| !verdicts.contains(v))
        .collect();
    assert!(
        missing.is_empty(),
        "no generated case reached the verdicts {missing:?}"
    );
}

/// Shapes the gallery does not reach: a refusal inside nested loops, which
/// the explanation must find in the iteration that refused, and a kernel
/// error from a programme that validates.
#[test]
fn explanation_fixtures_name_their_decisions() {
    use morpholog_test_support::{coll, dec, subj};
    let program = morpholog_surface::parse_program(
        "
program explained
predicate Allowed(i: Subject, j: Subject)

transformation every_pair(items, inners):
    for i in items:
        for j in inners:
            require Allowed(i, j)

transformation share(amount, parts):
    require amount / parts > 0
",
    )
    .unwrap();
    let allowed =
        |i: &str, j: &str| morpholog_test_support::claim_instance("Allowed", &[subj(i), subj(j)]);
    let state = morpholog_core::State::from_claims(vec![
        allowed("a", "x"),
        allowed("a", "y"),
        allowed("b", "x"),
    ]);
    let every_pair = program.transformation("every_pair").unwrap();
    let share = program.transformation("share").unwrap();
    for (transition, expected, gate) in [
        (
            test_transition(
                every_pair,
                vec![
                    coll(vec![subj("a"), subj("b")]),
                    coll(vec![subj("x"), subj("y")]),
                ],
            ),
            "require",
            Some("Allowed(i, j)"),
        ),
        (test_transition(share, vec![dec(10), dec(0)]), "error", None),
    ] {
        let outcome = propose(&program, &transition, &state, &mut same_subjects());
        let explanation = explain(&program, &transition, &state, &mut same_subjects());
        assert_eq!(
            explanation,
            explain(&program, &transition, &state, &mut same_subjects())
        );
        assert_eq!(
            agreeing_verdict(&outcome, &explanation),
            Ok(expected),
            "{}",
            transition.transformation_name
        );
        if let (Some(gate), Verdict::Rejected(Rejection::Gate(g))) = (gate, &explanation.verdict) {
            assert_eq!(
                g.gate, gate,
                "the refusing statement, from the iteration that refused"
            );
        }
    }
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
                &p,
                &transition,
                &morpholog_core::State::default(),
                &mut same_subjects(),
            )
        )
    };
    assert_eq!(run(), run());
}
