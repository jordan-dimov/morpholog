//! The scoped-loading differential: a proposal evaluated against a
//! state projected to `compute_load_scope`'s answer must be
//! observationally equivalent to the same proposal against full
//! state. This is the end-to-end law behind predicate-scoped
//! loading: if a walker ever omits a predicate an evaluation path
//! can read (a `value` default, one arm of `if`, an expression-valued
//! sum target, a defined-call chain, a `pre`-only read), this test
//! reddens without knowing which walker, or which AST node, was at
//! fault.
//!
//! Pure kernel; no database. Cases are the whole worked-example
//! gallery plus hostile fragments whose predicates hide in exactly
//! the awkward positions, each against deterministic generated
//! states and arguments (over-loading is invisible here - the law
//! proves nothing was DROPPED, not that the scope is tight).
//!
//! Two laws, one per answer the scope authority gives. The interpreted
//! answer (body reads and invariant reads) must leave the whole
//! proposal's outcome unchanged. The compiled route loads the body's
//! reads alone and judges invariants in SQL, so its law is about the
//! body: staging over the projection must equal staging over full
//! state. The first law cannot stand in for the second - a body walker
//! that drops a predicate is masked whenever an invariant happens to
//! read the same predicate.
//!
//! In-crate rather than an integration test on purpose: the public
//! promise is the EQUIVALENCE, not the scope set itself -
//! `compute_load_scope` stays `pub(crate)` so the loading mechanism
//! can change without a public API having promised today's answer.

use morpholog_core::{Program, State, StagedDelta, propose_stage_delta};
use morpholog_test_support::differential::{observable, sample_args, sample_state};
use morpholog_test_support::propose_with_test_actor;

use crate::propose::{Reads, compute_load_scope};

/// Hostile fragments: each hides a predicate somewhere a lazy or
/// forgetful walker would lose it. Kept small and self-describing;
/// the gallery supplies breadth, these supply spite.
const HOSTILE: &[(&str, &str)] = &[
    (
        "value_default_only",
        "program value_default_only
predicate Reading(m: Subject, v: Decimal)
predicate Fallback(v: Decimal)
predicate Out(v: Decimal)
transformation record(m):
    let v = value Reading(m, _) default value Fallback(_)
    admit Out(v)
",
    ),
    (
        "if_branch_only",
        "program if_branch_only
predicate Armed(x: Subject)
predicate OnlyThen(v: Decimal)
predicate OnlyOtherwise(v: Decimal)
predicate Out(v: Decimal)
transformation pick(x):
    let v = if(Armed(x), value OnlyThen(_), value OnlyOtherwise(_))
    admit Out(v)
",
    ),
    (
        "sum_target_lookup",
        "program sum_target_lookup
predicate Holding(h: Subject, n: Decimal)
predicate Weight(f: Decimal)
predicate Book(v: Decimal)
transformation total(caller):
    let v = sum(n * (value Weight(_)) | Holding(_, n))
    admit Book(v)
",
    ),
    (
        "through_defined_chain",
        "program through_defined_chain
predicate Inner(x: Subject)
predicate Out(x: Subject)
define leaf(x):
    Inner(x)
define stem(x):
    leaf(x)
transformation note(x):
    require stem(x)
    admit Out(x)
",
    ),
    // `Ledger` is reachable ONLY through the invariant's `pre(...)`:
    // the transformation never touches it, so no statement walker can
    // smuggle it into the scope. (An earlier version of this fixture
    // also retracted `Ledger`, which put it in the read set through
    // `Stmt::Retract` and proved nothing about descending `pre` - the
    // review caught the fixture testing less than its name claimed.)
    (
        "pre_only_read",
        "program pre_only_read
predicate Ledger(x: Subject)
predicate Out(x: Subject)
invariant there_was_no_ledger_before:
    not pre(Ledger(_))
transformation touch(x):
    admit Out(x)
",
    ),
];

fn hostile_programs() -> Vec<(String, Program)> {
    HOSTILE
        .iter()
        .map(|(name, source)| {
            let program = morpholog_surface::parse_program(source)
                .unwrap_or_else(|e| panic!("hostile fragment `{name}` must parse: {e:?}"));
            (format!("hostile:{name}"), program)
        })
        .collect()
}

fn corpus() -> Vec<(String, Program)> {
    morpholog_examples::all_programs()
        .into_iter()
        .map(|p| (p.name.to_string(), p))
        .chain(hostile_programs())
        .collect()
}

fn project(full: &State, scope: &[morpholog_core::PredicateName]) -> State {
    State::from_claims(
        full.claims()
            .iter()
            .filter(|c| scope.contains(&c.predicate))
            .cloned()
            .collect(),
    )
}

/// What a staged body shows an observer: the decision, or the delta
/// it would apply. A kernel error is compared by its text.
fn staged_observable(staged: &Result<StagedDelta, morpholog_core::EvalError>) -> String {
    match staged {
        Ok(StagedDelta::Rejected { reason }) => format!("rejected: {reason}"),
        Ok(StagedDelta::Staged {
            asserted,
            retracted,
            emitted,
        }) => format!("staged: +{asserted:?} -{retracted:?} !{emitted:?}"),
        Err(e) => format!("error: {e}"),
    }
}

#[test]
fn body_only_scope_stages_the_same_delta_as_full_state() {
    let mut cases = 0usize;
    let mut skipped = 0usize;
    for (name, program) in &corpus() {
        for t in &program.transformations {
            for salt in 0..3u64 {
                let Some(args) = sample_args(program, t, salt) else {
                    skipped += 1;
                    continue;
                };
                let full = sample_state(program, 2, salt);
                let scope = compute_load_scope(
                    t,
                    &program.invariants,
                    &program.definitions,
                    Reads::Body,
                );
                let projected = project(&full, &scope);
                let transition = morpholog_core::Transition {
                    transformation_name: t.name.clone(),
                    args,
                    actor: morpholog_test_support::test_actor(),
                };
                let on_full = propose_stage_delta(t, &transition, &full, &program.definitions);
                let on_projected =
                    propose_stage_delta(t, &transition, &projected, &program.definitions);
                assert_eq!(
                    staged_observable(&on_full),
                    staged_observable(&on_projected),
                    "programme `{name}`, transformation `{}`, salt {salt}: \
                     the body-only projection changed the staged delta - a \
                     body walker dropped a predicate the body reads (scope: {scope:?})",
                    t.name
                );
                cases += 1;
            }
        }
    }
    assert!(
        cases >= 100,
        "generator collapse: only {cases} cases ran ({skipped} skipped)"
    );
}

#[test]
fn scoped_loading_is_observationally_equivalent_to_full_state() {
    let mut cases = 0usize;
    let mut skipped = 0usize;
    let corpus = corpus();

    for (name, program) in &corpus {
        for t in &program.transformations {
            for salt in 0..3u64 {
                let Some(args) = sample_args(program, t, salt) else {
                    skipped += 1;
                    continue;
                };
                let full = sample_state(program, 2, salt);
                let scope = compute_load_scope(
                    t,
                    &program.invariants,
                    &program.definitions,
                    Reads::BodyAndInvariants,
                );
                let projected = project(&full, &scope);

                let on_full = propose_with_test_actor(
                    t,
                    args.clone(),
                    &full,
                    &program.invariants,
                    &program.definitions,
                );
                let on_projected = propose_with_test_actor(
                    t,
                    args,
                    &projected,
                    &program.invariants,
                    &program.definitions,
                );
                assert_eq!(
                    observable(&on_full),
                    observable(&on_projected),
                    "programme `{name}`, transformation `{}`, salt {salt}: \
                     the projected state changed the outcome - a walker \
                     dropped a predicate the evaluator reads (scope: {scope:?})",
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
