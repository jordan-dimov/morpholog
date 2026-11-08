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
//! reads only through patterns with known coordinates keeps only the
//! rows matching one of those patterns, a coordinate a `bind` fixed
//! resolved against the whole state. The hostile fragments cover each
//! way a coordinate can be known (a parameter, the actor, a literal, a
//! definition's parameter passed a known argument, a value an earlier
//! `bind` found, an element of a collection parameter a `for`, a
//! `forall` or a membership test ranges over) and each way it cannot (a
//! key a `let` produces, a rebound parameter, a pattern beside a keyed
//! one that fixes nothing, a read before the bind that would key it, a
//! membership test behind a conjunct that can raise, a loop binding a
//! `let` replaces).
//!
//! Generated states seldom let a `bind` match, so the dependent reads
//! are also exercised over built states where every link of a chain
//! matches, and the projection is held to be strictly smaller than the
//! whole state there: equal outcomes over a projection that dropped
//! nothing would also hold for one that narrowed nothing.
//!
//! In-crate so `compute_load_scope` can stay `pub(crate)`: the promise is
//! the equivalence, not the scope set itself.

use morpholog_core::{Program, StagedDelta, State};
use morpholog_test_support::differential::{observable, same_subjects, sample_args, sample_state};

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
    // A value a `bind` found keys the read after it: the bind's own
    // pattern keys on the parameter, the read on the field the matching
    // row holds.
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
    // A parameter the bind does not mention stays bound after it: a
    // match extends the bindings in force, so the read after the bind
    // keys on the parameter still.
    (
        "parameter_survives_bind",
        "program parameter_survives_bind
predicate Owner(who: Subject)
predicate Cleared(k: Subject)
predicate Out(k: Subject)
transformation clear(k):
    bind Owner(who)
    require Cleared(k)
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
    // The spike's shape: each link of a chain keyed by the one before
    // it, a gate and a retract pattern keyed two and one links down.
    (
        "bind_chain",
        "program bind_chain
predicate Allocation(a: Subject, shipment: Subject, item: Subject)
predicate Shipment(shipment: Subject, order: Subject)
predicate Order(order: Subject, customer: Subject)
predicate Stock(item: Subject, on_hand: Decimal)
predicate Good(customer: Subject)
transformation ship(a):
    bind Allocation(a, shipment, item)
    bind Shipment(shipment, order)
    bind Order(order, customer)
    require Good(customer)
    bind Stock(item, on_hand)
    require on_hand >= 1
    let left = on_hand - 1
    retract Stock(item, on_hand)
    admit Stock(item, left)
",
    ),
    // A read before the bind names a fresh variable, not the bind's.
    (
        "read_before_bind",
        "program read_before_bind
predicate Owner(k: Subject, who: Subject)
predicate Cleared(who: Subject)
predicate Out(k: Subject)
transformation clear(k):
    require Cleared(who)
    bind Owner(k, who)
    admit Out(k)
",
    ),
    // A name a `let` also binds is never known, before or after.
    (
        "bind_then_let",
        "program bind_then_let
predicate Owner(k: Subject, who: Subject)
predicate Pointer(k: Subject, to: Subject)
predicate Cleared(who: Subject)
predicate Out(k: Subject)
transformation clear(k):
    bind Owner(k, who)
    let who = value Pointer(k, _) default who
    require Cleared(who)
    admit Out(k)
",
    ),
    // A bind inside a `for` body keys the rest of that body and nothing
    // after it.
    (
        "bind_in_for",
        "program bind_in_for
predicate Owner(k: Subject, who: Subject)
predicate Cleared(who: Subject)
predicate Done(who: Subject)
predicate Out(k: Subject)
transformation clear(k, others):
    for o in others:
        bind Owner(k, who)
        require Cleared(who)
        admit Out(o)
    require Done(who)
    admit Out(k)
",
    ),
    // A bind whose own pattern fixes nothing gives nothing.
    (
        "whole_bind_gives_nothing",
        "program whole_bind_gives_nothing
predicate Owner(k: Subject, who: Subject)
predicate Cleared(who: Subject)
predicate Out(k: Subject)
transformation clear(k):
    bind Owner(_, who)
    require Cleared(who)
    admit Out(k)
",
    ),
    // A bound field passes into a definition and keys a retract pattern.
    (
        "bind_key_through_definition_and_retract",
        "program bind_key_through_definition_and_retract
predicate Owner(k: Subject, who: Subject)
predicate Cleared(who: Subject)
predicate Flag(who: Subject, n: Decimal)
predicate Out(k: Subject)
define cleared(x):
    Cleared(x)
transformation clear(k):
    bind Owner(k, who)
    require cleared(who)
    retract Flag(who, _)
    admit Out(k)
",
    ),
    // An element of a collection parameter keys a read, through a `for`,
    // a `forall ... in`, or a membership test beside a pattern; the
    // fragments after the first four are the ways it does not.
    (
        "for_over_parameter",
        "program for_over_parameter
predicate Stock(item: Subject, n: Decimal)
predicate Held(item: Subject)
predicate Out(t: Subject)
transformation merge(t, others):
    for o in others:
        require Held(o)
        retract Stock(o, _)
    admit Out(t)
",
    ),
    (
        "forall_in_parameter",
        "program forall_in_parameter
predicate Stock(item: Subject, n: Decimal)
predicate Out(t: Subject)
transformation merge(t, others):
    require all_stocked: forall o in others: Stock(o, _)
    admit Out(t)
",
    ),
    (
        "member_in_negated_exists",
        "program member_in_negated_exists
predicate Alloc(a: Subject, item: Subject)
predicate Out(t: Subject)
transformation merge(t, others, listed):
    require every_listed: not (exists a: Alloc(a, i) and i in others and not (a in listed))
    admit Out(t)
",
    ),
    (
        "member_in_sum",
        "program member_in_sum
predicate Stock(item: Subject, n: Decimal)
predicate Total(t: Subject, n: Decimal)
transformation merge(t, others):
    let added = sum(q | Stock(o, q) and o in others)
    admit Total(t, added)
",
    ),
    (
        "member_before_pattern",
        "program member_before_pattern
predicate Stock(item: Subject, n: Decimal)
predicate Out(t: Subject)
transformation merge(t, others):
    require some: exists o: o in others and Stock(o, _)
    admit Out(t)
",
    ),
    (
        "member_after_raising",
        "program member_after_raising
predicate Stock(item: Subject, q: Decimal)
predicate Out(t: Subject)
transformation check(t, others):
    require exists x: Stock(x, q) and 1 / q > 0 and x in others
    admit Out(t)
",
    ),
    (
        "negated_membership",
        "program negated_membership
predicate Stock(item: Subject, n: Decimal)
predicate Out(t: Subject)
transformation merge(t, others):
    require none_outside: not (exists o: Stock(o, _) and not (o in others))
    admit Out(t)
",
    ),
    (
        "membership_under_or",
        "program membership_under_or
predicate Stock(item: Subject, n: Decimal)
predicate Out(t: Subject)
transformation merge(t, others):
    require some: exists o: Stock(o, _) and (o in others or o = t)
    admit Out(t)
",
    ),
    (
        "for_over_let_collection",
        "program for_over_let_collection
predicate Items(t: Subject, items: Collection)
predicate Stock(item: Subject, n: Decimal)
predicate Out(t: Subject)
transformation merge(t):
    let items = value Items(t, _)
    for o in items:
        require Stock(o, _)
    admit Out(t)
",
    ),
    (
        "collection_rebound_by_let",
        "program collection_rebound_by_let
predicate Items(t: Subject, items: Collection)
predicate Stock(item: Subject, n: Decimal)
predicate Out(t: Subject)
transformation merge(t, others):
    let others = value Items(t, _) default others
    for o in others:
        require Stock(o, _)
    admit Out(t)
",
    ),
    (
        "loop_binding_rebound_by_let",
        "program loop_binding_rebound_by_let
predicate Stock(item: Subject)
predicate Out(t: Subject)
transformation check(t, others):
    for o in others:
        let o = #outside
        require Stock(o)
    admit Out(t)
",
    ),
    (
        "loop_binding_read_after_loop",
        "program loop_binding_read_after_loop
predicate Stock(item: Subject, n: Decimal)
predicate Done(item: Subject)
predicate Out(t: Subject)
transformation merge(t, others):
    for o in others:
        require Stock(o, _)
    require Done(o)
    admit Out(t)
",
    ),
    // A loop binding named like an earlier bind's variable is the loop's
    // element inside the loop, not the bind's value.
    (
        "loop_binding_shadows_bind_field",
        "program loop_binding_shadows_bind_field
predicate Link(t: Subject, o: Subject)
predicate Stock(item: Subject)
predicate Out(t: Subject)
transformation check(t, others):
    bind Link(t, o)
    for o in others:
        require Stock(o)
    admit Out(t)
",
    ),
    // An inner loop over a collection the body computed reuses the outer
    // loop's name: inside it the name is the inner loop's, unknown.
    (
        "nested_loop_rebinds_the_name",
        "program nested_loop_rebinds_the_name
predicate Items(t: Subject, items: Collection)
predicate Stock(item: Subject)
predicate Out(t: Subject)
transformation check(t, others):
    let more = value Items(t, _)
    for o in others:
        for o in more:
            require Stock(o)
    admit Out(t)
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
    (
        "mints",
        "program mints
predicate Minted(x: Subject, s: Subject)
transformation mint(x):
    let s = new Subject()
    admit Minted(x, s)
",
    ),
];

pub(crate) fn hostile_program(name: &str) -> Program {
    hostile_programs()
        .into_iter()
        .find(|(n, _)| n == &format!("hostile:{name}"))
        .unwrap_or_else(|| panic!("{name} is a hostile fragment"))
        .1
}

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
            .filter(|c| scope.admits(c, full))
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
        let prepared = morpholog_test_support::prepare(program);
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
                    decided_at: None,
                };
                let scope = compute_load_scope(
                    t,
                    Some(&transition),
                    &program.invariants,
                    &program.definitions,
                    Reads::Body,
                );
                let projected = project(&full, &scope);
                let on_full = prepared
                    .stage_delta(&transition, &full, &mut same_subjects())
                    .map(|staged| staged.expect("the transformation is declared"));
                let on_projected = prepared
                    .stage_delta(&transition, &projected, &mut same_subjects())
                    .map(|staged| staged.expect("the transformation is declared"));
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
    let mut minted = false;
    let corpus = corpus();

    for (name, program) in &corpus {
        let prepared = morpholog_test_support::prepare(program);
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
                    decided_at: None,
                };
                let scope = compute_load_scope(
                    t,
                    Some(&transition),
                    &program.invariants,
                    &program.definitions,
                    Reads::BodyAndInvariants,
                );
                let projected = project(&full, &scope);

                // Both runs draw the same subjects, so a minted subject
                // compares like any other value.
                let on_full = prepared
                    .propose(&transition, &full, &mut same_subjects())
                    .map(|outcome| outcome.expect("the transformation is declared"));
                let on_projected = prepared
                    .propose(&transition, &projected, &mut same_subjects())
                    .map(|outcome| outcome.expect("the transformation is declared"));
                minted |= observable(&on_full).contains("same-0");
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
    assert!(
        minted,
        "no case committed a minted subject, so the shared subject sequence is untested"
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
        let program = hostile_program(program_name);
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

    let field = |predicate: &str, position: usize, of: &[(usize, KnownTerm)]| KnownTerm::Field {
        predicate: predicate.into(),
        position,
        of: of.to_vec(),
    };

    let p = plan("bind_key_deferred", "clear");
    assert_eq!(p.reads[&"Owner".into()], keyed(&[&[(0, param("k"))]]));
    let who = field("Owner", 1, &[(0, param("k"))]);
    assert_eq!(p.reads[&"Cleared".into()], keyed(&[&[(0, who.clone())]]));
    assert_eq!(p.admits[&"Out".into()], keyed(&[&[(0, param("k"))]]));

    let p = plan("bind_chain", "ship");
    let shipment = field("Allocation", 1, &[(0, param("a"))]);
    let item = field("Allocation", 2, &[(0, param("a"))]);
    let order = field("Shipment", 1, &[(0, shipment.clone())]);
    let customer = field("Order", 1, &[(0, order.clone())]);
    assert_eq!(p.reads[&"Shipment".into()], keyed(&[&[(0, shipment)]]));
    assert_eq!(p.reads[&"Order".into()], keyed(&[&[(0, order)]]));
    assert_eq!(p.reads[&"Good".into()], keyed(&[&[(0, customer)]]));
    assert_eq!(
        p.reads[&"Stock".into()],
        keyed(&[&[(0, item.clone())]]),
        "the retract pattern, keyed by the item and the quantity the bind found, adds nothing to the bind's own"
    );
    assert_eq!(
        p.admits[&"Stock".into()],
        keyed(&[&[(0, item)]]),
        "`left` is a let, so the admit keys on the item alone"
    );

    let p = plan("read_before_bind", "clear");
    assert_eq!(p.reads[&"Cleared".into()], ReadFilter::Whole);
    assert_eq!(p.reads[&"Owner".into()], keyed(&[&[(0, param("k"))]]));

    let p = plan("bind_then_let", "clear");
    assert_eq!(p.reads[&"Owner".into()], keyed(&[&[(0, param("k"))]]));
    assert_eq!(p.reads[&"Cleared".into()], ReadFilter::Whole);

    let p = plan("bind_in_for", "clear");
    assert_eq!(p.reads[&"Cleared".into()], keyed(&[&[(0, who.clone())]]));
    assert_eq!(p.reads[&"Done".into()], ReadFilter::Whole);

    let p = plan("whole_bind_gives_nothing", "clear");
    assert_eq!(p.reads[&"Owner".into()], ReadFilter::Whole);
    assert_eq!(p.reads[&"Cleared".into()], ReadFilter::Whole);

    let p = plan("bind_key_through_definition_and_retract", "clear");
    assert_eq!(p.reads[&"Cleared".into()], keyed(&[&[(0, who.clone())]]));
    assert_eq!(p.reads[&"Flag".into()], keyed(&[&[(0, who)]]));

    let p = plan("parameter_survives_bind", "clear");
    assert_eq!(p.reads[&"Owner".into()], ReadFilter::Whole);
    assert_eq!(p.reads[&"Cleared".into()], keyed(&[&[(0, param("k"))]]));

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
                KnownTerm::Literal(morpholog_core::Value::decimal("1").unwrap())
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
    assert_eq!(
        p.reads[&"P".into()],
        keyed(&[&[(0, KnownTerm::Element("items".into()))]]),
        "inside the loop the name is the loop's element, whatever it was outside"
    );
    assert_eq!(p.admits[&"Out".into()], ReadFilter::Whole);

    let element = |p: &str| KnownTerm::Element(p.into());
    let p = plan("for_over_parameter", "merge");
    assert_eq!(p.reads[&"Held".into()], keyed(&[&[(0, element("others"))]]));
    assert_eq!(
        p.reads[&"Stock".into()],
        keyed(&[&[(0, element("others"))]])
    );
    let p = plan("forall_in_parameter", "merge");
    assert_eq!(
        p.reads[&"Stock".into()],
        keyed(&[&[(0, element("others"))]])
    );
    let p = plan("member_in_negated_exists", "merge");
    assert_eq!(
        p.reads[&"Alloc".into()],
        keyed(&[&[(1, element("others"))]])
    );
    let p = plan("member_in_sum", "merge");
    assert_eq!(
        p.reads[&"Stock".into()],
        keyed(&[&[(0, element("others"))]])
    );
    let p = plan("member_before_pattern", "merge");
    assert_eq!(
        p.reads[&"Stock".into()],
        keyed(&[&[(0, element("others"))]])
    );
    for (fragment, transformation) in [
        ("member_after_raising", "check"),
        ("negated_membership", "merge"),
        ("membership_under_or", "merge"),
        ("for_over_let_collection", "merge"),
        ("collection_rebound_by_let", "merge"),
        ("loop_binding_rebound_by_let", "check"),
    ] {
        let p = plan(fragment, transformation);
        assert_eq!(
            p.reads[&"Stock".into()],
            ReadFilter::Whole,
            "{fragment}: no element key here"
        );
    }
    let p = plan("loop_binding_read_after_loop", "merge");
    assert_eq!(
        p.reads[&"Stock".into()],
        keyed(&[&[(0, element("others"))]])
    );
    assert_eq!(p.reads[&"Done".into()], ReadFilter::Whole);
    let p = plan("loop_binding_shadows_bind_field", "check");
    assert_eq!(
        p.reads[&"Stock".into()],
        keyed(&[&[(0, element("others"))]]),
        "the loop's name wins over the bind's inside the loop"
    );
    let p = plan("nested_loop_rebinds_the_name", "check");
    assert_eq!(
        p.reads[&"Stock".into()],
        ReadFilter::Whole,
        "inside the inner loop the name is the inner loop's"
    );
}

/// Stage one transformation of a hostile fragment over `full` and over
/// its body-only projection; both must show the same.
fn stage_over(
    fragment: &str,
    args: Vec<morpholog_core::EvalValue>,
    full: Vec<morpholog_core::ClaimInstance>,
) -> (String, State) {
    let program = hostile_program(fragment);
    let t = &program.transformations[0];
    let prepared = morpholog_test_support::prepare(&program);
    let full = State::from_claims(full);
    let transition = morpholog_core::Transition {
        transformation_name: t.name.clone(),
        args,
        actor: morpholog_test_support::test_actor(),
        decided_at: None,
    };
    let scope = compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        Reads::Body,
    );
    let projected = project(&full, &scope);
    let on_full = prepared
        .stage_delta(&transition, &full, &mut same_subjects())
        .map(|staged| staged.expect("the transformation is declared"));
    let on_projected = prepared
        .stage_delta(&transition, &projected, &mut same_subjects())
        .map(|staged| staged.expect("the transformation is declared"));
    let shown = staged_observable(&on_full);
    assert_eq!(
        shown,
        staged_observable(&on_projected),
        "{fragment}: the projection changed the staged delta (scope: {scope:?})"
    );
    (shown, projected)
}

/// Three held items in stock, two of them in the collection: the loop
/// runs over two, the projection holds two of three, and the act stages.
#[test]
fn a_loop_over_a_collection_parameter_reads_only_its_elements() {
    use morpholog_test_support::{claim_instance, coll, dec, subj};
    let full = vec![
        claim_instance("Held", &[subj("i1")]),
        claim_instance("Held", &[subj("i2")]),
        claim_instance("Held", &[subj("i3")]),
        claim_instance("Stock", &[subj("i1"), dec(5)]),
        claim_instance("Stock", &[subj("i2"), dec(5)]),
        claim_instance("Stock", &[subj("i3"), dec(5)]),
    ];
    let (shown, projected) = stage_over(
        "for_over_parameter",
        vec![subj("t"), coll(vec![subj("i1"), subj("i2")])],
        full.clone(),
    );
    assert!(shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims_for("Stock").count(), 2);
    assert_eq!(projected.claims_for("Held").count(), 2);

    // An empty collection: the loop runs over nothing, the load holds nothing.
    let (shown, projected) = stage_over(
        "for_over_parameter",
        vec![subj("t"), coll(vec![])],
        full.clone(),
    );
    assert!(shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims().len(), 0);

    // A duplicate element: the kernel iterates it twice, the load holds it once.
    let (shown, projected) = stage_over(
        "for_over_parameter",
        vec![subj("t"), coll(vec![subj("i1"), subj("i1")])],
        full,
    );
    assert!(shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims_for("Stock").count(), 1);
}

/// The merge act's own shape: a positive membership test inside a
/// negated existential narrows the pattern beside it.
#[test]
fn a_membership_test_inside_a_negated_existential_keys_its_pattern() {
    use morpholog_test_support::{claim_instance, coll, subj};
    let full = vec![
        claim_instance("Alloc", &[subj("a1"), subj("i1")]),
        claim_instance("Alloc", &[subj("a2"), subj("i2")]),
        claim_instance("Alloc", &[subj("a3"), subj("i3")]),
    ];
    // a1 is listed, so the gate holds; a3's item is outside and never read.
    let (shown, projected) = stage_over(
        "member_in_negated_exists",
        vec![subj("t"), coll(vec![subj("i1")]), coll(vec![subj("a1")])],
        full.clone(),
    );
    assert!(shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims_for("Alloc").count(), 1);
    // a2 is in the collection and unlisted, so the gate refuses, alike.
    let (shown, _) = stage_over(
        "member_in_negated_exists",
        vec![
            subj("t"),
            coll(vec![subj("i1"), subj("i2")]),
            coll(vec![subj("a1")]),
        ],
        full,
    );
    assert!(shown.starts_with("rejected:"), "{shown}");
}

/// A row outside the collection makes a conjunct raise before the
/// membership test is reached. The whole state errors; a load that
/// dropped the row would reject instead. The plan keeps the read whole.
#[test]
fn a_membership_test_behind_a_raising_conjunct_keeps_the_error_observable() {
    use morpholog_test_support::{claim_instance, coll, dec, subj};
    let full = vec![
        claim_instance("Stock", &[subj("outside"), dec(0)]),
        claim_instance("Stock", &[subj("inside"), dec(5)]),
    ];
    let (shown, projected) = stage_over(
        "member_after_raising",
        vec![subj("t"), coll(vec![subj("inside")])],
        full,
    );
    assert!(
        shown.starts_with("error:"),
        "the division by zero must surface: {shown}"
    );
    assert_eq!(projected.claims_for("Stock").count(), 2);
}

/// A `let` in the loop replaces the loop's value, so the read it keys is
/// of the replacement, which the collection never named.
#[test]
fn a_loop_binding_a_let_replaces_reads_through_the_replacement() {
    use morpholog_test_support::{claim_instance, coll, subj};
    let full = vec![claim_instance("Stock", &[subj("outside")])];
    let (shown, projected) = stage_over(
        "loop_binding_rebound_by_let",
        vec![subj("t"), coll(vec![subj("inside")])],
        full,
    );
    assert!(
        shown.starts_with("staged:"),
        "the replaced value finds its row: {shown}"
    );
    assert_eq!(projected.claims_for("Stock").count(), 1);
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
    let prepared = morpholog_test_support::prepare(&program);
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
        decided_at: None,
    };
    let scope = compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        Reads::Body,
    );
    let projected = project(&full, &scope);
    let on_full = prepared
        .stage_delta(&transition, &full, &mut same_subjects())
        .map(|staged| staged.expect("the transformation is declared"));
    let on_projected = prepared
        .stage_delta(&transition, &projected, &mut same_subjects())
        .map(|staged| staged.expect("the transformation is declared"));
    assert!(
        matches!(on_full, Ok(StagedDelta::Staged { .. })),
        "the body reads P through the link: {on_full:?}"
    );
    assert_eq!(
        staged_observable(&on_full),
        staged_observable(&on_projected)
    );
}

/// The chain fixture: two shipments, each through its own shipment and
/// order to its own good customer, and a third item nobody allocated.
pub(crate) fn chain_state() -> Vec<morpholog_core::ClaimInstance> {
    use morpholog_test_support::{claim_instance, dec, subj};
    vec![
        claim_instance("Allocation", &[subj("a1"), subj("s1"), subj("i1")]),
        claim_instance("Allocation", &[subj("a2"), subj("s2"), subj("i2")]),
        claim_instance("Shipment", &[subj("s1"), subj("o1")]),
        claim_instance("Shipment", &[subj("s2"), subj("o2")]),
        claim_instance("Order", &[subj("o1"), subj("c1")]),
        claim_instance("Order", &[subj("o2"), subj("c2")]),
        claim_instance("Good", &[subj("c1")]),
        claim_instance("Good", &[subj("c2")]),
        claim_instance("Stock", &[subj("i1"), dec(5)]),
        claim_instance("Stock", &[subj("i2"), dec(5)]),
        claim_instance("Stock", &[subj("i3"), dec(5)]),
    ]
}

/// Stage `ship(a1)` of the chain fragment over `full` and over its
/// body-only projection; both must show the same, and the projection is
/// returned for the caller to say how small it had to be.
fn ship_a1_over(full: Vec<morpholog_core::ClaimInstance>) -> (String, State) {
    let program = hostile_program("bind_chain");
    let t = &program.transformations[0];
    let prepared = morpholog_test_support::prepare(&program);
    let full = State::from_claims(full);
    let transition = morpholog_core::Transition {
        transformation_name: t.name.clone(),
        args: vec![morpholog_test_support::subj("a1")],
        actor: morpholog_test_support::test_actor(),
        decided_at: None,
    };
    let scope = compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        Reads::Body,
    );
    let projected = project(&full, &scope);
    let on_full = prepared
        .stage_delta(&transition, &full, &mut same_subjects())
        .map(|staged| staged.expect("the transformation is declared"));
    let on_projected = prepared
        .stage_delta(&transition, &projected, &mut same_subjects())
        .map(|staged| staged.expect("the transformation is declared"));
    let shown = staged_observable(&on_full);
    assert_eq!(
        shown,
        staged_observable(&on_projected),
        "the projection changed the staged delta (scope: {scope:?})"
    );
    (shown, projected)
}

/// Every link matches: the projection holds one row per link and the
/// one stock row the allocation names, and the act stages through it.
#[test]
fn a_chain_of_binds_reads_only_the_rows_the_links_name() {
    let (shown, projected) = ship_a1_over(chain_state());
    assert!(shown.starts_with("staged:"), "{shown}");
    let mut rows: Vec<String> = projected
        .claims()
        .iter()
        .map(|c| format!("{} {:?}", c.predicate, c.args))
        .collect();
    rows.sort();
    let wanted = ["Allocation", "Good", "Order", "Shipment", "Stock"];
    assert_eq!(rows.len(), wanted.len(), "{rows:#?}");
    for (row, predicate) in rows.iter().zip(wanted) {
        assert!(row.starts_with(predicate), "{row} is not a {predicate} row");
    }
    assert!(
        rows.iter()
            .any(|r| r.starts_with("Stock") && r.contains("i1")),
        "{rows:#?}"
    );
}

/// A parent matching several rows: every value they hold reaches the
/// load, and the kernel's error is the same over the projection.
#[test]
fn a_parent_matching_several_rows_loads_every_dependant_and_errors_alike() {
    use morpholog_test_support::{claim_instance, subj};
    let mut full = chain_state();
    full.push(claim_instance(
        "Allocation",
        &[subj("a1"), subj("s1"), subj("i2")],
    ));
    let (shown, projected) = ship_a1_over(full);
    assert!(shown.starts_with("error:"), "{shown}");
    assert_eq!(
        projected.claims_for("Stock").count(),
        2,
        "both items' stock"
    );
}

/// The dependent row is absent: the act stops at that bind on both.
#[test]
fn an_absent_dependant_decides_alike() {
    let full: Vec<_> = chain_state()
        .into_iter()
        .filter(|c| !(c.predicate.as_str() == "Stock" && format!("{:?}", c.args).contains("i1")))
        .collect();
    let (shown, projected) = ship_a1_over(full);
    assert!(!shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims_for("Stock").count(), 0);
}

/// Two allocations name one item: the item's row is loaded once, and the
/// act stages.
#[test]
fn two_parents_naming_one_dependant_load_it_once() {
    use morpholog_test_support::{claim_instance, subj};
    let full: Vec<_> = chain_state()
        .into_iter()
        .map(|c| {
            if c.predicate.as_str() == "Allocation" && format!("{:?}", c.args).contains("a2") {
                claim_instance("Allocation", &[subj("a2"), subj("s2"), subj("i1")])
            } else {
                c
            }
        })
        .collect();
    let (shown, projected) = ship_a1_over(full);
    assert!(shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims_for("Stock").count(), 1);
}

/// A refusal after the dependent read: the quantity the chain reached
/// fails the gate on both.
#[test]
fn a_refusal_after_the_dependent_read_decides_alike() {
    use morpholog_test_support::{claim_instance, dec, subj};
    let full: Vec<_> = chain_state()
        .into_iter()
        .map(|c| {
            if c.predicate.as_str() == "Stock" && format!("{:?}", c.args).contains("i1") {
                claim_instance("Stock", &[subj("i1"), dec(0)])
            } else {
                c
            }
        })
        .collect();
    let (shown, _) = ship_a1_over(full);
    assert!(shown.starts_with("rejected:"), "{shown}");
}

/// In an atomic batch, an act after one that admits a predicate a key
/// reads through loses that key and keeps its others; one that only
/// retracts, or admits a predicate off the chain, changes nothing.
#[test]
fn a_key_read_through_an_earlier_admission_is_dropped_and_the_rest_kept() {
    use crate::propose::LoadFilter;
    use std::collections::BTreeSet;
    let program = morpholog_surface::parse_program(
        "program batch_keys
predicate Allocation(a: Subject, item: Subject)
predicate Stock(warehouse: Subject, item: Subject, on_hand: Decimal)
predicate Out(a: Subject)
transformation ship(a, warehouse):
    bind Allocation(a, item)
    bind Stock(warehouse, item, on_hand)
    admit Out(a)
",
    )
    .expect("parses");
    let t = &program.transformations[0];
    let transition = morpholog_core::Transition {
        transformation_name: t.name.clone(),
        args: vec![
            morpholog_test_support::subj("a1"),
            morpholog_test_support::subj("w1"),
        ],
        actor: morpholog_test_support::test_actor(),
        decided_at: None,
    };
    let scope = || {
        compute_load_scope(
            t,
            Some(&transition),
            &program.invariants,
            &program.definitions,
            Reads::Body,
        )
    };
    let stock_positions = |scope: &LoadScope| -> Vec<Vec<usize>> {
        match &scope.filters[&"Stock".into()] {
            LoadFilter::Whole => vec![],
            LoadFilter::Keyed(patterns) => patterns
                .iter()
                .map(|p| p.iter().map(|(i, _)| *i).collect())
                .collect(),
        }
    };
    let untouched = scope();
    assert_eq!(stock_positions(&untouched), vec![vec![0, 1]]);

    let mut after_allocation = scope();
    after_allocation.after_admissions_of(&BTreeSet::from(["Allocation".into()]));
    assert_eq!(
        stock_positions(&after_allocation),
        vec![vec![0]],
        "the item key read through Allocation goes, the warehouse parameter stays"
    );
    assert_eq!(
        after_allocation.filters[&"Allocation".into()],
        untouched.filters[&"Allocation".into()],
        "the parent's own parameter key is untouched"
    );

    let mut after_stock = scope();
    after_stock.after_admissions_of(&BTreeSet::from(["Stock".into()]));
    assert_eq!(
        after_stock, untouched,
        "an admission off the chain changes nothing"
    );

    let mut after_nothing = scope();
    after_nothing.after_admissions_of(&BTreeSet::new());
    assert_eq!(after_nothing, untouched);

    // With no other coordinate, the pattern becomes whole.
    let program = morpholog_surface::parse_program(
        "program batch_keys_alone
predicate Allocation(a: Subject, item: Subject)
predicate Stock(item: Subject, on_hand: Decimal)
predicate Out(a: Subject)
transformation ship(a):
    bind Allocation(a, item)
    bind Stock(item, on_hand)
    admit Out(a)
",
    )
    .expect("parses");
    let t = &program.transformations[0];
    let mut alone = compute_load_scope(
        t,
        Some(&morpholog_core::Transition {
            transformation_name: t.name.clone(),
            args: vec![morpholog_test_support::subj("a1")],
            actor: morpholog_test_support::test_actor(),
            decided_at: None,
        }),
        &program.invariants,
        &program.definitions,
        Reads::Body,
    );
    alone.after_admissions_of(&BTreeSet::from(["Allocation".into()]));
    assert_eq!(alone.filters[&"Stock".into()], LoadFilter::Whole);
}

/// A mixed execution loads what its interpreted invariants read, whole,
/// and nothing for the invariants the database checks or for the admits:
/// the table decides membership there.
#[test]
fn a_mixed_execution_loads_only_what_its_interpreted_invariants_read() {
    let program = morpholog_surface::parse_program(
        "program mixed\n\
         predicate Amount(id: Subject, v: Decimal)\n\
         predicate Big(id: Subject, v: Decimal)\n\
         predicate Seen(id: Subject)\n\
         invariant compiled_one: forall a in Amount(id, v): v <= 10\n\
         invariant interpreted_one: forall b in Big(id, v): v + 1 <= v\n\
         transformation both(id, amount, big):\n\
         \x20   require Seen(id)\n\
         \x20   admit Amount(id, amount)\n\
         \x20   admit Big(id, big)\n",
    )
    .expect("parses");
    let t = &program.transformations[0];
    let transition = morpholog_core::Transition {
        transformation_name: t.name.clone(),
        args: vec![
            morpholog_test_support::subj("a"),
            morpholog_test_support::dec(1),
            morpholog_test_support::dec(1),
        ],
        actor: morpholog_test_support::test_actor(),
        decided_at: None,
    };
    let scope = compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        Reads::BodyAndSome(&[1]),
    );
    let mut predicates: Vec<String> = scope.predicates().iter().map(ToString::to_string).collect();
    predicates.sort();
    assert_eq!(
        predicates,
        ["Big", "Seen"],
        "the body's read and the interpreted invariant's, not the compiled one's: {scope:?}"
    );
    assert_eq!(
        scope.filters.get(&"Big".into()),
        Some(&crate::propose::LoadFilter::Whole)
    );
}

/// The link names one item, the collection another: the loop reads the
/// collection's, and a load keyed by the link's would miss it.
#[test]
fn a_loop_binding_shadowing_a_bind_field_reads_the_elements() {
    use morpholog_test_support::{claim_instance, coll, subj};
    let full = vec![
        claim_instance("Link", &[subj("t"), subj("linked")]),
        claim_instance("Stock", &[subj("i1")]),
    ];
    let (shown, projected) = stage_over(
        "loop_binding_shadows_bind_field",
        vec![subj("t"), coll(vec![subj("i1")])],
        full,
    );
    assert!(shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims_for("Stock").count(), 1);
}

/// The outer loop names one item, the inner loop's computed collection
/// another: the read is of the inner one, which a key from the outer
/// loop would miss.
#[test]
fn a_nested_loop_reusing_the_name_reads_its_own_collection() {
    use morpholog_test_support::{claim_instance, coll, subj};
    let full = vec![
        claim_instance("Items", &[subj("t"), coll(vec![subj("i2")])]),
        claim_instance("Stock", &[subj("i2")]),
    ];
    let (shown, projected) = stage_over(
        "nested_loop_rebinds_the_name",
        vec![subj("t"), coll(vec![subj("i1")])],
        full,
    );
    assert!(shown.starts_with("staged:"), "{shown}");
    assert_eq!(projected.claims_for("Stock").count(), 1);
}

/// A scope with every kind of key: what the plan says it binds is what
/// the loader binds.
#[test]
fn the_parameter_count_is_the_loader_s_bind_count() {
    use crate::propose::{LoadFilter, load_sql};
    use morpholog_core::Key;
    use morpholog_test_support::{dec, subj};
    let mut scope = LoadScope::default();
    scope.filters.insert("W".into(), LoadFilter::Whole);
    scope.filters.insert(
        "K".into(),
        LoadFilter::Keyed(vec![
            vec![(0, Key::Value(subj("a"))), (1, Key::Value(dec(1)))],
            vec![(
                0,
                Key::Field {
                    predicate: "P".into(),
                    position: 1,
                    of: vec![
                        (0, Key::Value(subj("b"))),
                        (
                            1,
                            Key::Field {
                                predicate: "Q".into(),
                                position: 0,
                                of: vec![(0, Key::OneOf(vec![subj("c"), subj("d"), subj("e")]))],
                            },
                        ),
                    ],
                },
            )],
            vec![(0, Key::OneOf(vec![subj("f"), subj("g")]))],
        ]),
    );
    let (_, binds) = load_sql(&scope).unwrap();
    assert_eq!(scope.parameters(), binds.len());
    assert_eq!(binds.len(), 2 + 1 + 3 + 2);
}

/// At the limit nothing changes; one over, the largest element key goes,
/// the first of equals, its pattern keeping its other coordinate; a
/// pattern left with none reads whole.
#[test]
fn the_budget_widens_the_largest_element_key_first_and_only_past_the_limit() {
    use crate::propose::{LoadFilter, PG_BIND_LIMIT};
    use morpholog_core::Key;
    use morpholog_test_support::subj;
    let elements = |n: usize| Key::OneOf((0..n).map(|i| subj(&format!("e{i}"))).collect());
    let big = PG_BIND_LIMIT - 3;
    let at_limit = || {
        let mut scope = LoadScope::default();
        scope.filters.insert(
            "A".into(),
            LoadFilter::Keyed(vec![vec![(0, Key::Value(subj("a"))), (1, elements(2))]]),
        );
        scope.filters.insert(
            "B".into(),
            LoadFilter::Keyed(vec![vec![(0, elements(big))]]),
        );
        scope
    };
    let mut scope = at_limit();
    assert_eq!(scope.parameters(), PG_BIND_LIMIT);
    scope.within_parameter_budget();
    assert_eq!(scope, at_limit(), "at the limit nothing is widened");

    let mut scope = at_limit();
    scope.filters.insert(
        "C".into(),
        LoadFilter::Keyed(vec![vec![(0, Key::Value(subj("c")))]]),
    );
    assert_eq!(scope.parameters(), PG_BIND_LIMIT + 1);
    scope.within_parameter_budget();
    assert_eq!(
        scope.filters[&"B".into()],
        LoadFilter::Whole,
        "the largest element key goes, and its pattern had nothing else"
    );
    assert_eq!(
        scope.filters[&"A".into()],
        LoadFilter::Keyed(vec![vec![(0, Key::Value(subj("a"))), (1, elements(2))]]),
        "the small key stays"
    );
    assert!(scope.parameters() <= PG_BIND_LIMIT);

    // Equals: the first in predicate order goes, and the pattern keeps
    // its value coordinate.
    let mut scope = LoadScope::default();
    for name in ["X", "Y"] {
        scope.filters.insert(
            name.into(),
            LoadFilter::Keyed(vec![vec![
                (0, Key::Value(subj("k"))),
                (1, elements(big / 2 + 2)),
            ]]),
        );
    }
    assert!(scope.parameters() > PG_BIND_LIMIT);
    scope.within_parameter_budget();
    assert_eq!(
        scope.filters[&"X".into()],
        LoadFilter::Keyed(vec![vec![(0, Key::Value(subj("k")))]])
    );
    assert_eq!(
        scope.filters[&"Y".into()],
        LoadFilter::Keyed(vec![vec![
            (0, Key::Value(subj("k"))),
            (1, elements(big / 2 + 2))
        ]])
    );
}

/// Seventy loops over seventy collections of a thousand elements bind
/// seventy thousand parameters before the budget. After it the load fits
/// one statement, admits every row the unwidened plan admitted, and the
/// act decides the same over the projection as over the whole state,
/// including the refusal an omitted row would hide.
#[test]
fn a_load_past_the_limit_widens_conservatively_and_decides_alike() {
    use crate::propose::{LoadFilter, PG_BIND_LIMIT};
    use morpholog_core::{Key, ReadPlan};
    use morpholog_test_support::{claim_instance, coll, subj};
    let n = 70;
    let mut source = String::from("program wide\n");
    for k in 0..n {
        source.push_str(&format!("predicate P{k}(o: Subject)\n"));
    }
    source.push_str("predicate Out(t: Subject)\n");
    let params: Vec<String> = (0..n).map(|k| format!("c{k}")).collect();
    source.push_str(&format!(
        "transformation check(t, {}):\n",
        params.join(", ")
    ));
    for k in 0..n {
        source.push_str(&format!("    for o in c{k}:\n        require P{k}(o)\n"));
    }
    source.push_str("    admit Out(t)\n");
    let program = morpholog_surface::parse_program(&source).expect("parses");
    let t = &program.transformations[0];
    let prepared = morpholog_test_support::prepare(&program);

    // Each collection names a thousand items; the state holds a row for
    // each of them under its predicate, except one row under P69 for the
    // last element, which the gate then refuses on. One more row, under
    // a predicate the budget will read whole, names an item no
    // collection holds: the widened load must carry it and the act must
    // not notice it.
    let elements: Vec<morpholog_core::EvalValue> =
        (0..1000).map(|i| subj(&format!("i{i}"))).collect();
    let stranger = claim_instance("P0", &[subj("stranger")]);
    let mut full = vec![stranger.clone()];
    for k in 0..n {
        for (i, e) in elements.iter().enumerate() {
            if k == n - 1 && i == 999 {
                continue;
            }
            full.push(claim_instance(&format!("P{k}"), std::slice::from_ref(e)));
        }
    }
    let full = State::from_claims(full);
    let mut args = vec![subj("t")];
    args.extend((0..n).map(|_| coll(elements.clone())));
    let transition = morpholog_core::Transition {
        transformation_name: t.name.clone(),
        args,
        actor: morpholog_test_support::test_actor(),
        decided_at: None,
    };

    // The plan before the budget: every read keyed by its elements.
    let plan = ReadPlan::of(t, &program.definitions);
    let mut unwidened = LoadScope::default();
    for (predicate, filter) in &plan.reads {
        let patterns = filter.resolve(t, &transition).expect("every read is keyed");
        unwidened
            .filters
            .insert(predicate.clone(), LoadFilter::Keyed(patterns));
    }
    assert_eq!(unwidened.parameters(), n * 1000);

    let scope = compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        Reads::Body,
    );
    assert!(
        scope.parameters() <= PG_BIND_LIMIT,
        "{}",
        scope.parameters()
    );
    let whole: Vec<String> = scope
        .filters
        .iter()
        .filter(|(_, f)| **f == LoadFilter::Whole)
        .map(|(p, _)| p.to_string())
        .collect();
    assert_eq!(
        whole,
        ["P0", "P1", "P10", "P11", "P12"],
        "the first five of equals in name order, whole"
    );
    assert!(matches!(
        &scope.filters[&"P5".into()],
        LoadFilter::Keyed(patterns) if matches!(&patterns[0][0].1, Key::OneOf(v) if v.len() == 1000)
    ));

    for claim in full.claims().iter() {
        if unwidened.admits(claim, &full) {
            assert!(scope.admits(claim, &full), "widening dropped {claim:?}");
        }
    }
    assert!(
        !unwidened.admits(&stranger, &full) && scope.admits(&stranger, &full),
        "the widened read carries a row the keyed read did not"
    );
    let projected = project(&full, &scope);
    let on_full = prepared
        .stage_delta(&transition, &full, &mut same_subjects())
        .map(|staged| staged.expect("declared"));
    let on_projected = prepared
        .stage_delta(&transition, &projected, &mut same_subjects())
        .map(|staged| staged.expect("declared"));
    let shown = staged_observable(&on_full);
    assert!(shown.starts_with("rejected:"), "{shown}");
    assert_eq!(shown, staged_observable(&on_projected));
}
