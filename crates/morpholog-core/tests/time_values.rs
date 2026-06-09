//! Functional tests for the time value kinds (`Timestamp`,
//! `Duration`): the arithmetic matrix, the ordered-comparison
//! domains, duration aggregation, and the authoring-time rule
//! checks. Expressed as a miniature of the laytime model that
//! forced the kinds - a notice instant, a commencement computed by
//! shifting it, counting intervals summed against an allowance -
//! so every assertion is a business behaviour, not an operator
//! probe.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::EvalValue;
use morpholog_core::Outcome;
use morpholog_core::ir_builder::{
    add, and, assert_, bind_one, claim, duration, duration_le, implies, invariant, let_, params,
    predicate, program, sub, sum, term, timestamp_le, transformation, var,
};
use morpholog_core::{
    Definition, EvalError, Invariant, Program, State, Transformation, ValidationError,
};
use morpholog_test_support::{dur, must_accept, propose_with_test_actor, subj, ts};

/// Propose and require a business rejection (not a kernel error).
fn must_reject(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: State,
    invariants: &[Invariant],
    definitions: &[Definition],
) {
    let outcome = propose_with_test_actor(t, args, &pre, invariants, definitions)
        .expect("proposal should evaluate cleanly");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected rejection, got {outcome:?}"
    );
}

/// The miniature: NOR is tendered at an instant; commencement is NOR
/// shifted by a six-hour turn time; counting intervals accumulate
/// against an allowed laytime, seeded at zero when the allowance is
/// set (the empty sum is decimal, so a duration aggregate seeds its
/// own zero - see `unseeded_duration_aggregate_errors_at_evaluation`).
fn mini_laytime() -> Program {
    program("mini_laytime")
        .predicates(vec![
            predicate("NorTendered")
                .subject("voyage")
                .timestamp("at")
                .build(),
            predicate("Commenced")
                .subject("voyage")
                .timestamp("at")
                .build(),
            predicate("CountingInterval")
                .subject("interval")
                .subject("voyage")
                .duration("len")
                .build(),
            predicate("AllowedLaytime")
                .subject("voyage")
                .duration("allowed")
                .build(),
            predicate("Gap").subject("voyage").duration("len").build(),
        ])
        .invariants(vec![
            invariant(
                "commencement_not_before_nor",
                implies(
                    and(vec![
                        claim("Commenced", vec![var("v"), var("c")]),
                        claim("NorTendered", vec![var("v"), var("n")]),
                    ]),
                    timestamp_le(term(var("n")), term(var("c"))),
                ),
            ),
            invariant(
                "counted_within_allowance",
                implies(
                    claim("AllowedLaytime", vec![var("v"), var("a")]),
                    duration_le(
                        sum(
                            var("len"),
                            claim("CountingInterval", vec![var("i"), var("v"), var("len")]),
                        ),
                        term(var("a")),
                    ),
                ),
            ),
        ])
        .transformations(vec![
            transformation(
                "tender_nor",
                params(&["voyage", "at"]),
                vec![assert_("NorTendered", vec![var("voyage"), var("at")])],
            ),
            transformation(
                "commence_after_turn",
                params(&["voyage"]),
                vec![
                    bind_one(claim("NorTendered", vec![var("voyage"), var("n")])),
                    let_("c", add(term(var("n")), term(duration("PT6H")))),
                    assert_("Commenced", vec![var("voyage"), var("c")]),
                ],
            ),
            transformation(
                "commence_at",
                params(&["voyage", "at"]),
                vec![assert_("Commenced", vec![var("voyage"), var("at")])],
            ),
            transformation(
                "set_allowance",
                params(&["voyage", "seed", "allowed"]),
                vec![
                    assert_("AllowedLaytime", vec![var("voyage"), var("allowed")]),
                    // The seed: a zero-length interval admitted with the
                    // allowance, so the duration aggregate is never the
                    // (decimal) empty sum.
                    assert_(
                        "CountingInterval",
                        vec![var("seed"), var("voyage"), duration("PT0S")],
                    ),
                ],
            ),
            transformation(
                "record_interval",
                params(&["interval", "voyage", "len"]),
                vec![assert_(
                    "CountingInterval",
                    vec![var("interval"), var("voyage"), var("len")],
                )],
            ),
            transformation(
                "measure_gap",
                params(&["voyage", "from", "to"]),
                vec![
                    let_("g", sub(term(var("to")), term(var("from")))),
                    assert_("Gap", vec![var("voyage"), var("g")]),
                ],
            ),
        ])
        .build()
}

#[test]
fn mini_laytime_programme_validates() {
    let p = mini_laytime();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
}

#[test]
fn commencement_is_the_notice_instant_shifted_by_the_turn_time() {
    let p = mini_laytime();
    let state = must_accept(
        p.transformation("tender_nor").unwrap(),
        vec![subj("v1"), ts("2026-10-24T14:00:00Z")],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    let state = must_accept(
        p.transformation("commence_after_turn").unwrap(),
        vec![subj("v1")],
        state,
        &p.invariants,
        &p.definitions,
    );
    assert!(
        state.claims().iter().any(|c| {
            c.predicate.as_str() == "Commenced" && c.args[1] == ts("2026-10-24T20:00:00Z")
        }),
        "commencement should be NOR + PT6H; claims: {:?}",
        state.claims()
    );
}

#[test]
fn commencement_before_the_notice_violates_the_ordering_invariant() {
    let p = mini_laytime();
    let state = must_accept(
        p.transformation("tender_nor").unwrap(),
        vec![subj("v1"), ts("2026-10-24T14:00:00Z")],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    // At the notice instant exactly: at_or_before is inclusive.
    let state = must_accept(
        p.transformation("commence_at").unwrap(),
        vec![subj("v1"), ts("2026-10-24T14:00:00Z")],
        state.clone(),
        &p.invariants,
        &p.definitions,
    );
    let _ = state;

    let state = must_accept(
        p.transformation("tender_nor").unwrap(),
        vec![subj("v2"), ts("2026-10-24T14:00:00Z")],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    must_reject(
        p.transformation("commence_at").unwrap(),
        vec![subj("v2"), ts("2026-10-24T13:59:59Z")],
        state,
        &p.invariants,
        &p.definitions,
    );
}

#[test]
fn counting_intervals_accumulate_against_the_allowance() {
    let p = mini_laytime();
    let state = must_accept(
        p.transformation("set_allowance").unwrap(),
        vec![subj("v1"), subj("seed"), dur("PT6H")],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    let state = must_accept(
        p.transformation("record_interval").unwrap(),
        vec![subj("i1"), subj("v1"), dur("PT3H")],
        state,
        &p.invariants,
        &p.definitions,
    );
    // PT3H + PT2H30M = PT5H30M, inside the PT6H allowance.
    let state = must_accept(
        p.transformation("record_interval").unwrap(),
        vec![subj("i2"), subj("v1"), dur("PT2H30M")],
        state,
        &p.invariants,
        &p.definitions,
    );
    // One more hour would make PT6H30M: the candidate state breaks the
    // invariant, so the whole proposal is refused.
    must_reject(
        p.transformation("record_interval").unwrap(),
        vec![subj("i3"), subj("v1"), dur("PT1H")],
        state,
        &p.invariants,
        &p.definitions,
    );
}

#[test]
fn the_gap_between_two_instants_is_a_duration() {
    let p = mini_laytime();
    let state = must_accept(
        p.transformation("measure_gap").unwrap(),
        vec![
            subj("v1"),
            ts("2026-10-24T14:00:00Z"),
            ts("2026-10-24T17:30:00Z"),
        ],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    assert!(
        state
            .claims()
            .iter()
            .any(|c| c.predicate.as_str() == "Gap" && c.args[1] == dur("PT3H30M")),
        "gap should be PT3H30M; claims: {:?}",
        state.claims()
    );
}

#[test]
fn unseeded_duration_aggregate_errors_at_evaluation() {
    // The empty sum is decimal zero - the only choice that keeps every
    // pre-existing decimal aggregate working - so a duration aggregate
    // whose body matches nothing compares decimal-to-duration and the
    // kernel refuses with a type error rather than guessing. This is
    // why `set_allowance` seeds a zero-length interval; the test pins
    // the landmine the seed defuses.
    let p = mini_laytime();
    let unseeded = transformation(
        "set_allowance_unseeded",
        params(&["voyage", "allowed"]),
        vec![assert_(
            "AllowedLaytime",
            vec![var("voyage"), var("allowed")],
        )],
    );
    let err = propose_with_test_actor(
        &unseeded,
        vec![subj("v1"), dur("PT6H")],
        &State::default(),
        &p.invariants,
        &p.definitions,
    )
    .expect_err("empty duration aggregate must surface a kernel error");
    assert!(
        matches!(&err, EvalError::TypeMismatch(m) if m.contains("duration")),
        "expected a duration type error, got: {err:?}"
    );
}

#[test]
fn adding_two_timestamps_is_refused_at_authoring_time() {
    let p = program("bad_arith")
        .predicates(vec![
            predicate("E")
                .subject("v")
                .timestamp("a")
                .timestamp("b")
                .build(),
        ])
        .invariants(vec![invariant(
            "nonsense",
            implies(
                claim("E", vec![var("v"), var("a"), var("b")]),
                duration_le(add(term(var("a")), term(var("b"))), term(duration("PT1H"))),
            ),
        )])
        .build();
    let errs = p.validate().expect_err("two timestamps cannot be added");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::NoArithRule { operator: "+", .. })),
        "expected NoArithRule, got {errs:?}"
    );
}

#[test]
fn a_parameter_used_only_in_time_arithmetic_infers_its_forced_kind() {
    // `turn_time` appears in no claim position - only as the right
    // operand of `tendered_at + turn_time`. The matrix has exactly one
    // rule for Timestamp + _, so the parameter resolves to Duration
    // and its schema is honest (review feedback on the time-arc PR:
    // stage 2 will externalise exactly this kind of parameter).
    use morpholog_core::transformation_param_kinds;
    use morpholog_core::{ParamKind, PredicateArgKind};

    let p = program("turnable")
        .predicates(vec![
            predicate("Nor").subject("voyage").timestamp("at").build(),
            predicate("Commenced")
                .subject("voyage")
                .timestamp("at")
                .build(),
        ])
        .transformations(vec![transformation(
            "commence_with_turn_time",
            params(&["voyage", "turn_time"]),
            vec![
                bind_one(claim("Nor", vec![var("voyage"), var("tendered_at")])),
                let_("c", add(term(var("tendered_at")), term(var("turn_time")))),
                assert_("Commenced", vec![var("voyage"), var("c")]),
            ],
        )])
        .build();
    let validated = p.validated().expect("programme validates");
    let kinds = transformation_param_kinds(&validated, &"commence_with_turn_time".into()).unwrap();
    let turn_time = kinds
        .iter()
        .find(|(v, _)| v.as_str() == "turn_time")
        .map(|(_, k)| k.clone())
        .expect("turn_time is a parameter");
    assert_eq!(
        turn_time,
        ParamKind::Concrete(PredicateArgKind::Duration),
        "the matrix forces Duration for Timestamp + _"
    );

    // And it works at runtime end to end.
    let state = must_accept(
        p.transformation("commence_with_turn_time").unwrap(),
        vec![subj("v1"), dur("PT6H")],
        {
            let pre = program("seed")
                .predicates(vec![predicate("Nor").subject("v").timestamp("at").build()])
                .build();
            let _ = pre;
            // Admit the NOR through a tiny setup transformation.
            let setup = transformation(
                "tender",
                params(&["voyage", "at"]),
                vec![assert_("Nor", vec![var("voyage"), var("at")])],
            );
            must_accept(
                &setup,
                vec![subj("v1"), ts("2026-10-24T14:00:00Z")],
                State::default(),
                &[],
                &[],
            )
        },
        &[],
        &[],
    );
    assert!(
        state
            .claims()
            .iter()
            .any(|c| c.predicate.as_str() == "Commenced"
                && c.args[1] == ts("2026-10-24T20:00:00Z")),
        "turn time applied: {:?}",
        state.claims()
    );
}

#[test]
fn the_remaining_matrix_arms_evaluate() {
    // Timestamp - Duration (shift an instant backwards) and
    // Min(Duration, Duration) (the floor's sibling) - the two rule
    // arms no other test reaches.
    use morpholog_core::ir_builder::min;

    let p = program("arms")
        .predicates(vec![
            predicate("Back").subject("v").timestamp("at").build(),
            predicate("Shorter").subject("v").duration("d").build(),
        ])
        .transformations(vec![
            transformation(
                "shift_back",
                params(&["v", "at"]),
                vec![
                    let_("earlier", sub(term(var("at")), term(duration("PT2H")))),
                    assert_("Back", vec![var("v"), var("earlier")]),
                ],
            ),
            transformation(
                "cap_below",
                params(&["v", "a", "b"]),
                vec![
                    let_("m", min(term(var("a")), term(var("b")))),
                    assert_("Shorter", vec![var("v"), var("m")]),
                ],
            ),
        ])
        .build();
    assert!(p.validate().is_ok(), "{:?}", p.validate());

    let state = must_accept(
        p.transformation("shift_back").unwrap(),
        vec![subj("v1"), ts("2026-10-24T14:00:00Z")],
        State::default(),
        &[],
        &[],
    );
    assert!(
        state
            .claims()
            .iter()
            .any(|c| c.args[1] == ts("2026-10-24T12:00:00Z")),
        "instant shifted backwards: {:?}",
        state.claims()
    );

    let state = must_accept(
        p.transformation("cap_below").unwrap(),
        vec![subj("v1"), dur("PT5H"), dur("PT3H")],
        State::default(),
        &[],
        &[],
    );
    assert!(
        state.claims().iter().any(|c| c.args[1] == dur("PT3H")),
        "min picks the shorter span: {:?}",
        state.claims()
    );
}

#[test]
fn a_reversed_instant_difference_is_a_negative_span() {
    // Timestamp subtraction is signed: earlier - later is negative,
    // and the duration max floor is what models clamp with (the
    // laytime excess does exactly that). Pinned so the sign semantics
    // are a documented contract, not an accident.
    let p = program("signed")
        .predicates(vec![predicate("Gap").subject("v").duration("d").build()])
        .transformations(vec![transformation(
            "measure",
            params(&["v", "from", "to"]),
            vec![
                let_("g", sub(term(var("to")), term(var("from")))),
                assert_("Gap", vec![var("v"), var("g")]),
            ],
        )])
        .build();
    let state = must_accept(
        p.transformation("measure").unwrap(),
        vec![
            subj("v1"),
            ts("2026-10-24T14:00:00Z"),
            ts("2026-10-24T12:00:00Z"),
        ],
        State::default(),
        &[],
        &[],
    );
    assert!(
        state.claims().iter().any(|c| c.args[1] == dur("-PT2H")),
        "reversed difference is negative: {:?}",
        state.claims()
    );
}
