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
use morpholog_core::{EvalError, Invariant, Program, State, Transformation, ValidationError};
use morpholog_test_support::{dur, must_accept, propose_with_test_actor, subj, ts};

/// Propose and require a business rejection (not a kernel error).
fn must_reject(t: &Transformation, args: Vec<EvalValue>, pre: State, invariants: &[Invariant]) {
    let outcome = propose_with_test_actor(t, args, &pre, invariants)
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
    );
    let state = must_accept(
        p.transformation("commence_after_turn").unwrap(),
        vec![subj("v1")],
        state,
        &p.invariants,
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
    );
    // At the notice instant exactly: at_or_before is inclusive.
    let state = must_accept(
        p.transformation("commence_at").unwrap(),
        vec![subj("v1"), ts("2026-10-24T14:00:00Z")],
        state.clone(),
        &p.invariants,
    );
    let _ = state;

    let state = must_accept(
        p.transformation("tender_nor").unwrap(),
        vec![subj("v2"), ts("2026-10-24T14:00:00Z")],
        State::default(),
        &p.invariants,
    );
    must_reject(
        p.transformation("commence_at").unwrap(),
        vec![subj("v2"), ts("2026-10-24T13:59:59Z")],
        state,
        &p.invariants,
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
    );
    let state = must_accept(
        p.transformation("record_interval").unwrap(),
        vec![subj("i1"), subj("v1"), dur("PT3H")],
        state,
        &p.invariants,
    );
    // PT3H + PT2H30M = PT5H30M, inside the PT6H allowance.
    let state = must_accept(
        p.transformation("record_interval").unwrap(),
        vec![subj("i2"), subj("v1"), dur("PT2H30M")],
        state,
        &p.invariants,
    );
    // One more hour would make PT6H30M: the candidate state breaks the
    // invariant, so the whole proposal is refused.
    must_reject(
        p.transformation("record_interval").unwrap(),
        vec![subj("i3"), subj("v1"), dur("PT1H")],
        state,
        &p.invariants,
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
