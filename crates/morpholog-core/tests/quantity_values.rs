//! Functional tests for unit-tagged quantities (`Decimal[U]`): the
//! same-unit algebra, scaling, the duration ratio, quantity
//! aggregation with its seed pattern, and the authoring-time rule
//! checks. Expressed as a miniature of the demurrage-settlement model
//! that forced the kind - cargo parcels in tonnes against a vessel's
//! capacity, a daily demurrage amount in USD settled against the time
//! the voyage ran over - so every assertion is a business behaviour,
//! not an operator probe.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    and, assert_, claim, div, duration, implies, invariant, le, mul, params, predicate, program,
    qty, sum, term, transformation, var,
};
use morpholog_core::{
    EvalError, EvalValue, Invariant, Outcome, Program, State, Transformation, ValidationError,
};
use morpholog_test_support::{dur, must_accept, propose_with_test_actor, qty as q, subj};

/// Propose and require a business rejection (not a kernel error).
fn must_reject(t: &Transformation, args: Vec<EvalValue>, pre: State, invariants: &[Invariant]) {
    let outcome = propose_with_test_actor(t, args, &pre, invariants)
        .expect("proposal should evaluate cleanly");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected rejection, got {outcome:?}"
    );
}

/// Propose and require a kernel evaluation error whose message
/// contains every given fragment - the unit must appear in the
/// author-facing diagnosis, never a unit-erased kind.
fn must_error_naming(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    invariants: &[Invariant],
    fragments: &[&str],
) {
    let err = propose_with_test_actor(t, args, pre, invariants)
        .expect_err("expected a kernel evaluation error");
    let msg = format!("{err}");
    assert!(matches!(err, EvalError::TypeMismatch(_)), "got {err:?}");
    for fragment in fragments {
        assert!(msg.contains(fragment), "`{fragment}` not in: {msg}");
    }
}

/// The miniature: a vessel's capacity caps the summed cargo parcels
/// (tonnes); a daily demurrage amount (USD) scaled by the days of
/// excess caps the summed settlements. Setting the rate seeds a
/// zero-amount settlement, the same pattern a duration aggregate
/// uses - the empty sum is decimal, so a unitful aggregate seeds its
/// own zero (see `unseeded_quantity_aggregate_errors_at_evaluation`).
fn mini_demurrage() -> Program {
    program("mini_demurrage")
        .predicates(vec![
            predicate("CargoCapacity")
                .subject("voyage")
                .quantity("cap", "t")
                .build(),
            predicate("CargoParcel")
                .subject("parcel")
                .subject("voyage")
                .quantity("qty", "t")
                .build(),
            predicate("DemurrageRate")
                .subject("voyage")
                .quantity("daily", "USD")
                .build(),
            predicate("TimeOnDemurrage")
                .subject("voyage")
                .duration("excess")
                .build(),
            predicate("Settled")
                .subject("settlement")
                .subject("voyage")
                .quantity("amount", "USD")
                .build(),
        ])
        .invariants(vec![
            invariant(
                "cargo_within_capacity",
                implies(
                    claim("CargoCapacity", vec![var("v"), var("cap")]),
                    le(
                        sum(
                            var("qty"),
                            claim("CargoParcel", vec![var("p"), var("v"), var("qty")]),
                        ),
                        term(var("cap")),
                    ),
                ),
            ),
            invariant(
                "settled_within_due",
                implies(
                    and(vec![
                        claim("DemurrageRate", vec![var("v"), var("daily")]),
                        claim("TimeOnDemurrage", vec![var("v"), var("x")]),
                    ]),
                    le(
                        sum(
                            var("amount"),
                            claim("Settled", vec![var("s"), var("v"), var("amount")]),
                        ),
                        // The due figure: the daily USD amount scaled by
                        // the (dimensionless, exact) count of days the
                        // voyage ran over.
                        mul(
                            term(var("daily")),
                            div(term(var("x")), term(duration("PT24H"))),
                        ),
                    ),
                ),
            ),
        ])
        .transformations(vec![
            transformation(
                "set_capacity",
                params(&["voyage", "seed", "cap"]),
                vec![
                    assert_("CargoCapacity", vec![var("voyage"), var("cap")]),
                    // The same seed pattern as the rate: the tonnes
                    // aggregate must never be the (decimal) empty sum.
                    assert_(
                        "CargoParcel",
                        vec![var("seed"), var("voyage"), qty("0", "t")],
                    ),
                ],
            ),
            transformation(
                "load_parcel",
                params(&["parcel", "voyage", "qty"]),
                vec![assert_(
                    "CargoParcel",
                    vec![var("parcel"), var("voyage"), var("qty")],
                )],
            ),
            transformation(
                "set_rate",
                params(&["voyage", "seed", "daily", "excess"]),
                vec![
                    assert_("DemurrageRate", vec![var("voyage"), var("daily")]),
                    assert_("TimeOnDemurrage", vec![var("voyage"), var("excess")]),
                    // The seed: a zero-amount settlement admitted with
                    // the rate, so the USD aggregate is never the
                    // (decimal) empty sum.
                    assert_("Settled", vec![var("seed"), var("voyage"), qty("0", "USD")]),
                ],
            ),
            transformation(
                "set_rate_unseeded",
                params(&["voyage", "daily", "excess"]),
                vec![
                    assert_("DemurrageRate", vec![var("voyage"), var("daily")]),
                    assert_("TimeOnDemurrage", vec![var("voyage"), var("excess")]),
                ],
            ),
            transformation(
                "settle",
                params(&["settlement", "voyage", "amount"]),
                vec![assert_(
                    "Settled",
                    vec![var("settlement"), var("voyage"), var("amount")],
                )],
            ),
        ])
        .build()
}

#[test]
fn mini_demurrage_programme_validates() {
    let p = mini_demurrage();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
}

#[test]
fn cargo_loads_to_exactly_the_capacity_and_not_a_tonne_more() {
    let p = mini_demurrage();
    let load = p.transformation("load_parcel").unwrap();
    let state = must_accept(
        p.transformation("set_capacity").unwrap(),
        vec![subj("v1"), subj("seed"), q("45000", "t")],
        State::default(),
        &p.invariants,
    );
    let state = must_accept(
        load,
        vec![subj("p1"), subj("v1"), q("30000", "t")],
        state,
        &p.invariants,
    );
    // Loading to the boundary is admissible: the comparison is exact.
    let state = must_accept(
        load,
        vec![subj("p2"), subj("v1"), q("15000", "t")],
        state,
        &p.invariants,
    );
    // One more tonne breaches the capacity invariant.
    must_reject(
        load,
        vec![subj("p3"), subj("v1"), q("1", "t")],
        state,
        &p.invariants,
    );
}

#[test]
fn settlement_caps_at_the_daily_amount_times_the_exact_days_of_excess() {
    let p = mini_demurrage();
    let settle = p.transformation("settle").unwrap();
    // 132 hours over at 25000 USD per day: 132/24 = 5.5 days exactly,
    // so the due figure is 137500 USD - decimal-exact, no floats.
    let state = must_accept(
        p.transformation("set_rate").unwrap(),
        vec![subj("v1"), subj("seed"), q("25000", "USD"), dur("PT132H")],
        State::default(),
        &p.invariants,
    );
    let state = must_accept(
        settle,
        vec![subj("s1"), subj("v1"), q("137500", "USD")],
        state,
        &p.invariants,
    );
    // A cent past the due figure is refused.
    must_reject(
        settle,
        vec![subj("s2"), subj("v1"), q("0.01", "USD")],
        state,
        &p.invariants,
    );
}

#[test]
fn mixed_unit_comparison_names_both_units() {
    let p = mini_demurrage();
    // Settling in tonnes against a USD aggregate: the sum mixes
    // Decimal[USD] (the seed) with Decimal[t], and the error names
    // both labels rather than a unit-erased "quantity".
    let state = must_accept(
        p.transformation("set_rate").unwrap(),
        vec![subj("v1"), subj("seed"), q("25000", "USD"), dur("PT24H")],
        State::default(),
        &p.invariants,
    );
    must_error_naming(
        p.transformation("settle").unwrap(),
        vec![subj("s1"), subj("v1"), q("5", "t")],
        &state,
        &p.invariants,
        &["Decimal[USD]", "Decimal[t]"],
    );
}

#[test]
fn unseeded_quantity_aggregate_errors_at_evaluation() {
    // The landmine the seed pattern exists for: with no settlement
    // rows at all, the sum is the (decimal) empty sum, and comparing
    // it against a USD figure is a kernel type error - not a lawful
    // rejection, and not a silent pass. Admitting the rate without
    // its seed steps on it immediately.
    let p = mini_demurrage();
    must_error_naming(
        p.transformation("set_rate_unseeded").unwrap(),
        vec![subj("v1"), q("25000", "USD"), dur("PT24H")],
        &State::default(),
        &p.invariants,
        &["Decimal[USD]"],
    );
}

#[test]
fn quantity_arithmetic_is_same_unit_only_and_scaling_is_exact() {
    // Authoring-time: the static matrix refuses USD + t outright,
    // with both units in the message.
    let mixed = program("mixed_add")
        .predicates(vec![
            predicate("A").quantity("usd", "USD").build(),
            predicate("B").quantity("tonnes", "t").build(),
        ])
        .invariants(vec![invariant(
            "bad_add",
            implies(
                and(vec![claim("A", vec![var("a")]), claim("B", vec![var("b")])]),
                le(
                    morpholog_core::ir_builder::add(term(var("a")), term(var("b"))),
                    term(var("a")),
                ),
            ),
        )])
        .build();
    let errs = mixed
        .validate()
        .expect_err("mixed-unit Add must not validate");
    let no_rule = errs
        .iter()
        .find(|e| matches!(e, ValidationError::NoArithRule { .. }))
        .expect("expected a NoArithRule error");
    let msg = format!("{no_rule}");
    assert!(
        msg.contains("Decimal[USD]") && msg.contains("Decimal[t]"),
        "the rule refusal names both units: {msg}"
    );

    // Same-unit multiplication is refused too: two USD amounts
    // multiply into no meaningful unit. The static matrix knows it.
    let squared = program("usd_squared")
        .predicates(vec![predicate("A").quantity("usd", "USD").build()])
        .invariants(vec![invariant(
            "bad_mul",
            implies(
                claim("A", vec![var("a")]),
                le(mul(term(var("a")), term(var("a"))), term(var("a"))),
            ),
        )])
        .build();
    assert!(
        squared
            .validate()
            .expect_err("same-unit Mul must not validate")
            .iter()
            .any(|e| matches!(e, ValidationError::NoArithRule { .. })),
        "expected NoArithRule for Decimal[USD] * Decimal[USD]"
    );
}

#[test]
fn same_unit_ratio_is_a_bare_decimal() {
    // utilisation = settled / due, both USD: the borrowing-base shape
    // carried over to quantities. The ratio is dimensionless, so it
    // compares against a bare decimal bound.
    let p = program("utilisation")
        .predicates(vec![
            predicate("Settled").quantity("amount", "USD").build(),
            predicate("Due").quantity("amount", "USD").build(),
        ])
        .invariants(vec![invariant(
            "ratio_at_most_one",
            implies(
                and(vec![
                    claim("Settled", vec![var("s")]),
                    claim("Due", vec![var("d")]),
                ]),
                le(div(term(var("s")), term(var("d"))), term(dec_lit("1"))),
            ),
        )])
        .transformations(vec![
            transformation(
                "set_due",
                params(&["amount"]),
                vec![assert_("Due", vec![var("amount")])],
            ),
            transformation(
                "settle",
                params(&["amount"]),
                vec![assert_("Settled", vec![var("amount")])],
            ),
        ])
        .build();
    assert!(p.validate().is_ok(), "{:?}", p.validate());

    let settle = p.transformation("settle").unwrap();
    let state = must_accept(
        p.transformation("set_due").unwrap(),
        vec![q("1000", "USD")],
        State::default(),
        &p.invariants,
    );
    // Settling the whole due figure is a ratio of exactly 1: admissible.
    let state = must_accept(settle, vec![q("1000", "USD")], state, &p.invariants);
    // A single settlement past the due figure breaches the ratio bound.
    must_reject(settle, vec![q("1000.01", "USD")], state, &p.invariants);
}

/// A bare decimal literal as a `Term` wrapped for value position.
fn dec_lit(s: &str) -> morpholog_core::Term {
    morpholog_core::ir_builder::dec(s)
}
