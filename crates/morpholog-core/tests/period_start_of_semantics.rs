//! `period_start_of(anchor, span, index)` semantics: the boundary
//! date back from the coordinate - period `index`'s first day,
//! computed by multiplying the span's components ONCE from the anchor,
//! exactly as `period_index` defines boundary n. Inverse round trip
//! wherever the boundary is representable; a boundary outside the
//! calendar refused by name (never clamped); a zero span and a
//! fractional index refused by name at both tiers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    date, dec, eq, params, period_index, period_start_of, require, span, term, transformation,
};
use morpholog_core::{Outcome, Prop, State};
use morpholog_test_support::propose_with_test_actor;

fn holds(prop: Prop) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let outcome =
        propose_with_test_actor(&t, vec![], &State::default(), &[], &[]).expect("evaluates");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "expected the proposition to hold: {outcome:?}"
    );
}

fn start_of(anchor: &str, sp: &str, index: &str, expected: &str) {
    holds(eq(
        period_start_of(term(date(anchor)), term(span(sp)), term(dec(index))),
        term(date(expected)),
    ));
    // The inverse law, checked through the IR for every row: the
    // boundary this row claims is the day period_index flips on.
    holds(eq(
        period_index(term(date(anchor)), term(span(sp)), term(date(expected))),
        term(dec(index)),
    ));
}

#[test]
fn the_boundary_comes_back_from_the_coordinate() {
    // Charging years anchored 1 April: period 0 starts at the anchor
    // itself, period 26 on 1 April 2026 - the day its index flips.
    start_of("2000-04-01", "P1Y", "0", "2000-04-01");
    start_of("2000-04-01", "P1Y", "1", "2001-04-01");
    start_of("2000-04-01", "P1Y", "26", "2026-04-01");
}

#[test]
fn negative_indexes_name_the_periods_before_the_anchor() {
    start_of("2000-04-01", "P1Y", "-1", "1999-04-01");
    start_of("2000-04-01", "P1Y", "-2", "1998-04-01");
}

#[test]
fn a_leap_day_anchor_clamps_each_boundary_from_the_anchor() {
    // Anniversaries of 29 February land on 28 February in common
    // years - and recover the 29th in leap years, because every
    // boundary is computed from the anchor, never from a previously
    // clamped boundary.
    start_of("2000-02-29", "P1Y", "1", "2001-02-28");
    start_of("2000-02-29", "P1Y", "3", "2003-02-28");
    start_of("2000-02-29", "P1Y", "4", "2004-02-29");
}

#[test]
fn a_thirty_first_anchor_with_monthly_spans_stays_anchored_not_iterated() {
    // From a 31 January anchor under P1M, boundary n is
    // anchor + n months clamped ONCE: the March boundary is the 31st
    // again. Iterated clamped hops (Jan 31 -> Feb 29 -> Mar 29) would
    // have drifted and never recovered the month's end.
    start_of("2000-01-31", "P1M", "1", "2000-02-29");
    start_of("2000-01-31", "P1M", "2", "2000-03-31");
    start_of("2000-01-31", "P1M", "13", "2001-02-28");
}

#[test]
fn weekly_and_mixed_spans_answer_the_same_way() {
    start_of("2026-01-05", "P1W", "1", "2026-01-12");
    start_of("2026-01-05", "P7D", "-1", "2025-12-29");
    start_of("2026-01-15", "P1M15D", "1", "2026-03-02");
}

#[test]
fn a_date_inside_a_period_floors_to_its_start_through_the_round_trip() {
    // The composition the worked example leans on: any date's period
    // start is period_start_of over its own period_index - the floor
    // to the governing boundary.
    holds(eq(
        period_start_of(
            term(date("2000-04-01")),
            term(span("P1Y")),
            period_index(
                term(date("2000-04-01")),
                term(span("P1Y")),
                term(date("2026-07-01")),
            ),
        ),
        term(date("2026-04-01")),
    ));
}

#[test]
fn an_index_whose_boundary_leaves_the_calendar_is_refused_not_clamped() {
    // period_index clips its outermost periods - a DATE always
    // belongs somewhere. An INDEX whose boundary is unrepresentable
    // has no honest date to return, so the refusal is by name.
    for index in ["8000", "-12001", "100000000000000000000"] {
        let t = transformation(
            "probe",
            params(&[]),
            vec![require(eq(
                period_start_of(
                    term(date("2000-04-01")),
                    term(span("P1Y")),
                    term(dec(index)),
                ),
                term(date("2000-04-01")),
            ))],
        );
        let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
            .expect_err("an out-of-calendar boundary is an error at evaluation");
        assert!(
            format!("{err}").contains("outside the representable calendar"),
            "index {index}: got {err}"
        );
    }
    // The last representable boundary itself still answers.
    start_of("2000-04-01", "P1Y", "7999", "9999-04-01");
}

#[test]
fn a_literal_zero_span_is_refused_at_validation_by_name() {
    use morpholog_core::ir_builder::{invariant, program};
    let p = program("zero_span")
        .invariants(vec![invariant(
            "z",
            eq(
                period_start_of(term(date("2000-04-01")), term(span("P0D")), term(dec("0"))),
                term(date("2000-04-01")),
            ),
        )])
        .build();
    let errs = p.validate().expect_err("a zero span cannot validate");
    assert!(
        errs.iter()
            .any(|e| format!("{e}").contains("period_start_of needs a positive span; got P0D")),
        "got: {errs:?}"
    );
}

#[test]
fn a_zero_span_through_a_variable_is_refused_at_evaluation_by_name() {
    use morpholog_core::ir_builder::{let_, var};
    let t = transformation(
        "probe",
        params(&[]),
        vec![
            let_("sp", term(span("P0D"))),
            let_(
                "d",
                period_start_of(term(date("2000-04-01")), term(var("sp")), term(dec("0"))),
            ),
            require(eq(term(var("d")), term(date("2000-04-01")))),
        ],
    );
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("a zero span is an error at evaluation");
    assert!(
        format!("{err}").contains("period_start_of needs a positive span"),
        "got: {err}"
    );
}

#[test]
fn a_literal_fractional_index_is_refused_at_validation_by_name() {
    use morpholog_core::ir_builder::{invariant, program};
    let p = program("fractional")
        .invariants(vec![invariant(
            "f",
            eq(
                period_start_of(
                    term(date("2000-04-01")),
                    term(span("P1Y")),
                    term(dec("1.5")),
                ),
                term(date("2001-04-01")),
            ),
        )])
        .build();
    let errs = p
        .validate()
        .expect_err("a fractional index cannot validate");
    assert!(
        errs.iter()
            .any(|e| format!("{e}").contains("whole-number index; got 1.5")),
        "got: {errs:?}"
    );
}

#[test]
fn a_computed_fractional_index_is_refused_at_evaluation_by_name() {
    use morpholog_core::ir_builder::{div, let_, var};
    // The fraction arrives computed, so the literal check cannot see
    // it; the runtime backstop names the refusal.
    let t = transformation(
        "probe",
        params(&[]),
        vec![
            let_("i", div(term(dec("3")), term(dec("2")))),
            let_(
                "d",
                period_start_of(term(date("2000-04-01")), term(span("P1Y")), term(var("i"))),
            ),
            require(eq(term(var("d")), term(date("2001-04-01")))),
        ],
    );
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("a fractional index is an error at evaluation");
    assert!(
        format!("{err}").contains("whole-number index; got 1.5"),
        "got: {err}"
    );
}

#[test]
fn every_slot_kind_mismatch_is_refused() {
    use morpholog_core::ValueExpr;
    use morpholog_core::ir_builder::{invariant, program};
    // One wrong slot per row: decimal anchor, date span, date index -
    // the whole (Date, CalendarSpan, Decimal) contract.
    let cases: Vec<(&str, ValueExpr)> = vec![
        (
            "decimal anchor",
            period_start_of(term(dec("1")), term(span("P1Y")), term(dec("0"))),
        ),
        (
            "date span",
            period_start_of(
                term(date("2000-04-01")),
                term(date("2000-04-02")),
                term(dec("0")),
            ),
        ),
        (
            "date index",
            period_start_of(
                term(date("2000-04-01")),
                term(span("P1Y")),
                term(date("2000-04-02")),
            ),
        ),
    ];
    for (label, expr) in cases {
        let p = program("bad_kinds")
            .invariants(vec![invariant("k", eq(expr, term(date("2000-04-01"))))])
            .build();
        let errs = p.validate().expect_err("a wrong slot kind cannot validate");
        assert!(
            errs.iter()
                .any(|e| format!("{e}").contains("period_start_of")),
            "{label}: expected a period_start_of slot refusal, got {errs:?}"
        );
    }
}

#[test]
fn a_bare_variable_in_each_slot_refines_to_its_kind() {
    use morpholog_core::ir_builder::{assert_, let_, predicate, program, var};
    use morpholog_core::{ParamKind, PredicateArgKind, transformation_param_kinds};
    // The anchor slot refines toward Date, the index slot toward
    // Decimal - and the RESULT is a Date, so the admitted claim's
    // declaration agrees with the computed value.
    let p = program("refines")
        .predicates(vec![predicate("Out").date("starts_on").build()])
        .transformations(vec![transformation(
            "probe",
            params(&["a", "i"]),
            vec![
                let_(
                    "d",
                    period_start_of(term(var("a")), term(span("P1Y")), term(var("i"))),
                ),
                assert_("Out", vec![var("d")]),
            ],
        )])
        .build();
    let validated = p.validated().expect("programme validates");
    let kinds = transformation_param_kinds(&validated, &"probe".into()).expect("kinds resolve");
    for (name, expected) in [
        ("a", PredicateArgKind::Date),
        ("i", PredicateArgKind::Decimal),
    ] {
        let kind = kinds
            .iter()
            .find(|(v, _)| v.as_str() == name)
            .map(|(_, k)| k.clone())
            .expect("is a parameter");
        assert_eq!(
            kind,
            ParamKind::Concrete(expected),
            "`{name}`'s only use is a period_start_of slot"
        );
    }
}

#[test]
fn a_parameter_refined_by_the_span_slot_cannot_escape_the_expression() {
    use morpholog_core::ir_builder::{assert_, let_, predicate, program, var};
    let p = program("escapes")
        .predicates(vec![predicate("Out").date("starts_on").build()])
        .transformations(vec![transformation(
            "probe",
            params(&["sp"]),
            vec![
                let_(
                    "d",
                    period_start_of(term(date("2000-04-01")), term(var("sp")), term(dec("0"))),
                ),
                assert_("Out", vec![var("d")]),
            ],
        )])
        .build();
    let errs = p
        .validate()
        .expect_err("a span-kinded parameter cannot validate");
    assert!(
        errs.iter()
            .any(|e| format!("{e}").contains("no transition argument may carry a span")),
        "got: {errs:?}"
    );
}

#[test]
fn the_round_trip_refuses_in_the_clipped_lowermost_period() {
    // period_index answers for EVERY representable date - its
    // lowermost period is clipped, so this date sits in a period
    // whose own starting boundary is unrepresentable. Composing back
    // therefore refuses rather than returning some wrong date: the
    // anniversary round-trip test is total only where the period's
    // starting boundary is representable.
    let t = transformation(
        "probe",
        params(&[]),
        vec![require(eq(
            period_start_of(
                term(date("0000-04-01")),
                term(span("P1Y")),
                period_index(
                    term(date("0000-04-01")),
                    term(span("P1Y")),
                    term(date("-009999-01-01")),
                ),
            ),
            term(date("-009999-01-01")),
        ))],
    );
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("the clipped period's boundary is unrepresentable");
    assert!(
        format!("{err}").contains("outside the representable calendar"),
        "got: {err}"
    );
}

#[test]
fn the_boundary_form_round_trips_through_the_formatter() {
    use morpholog_core::ir_builder::{invariant, program};
    let p = program("fmt")
        .invariants(vec![invariant(
            "start",
            eq(
                period_start_of(term(date("2000-04-01")), term(span("P1Y")), term(dec("26"))),
                term(date("2026-04-01")),
            ),
        )])
        .build();
    let rendered = morpholog_core::format::format_program(&p);
    assert!(
        rendered.contains("period_start_of(@2000-04-01, span(P1Y), 26)"),
        "{rendered}"
    );
}
