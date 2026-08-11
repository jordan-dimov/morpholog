//! The scoped-loading differential: a proposal evaluated against a
//! state projected to `compute_load_scope`'s answer must be
//! observationally equivalent to the same proposal against full
//! state. This is the end-to-end law behind predicate-scoped
//! loading: if a walker ever omits a predicate an evaluation path
//! can read (a `value` default, one arm of `if`, an expression-valued
//! sum target, a defined-call chain), this test reddens without
//! knowing which walker, or which AST node, was at fault.
//!
//! Pure kernel + the real `compute_load_scope`; no database. Cases
//! are the whole worked-example gallery plus hostile fragments whose
//! predicates hide in exactly the awkward positions, each against
//! deterministic generated states and arguments (over-loading is
//! invisible here - the law proves nothing was DROPPED, not that the
//! scope is tight).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Program, State};
use morpholog_postgres::compute_load_scope;
use morpholog_test_support::differential::{observable, sample_args, sample_state};
use morpholog_test_support::propose_with_test_actor;

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
    (
        "pre_only_read",
        "program pre_only_read
predicate Ledger(x: Subject)
predicate Out(x: Subject)
invariant ledger_never_shrinks:
    pre(Ledger(x)) implies Ledger(x)
transformation retract_one(x):
    retract Ledger(x)
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

#[test]
fn scoped_loading_is_observationally_equivalent_to_full_state() {
    let mut cases = 0usize;
    let mut skipped = 0usize;
    let corpus: Vec<(String, Program)> = morpholog_examples::all_programs()
        .into_iter()
        .map(|p| (p.name.to_string(), p))
        .chain(hostile_programs())
        .collect();

    for (name, program) in &corpus {
        for t in &program.transformations {
            for salt in 0..3u64 {
                let Some(args) = sample_args(program, t, salt) else {
                    skipped += 1;
                    continue;
                };
                let full = sample_state(program, 2, salt);
                let scope = compute_load_scope(t, &program.invariants, &program.definitions);
                let projected = State::from_claims(
                    full.claims()
                        .iter()
                        .filter(|c| scope.contains(&c.predicate))
                        .cloned()
                        .collect(),
                );

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
