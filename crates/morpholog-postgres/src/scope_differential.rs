//! Proves predicate-scoped loading drops nothing: a proposal against state
//! cut down to `compute_load_scope`'s answer must behave exactly as against
//! full state. If any walker misses a predicate an evaluation can read (a
//! `value` default, one arm of `if`, a sum target, a defined-call chain, a
//! `pre`-only read), this fails without needing to know which walker.
//!
//! Pure kernel, no database. The corpus is every worked example plus
//! hostile fragments that hide predicates in awkward places, over
//! generated states and arguments. It proves nothing was dropped, not that
//! the scope is tight.
//!
//! Two checks, one per route. The interpreted scope (body and invariant
//! reads) must leave the whole outcome unchanged. The compiled route loads
//! only the body's reads, so staging over that projection must match
//! staging over full state. The first check cannot replace the second: a
//! dropped body read is hidden whenever an invariant reads the same
//! predicate.
//!
//! The projection is keyed as the loader keys it: a predicate a body
//! reads only through patterns with coordinates known before it runs
//! keeps only the rows matching one of those patterns. The hostile
//! fragments cover each way a coordinate can be known (a parameter, the
//! actor, a literal, a definition's parameter passed a known argument)
//! and each way it cannot (a key a `bind` or a `let` produces, a
//! rebound parameter, a pattern beside a keyed one that fixes nothing).
//!
//! In-crate so `compute_load_scope` can stay `pub(crate)`: the promise is
//! the equivalence, not the scope set itself.

use morpholog_core::{Program, StagedDelta, State, propose_stage_delta};
use morpholog_test_support::differential::{observable, sample_args, sample_state};
use morpholog_test_support::propose_with_test_actor;

use crate::propose::{LoadScope, Reads, compute_load_scope};

/// Hostile fragments: each hides a predicate where a careless walker
/// would lose it.
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
    // A key produced by `bind` is unknown before the body runs, so the
    // read it keys is whole; the bind's own pattern keys on the parameter.
    (
        "bind_key_deferred",
        "program bind_key_deferred
predicate Owner(k: Subject, who: Subject)
predicate Cleared(who: Subject)
predicate Out(k: Subject)
transformation clear(k):
    bind Owner(k, who)
    require Cleared(who)
    admit Out(k)
",
    ),
    // One pattern fixes nothing, beside one that keys: the predicate is
    // whole.
    (
        "keyed_beside_unkeyed",
        "program keyed_beside_unkeyed
predicate Seat(k: Subject, n: Decimal)
predicate Out(k: Subject)
transformation take(k):
    require not Seat(k, _)
    require exists n: Seat(_, n)
    admit Out(k)
",
    ),
    // Two coordinates in one pattern: a row must match both.
    (
        "two_coordinates",
        "program two_coordinates
predicate Booked(trade: Subject, book: Subject, n: Decimal)
predicate Out(trade: Subject)
transformation book(trade, book):
    require not Booked(trade, book, _)
    admit Out(trade)
",
    ),
    // A definition's parameter passed a parameter keys the definition's
    // pattern; passed a wildcard it does not.
    (
        "through_definition_parameter",
        "program through_definition_parameter
predicate Held(k: Subject, x: Subject)
predicate Free(x: Subject)
predicate Out(k: Subject)
define held(k, x):
    Held(k, x)
define free(x):
    Free(x)
transformation take(k):
    require not held(k, _)
    require not free(_)
    admit Out(k)
",
    ),
    // The actor keys a gate like a parameter does.
    (
        "actor_keyed",
        "program actor_keyed
predicate May(who: Subject, k: Subject)
predicate Out(k: Subject)
transformation act(k):
    require May(actor, k)
    admit Out(k)
",
    ),
    // A retract pattern and a sum's body key on the parameter.
    (
        "keyed_retract_and_sum",
        "program keyed_retract_and_sum
predicate Line(k: Subject, n: Decimal)
predicate Total(k: Subject, n: Decimal)
transformation settle(k):
    require sum(n | Line(k, n)) <= 100
    retract Total(k, _)
    admit Total(k, 1)
",
    ),
    // A key produced by `let` is unknown before the body runs.
    (
        "let_key_deferred",
        "program let_key_deferred
predicate Pointer(k: Subject, to: Subject)
predicate Target(to: Subject)
predicate Out(k: Subject)
transformation follow(k):
    let to = value Pointer(k, _)
    require Target(to)
    admit Out(k)
",
    ),
    // A parameter rebound by `let` or `for` is unknown wherever it is
    // read; the surface permits both.
    (
        "parameter_rebound_by_let",
        "program parameter_rebound_by_let
predicate P(x: Subject)
predicate Link(from: Subject, to: Subject)
predicate Out(x: Subject)
transformation t(k):
    let k = value Link(k, _) default k
    require P(k)
    admit Out(k)
",
    ),
    (
        "parameter_rebound_by_for",
        "program parameter_rebound_by_for
predicate P(x: Subject)
predicate Out(x: Subject)
transformation t(k, items):
    for k in items:
        require P(k)
    admit Out(k)
",
    ),
    // `Ledger` is reachable ONLY through the invariant's `pre(...)`. The
    // transformation must not touch it, or a statement walker would put it
    // in scope and the fragment would prove nothing about `pre`.
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

pub(crate) fn corpus_with_hostiles() -> Vec<(String, Program)> {
    corpus()
}

fn corpus() -> Vec<(String, Program)> {
    morpholog_examples::all_programs()
        .into_iter()
        .map(|p| (p.name.to_string(), p))
        .chain(hostile_programs())
        .collect()
}

fn project(full: &State, scope: &LoadScope) -> State {
    State::from_claims(
        full.claims()
            .iter()
            .filter(|c| scope.admits(c))
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
                let transition = morpholog_core::Transition {
                    transformation_name: t.name.clone(),
                    args,
                    actor: morpholog_test_support::test_actor(),
                };
                let scope = compute_load_scope(
                    t,
                    Some(&transition),
                    &program.invariants,
                    &program.definitions,
                    Reads::Body,
                );
                let projected = project(&full, &scope);
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
                let transition = morpholog_core::Transition {
                    transformation_name: t.name.clone(),
                    args: args.clone(),
                    actor: morpholog_test_support::test_actor(),
                };
                let scope = compute_load_scope(
                    t,
                    Some(&transition),
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

/// The plan's shape on the hostile fragments: which reads key, on what,
/// and which stay whole. Conjunction within a pattern is what makes a
/// narrow case narrow; a key a body produces, a rebound parameter, or an
/// unkeyed pattern beside a keyed one makes the read whole.
#[test]
fn the_read_plan_keys_what_the_body_fixes_and_nothing_else() {
    use morpholog_core::{KeyedPattern, KnownTerm, ReadFilter, ReadPlan};
    let plan = |program_name: &str, transformation: &str| -> ReadPlan {
        let (_, program) = hostile_programs()
            .into_iter()
            .find(|(n, _)| n == &format!("hostile:{program_name}"))
            .unwrap_or_else(|| panic!("{program_name} is a hostile fragment"));
        let t = program
            .transformations
            .iter()
            .find(|t| t.name.as_str() == transformation)
            .unwrap();
        ReadPlan::of(t, &program.definitions)
    };
    let param = |p: &str| KnownTerm::Parameter(p.into());
    let keyed = |patterns: &[&[(usize, KnownTerm)]]| {
        ReadFilter::Keyed(
            patterns
                .iter()
                .map(|c| KeyedPattern {
                    coordinates: c.to_vec(),
                })
                .collect(),
        )
    };

    let p = plan("bind_key_deferred", "clear");
    assert_eq!(p.reads[&"Owner".into()], keyed(&[&[(0, param("k"))]]));
    assert_eq!(p.reads[&"Cleared".into()], ReadFilter::Whole);
    assert_eq!(p.admits[&"Out".into()], keyed(&[&[(0, param("k"))]]));

    let p = plan("keyed_beside_unkeyed", "take");
    assert_eq!(p.reads[&"Seat".into()], ReadFilter::Whole);

    let p = plan("two_coordinates", "book");
    assert_eq!(
        p.reads[&"Booked".into()],
        keyed(&[&[(0, param("trade")), (1, param("book"))]]),
        "two coordinates of one pattern stay one conjunction"
    );

    let p = plan("through_definition_parameter", "take");
    assert_eq!(p.reads[&"Held".into()], keyed(&[&[(0, param("k"))]]));
    assert_eq!(p.reads[&"Free".into()], ReadFilter::Whole);

    let p = plan("actor_keyed", "act");
    assert_eq!(
        p.reads[&"May".into()],
        keyed(&[&[(0, KnownTerm::Actor), (1, param("k"))]])
    );

    let p = plan("keyed_retract_and_sum", "settle");
    assert_eq!(p.reads[&"Line".into()], keyed(&[&[(0, param("k"))]]));
    assert_eq!(p.reads[&"Total".into()], keyed(&[&[(0, param("k"))]]));
    assert_eq!(
        p.admits[&"Total".into()],
        keyed(&[&[
            (0, param("k")),
            (
                1,
                KnownTerm::Literal(morpholog_core::Value::Decimal("1".into()))
            )
        ]])
    );

    let p = plan("let_key_deferred", "follow");
    assert_eq!(p.reads[&"Pointer".into()], keyed(&[&[(0, param("k"))]]));
    assert_eq!(p.reads[&"Target".into()], ReadFilter::Whole);

    let p = plan("parameter_rebound_by_let", "t");
    assert_eq!(p.reads[&"Link".into()], ReadFilter::Whole);
    assert_eq!(p.reads[&"P".into()], ReadFilter::Whole);
    assert_eq!(p.admits[&"Out".into()], ReadFilter::Whole);
    let p = plan("parameter_rebound_by_for", "t");
    assert_eq!(p.reads[&"P".into()], ReadFilter::Whole);
    assert_eq!(p.admits[&"Out".into()], ReadFilter::Whole);
}

/// A parameter rebound by `let` reads through its new value: with a link
/// from the argument to another subject, and a row only under that other
/// subject, the projection must still hold the row the body reads. The
/// generated states seldom hold this coincidence, so it is built.
#[test]
fn a_rebound_parameter_reads_its_predicate_whole() {
    let (_, program) = hostile_programs()
        .into_iter()
        .find(|(n, _)| n == "hostile:parameter_rebound_by_let")
        .unwrap();
    let t = &program.transformations[0];
    let full = State::from_claims(vec![
        morpholog_test_support::claim_instance(
            "Link",
            &[
                morpholog_test_support::subj("k"),
                morpholog_test_support::subj("t"),
            ],
        ),
        morpholog_test_support::claim_instance("P", &[morpholog_test_support::subj("t")]),
    ]);
    let args = vec![morpholog_test_support::subj("k")];
    let transition = morpholog_core::Transition {
        transformation_name: t.name.clone(),
        args: args.clone(),
        actor: morpholog_test_support::test_actor(),
    };
    let scope = compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        Reads::Body,
    );
    let projected = project(&full, &scope);
    let on_full = propose_stage_delta(t, &transition, &full, &program.definitions);
    let on_projected = propose_stage_delta(t, &transition, &projected, &program.definitions);
    assert!(
        matches!(on_full, Ok(StagedDelta::Staged { .. })),
        "the body reads P through the link: {on_full:?}"
    );
    assert_eq!(
        staged_observable(&on_full),
        staged_observable(&on_projected)
    );
}
