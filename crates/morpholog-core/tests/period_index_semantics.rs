//! `period_index(anchor, span, at)` semantics: the unique n with
//! `anchor + n*span <= at < anchor + (n+1)*span`, the nth boundary
//! computed by multiplying the span's components ONCE - never n
//! repeated clamped hops, whose drift #283 pinned. Negative before
//! the anchor, total over representable dates, and a zero span
//! refused by name at both tiers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    date, dec, eq, params, period_index, require, span, term, transformation,
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

fn index_of(anchor: &str, sp: &str, at: &str, expected: &str) {
    holds(eq(
        period_index(term(date(anchor)), term(span(sp)), term(date(at))),
        term(dec(expected)),
    ));
    // The defining property, checked through the IR for every row:
    // boundary(n) <= at < boundary(n+1), with an unrepresentable
    // boundary standing in as the clipped infinity.
    let parsed = morpholog_core::calendar::parse_calendar_span(sp).expect("test span parses");
    let n: i64 = expected.parse().expect("test index parses");
    for (k, upper) in [(n, false), (n + 1, true)] {
        let months = k * i64::from(parsed.months);
        let days = k * i64::from(parsed.days);
        let magnitude = format!("P{}M{}D", months.abs(), days.abs());
        let boundary = if k >= 0 {
            morpholog_core::ir_builder::add(term(date(anchor)), term(span(&magnitude)))
        } else {
            morpholog_core::ir_builder::sub(term(date(anchor)), term(span(&magnitude)))
        };
        let prop = if upper {
            // boundary(n+1) > at, i.e. NOT (boundary <= at).
            morpholog_core::ir_builder::not(morpholog_core::ir_builder::date_le(
                boundary,
                term(date(at)),
            ))
        } else {
            morpholog_core::ir_builder::date_le(boundary, term(date(at)))
        };
        let t = transformation("bound", params(&[]), vec![require(prop)]);
        match propose_with_test_actor(&t, vec![], &State::default(), &[], &[]) {
            // The boundary shift left the calendar. The clipping is
            // DIRECTIONAL: a negative multiple escapes below and
            // reads as negative infinity (lawful only for the lower
            // bound), a positive multiple escapes above and reads as
            // positive infinity (lawful only for the upper bound).
            // Accepting either direction blindly would bless exactly
            // the clipping mistakes this oracle exists to catch.
            Err(err) => {
                assert!(
                    format!("{err}").contains("leaves the calendar"),
                    "boundary({k}) for anchor {anchor} span {sp}: {err}"
                );
                let lawful = if upper { k > 0 } else { k < 0 };
                assert!(
                    lawful,
                    "boundary({k}) escaped the calendar in the wrong direction for the \
                     {} bound (anchor {anchor} span {sp} at {at})",
                    if upper { "upper" } else { "lower" }
                );
            }
            Ok(outcome) => assert!(
                matches!(outcome, Outcome::Accepted { .. }),
                "boundary({k}) property failed for anchor {anchor} span {sp} at {at}: {outcome:?}"
            ),
        }
    }
}

#[test]
fn the_first_period_is_index_zero_and_the_boundary_day_increments() {
    // Charging years anchored 1 April: 31 March sits in the OLD year,
    // 1 April opens the new one - the increment lands ON the
    // anniversary, not after it.
    index_of("2000-04-01", "P1Y", "2000-04-01", "0");
    index_of("2000-04-01", "P1Y", "2001-03-31", "0");
    index_of("2000-04-01", "P1Y", "2001-04-01", "1");
    index_of("2000-04-01", "P1Y", "2026-03-31", "25");
    index_of("2000-04-01", "P1Y", "2026-04-01", "26");
}

#[test]
fn dates_before_the_anchor_take_negative_indexes() {
    index_of("2000-04-01", "P1Y", "2000-03-31", "-1");
    index_of("2000-04-01", "P1Y", "1999-04-01", "-1");
    index_of("2000-04-01", "P1Y", "1999-03-31", "-2");
}

#[test]
fn a_leap_day_anchor_clamps_each_boundary_from_the_anchor() {
    // Anniversaries of 29 February land on 28 February in common
    // years - each boundary computed from the anchor, so the leap-day
    // identity is never lost to an intermediate clamp.
    index_of("2000-02-29", "P1Y", "2001-02-27", "0");
    index_of("2000-02-29", "P1Y", "2001-02-28", "1");
    index_of("2000-02-29", "P1Y", "2004-02-28", "3");
    index_of("2000-02-29", "P1Y", "2004-02-29", "4");
}

#[test]
fn a_monthly_anchor_subsumes_same_calendar_month() {
    // Monthly periods from a first-of-month anchor: two dates share a
    // calendar month exactly when their indexes agree.
    index_of("2000-01-01", "P1M", "2026-07-01", "318");
    index_of("2000-01-01", "P1M", "2026-07-31", "318");
    index_of("2000-01-01", "P1M", "2026-08-01", "319");
}

#[test]
fn a_thirty_first_anchor_with_monthly_spans_stays_anchored_not_iterated() {
    // Boundaries from a 31 January anchor under P1M: each is
    // anchor + n months (clamped per occurrence), so the March
    // boundary is the 31st again - the iterated-hops answer
    // (Jan 31 -> Feb 28 -> Mar 28) would have drifted.
    index_of("2000-01-31", "P1M", "2000-02-28", "0");
    index_of("2000-01-31", "P1M", "2000-02-29", "1");
    index_of("2000-01-31", "P1M", "2000-03-30", "1");
    index_of("2000-01-31", "P1M", "2000-03-31", "2");
}

#[test]
fn weekly_and_mixed_spans_index_the_same_way() {
    index_of("2026-01-05", "P1W", "2026-01-11", "0");
    index_of("2026-01-05", "P1W", "2026-01-12", "1");
    index_of("2026-01-15", "P1M15D", "2026-02-28", "0");
    index_of("2026-01-15", "P1M15D", "2026-03-02", "1");
}

#[test]
fn the_calendar_ends_stay_total() {
    // No out-of-range error can escape: boundaries beyond either end
    // of the calendar only bound the search.
    index_of("2000-04-01", "P1Y", "9999-12-31", "7999");
    index_of("2000-04-01", "P1Y", "-009999-01-01", "-12000");
    index_of("9999-01-01", "P1Y", "-009999-01-01", "-19998");
}

#[test]
fn the_outermost_periods_are_clipped_to_the_calendar() {
    // The reviewer's reproducer: the first representable anniversary
    // in year -9999 is AFTER the position, and the boundary before it
    // is unrepresentable - the clipped contract reads that boundary
    // as negative infinity, so the outermost period still answers.
    index_of("0000-04-01", "P1Y", "-009999-01-01", "-10000");
    // And a span so large that adjacent boundaries both leave the
    // calendar partitions dates into exactly two clipped periods:
    // -1 before the anchor, 0 from the anchor onward.
    index_of("2000-04-01", "P800000M", "9999-12-31", "0");
    index_of("2000-04-01", "P800000M", "-009999-01-01", "-1");
}

#[test]
fn day_only_spans_floor_toward_negative_infinity() {
    // The truncation-toward-zero mistake would answer 0 here.
    index_of("2026-01-05", "P7D", "2026-01-04", "-1");
    index_of("2026-01-05", "P1D", "2026-01-04", "-1");
    // Exactly one week before the anchor is the previous period's
    // own boundary day.
    index_of("2026-01-05", "P7D", "2025-12-29", "-1");
}

#[test]
fn a_literal_zero_span_is_refused_at_validation_by_name() {
    use morpholog_core::ir_builder::{invariant, program};
    let p = program("zero_span")
        .invariants(vec![invariant(
            "z",
            eq(
                period_index(
                    term(date("2000-04-01")),
                    term(span("P0D")),
                    term(date("2000-04-02")),
                ),
                term(dec("0")),
            ),
        )])
        .build();
    let errs = p.validate().expect_err("a zero span cannot validate");
    assert!(
        errs.iter()
            .any(|e| format!("{e}").contains("positive span; got P0D")),
        "got: {errs:?}"
    );
    // The multi-component spelling normalises to the same zero.
    let p = program("zero_span_long")
        .invariants(vec![invariant(
            "z",
            eq(
                period_index(
                    term(date("2000-04-01")),
                    term(span("P0Y0M0D")),
                    term(date("2000-04-02")),
                ),
                term(dec("0")),
            ),
        )])
        .build();
    // The diagnostic carries the NORMALISED span - the parsed value's
    // own face, not the author's spelling.
    let errs = p.validate().expect_err("P0Y0M0D normalises to zero");
    assert!(
        errs.iter()
            .any(|e| format!("{e}").contains("positive span; got P0D")),
        "got: {errs:?}"
    );
}

#[test]
fn a_zero_span_through_a_variable_is_refused_at_evaluation_by_name() {
    use morpholog_core::ir_builder::var;
    // The span arrives through a binding, so the literal-zero
    // validation cannot see it; the runtime backstop names the
    // refusal.
    let t = transformation(
        "probe",
        params(&[]),
        vec![
            morpholog_core::ir_builder::let_("sp", term(span("P0D"))),
            morpholog_core::ir_builder::let_(
                "n",
                period_index(
                    term(date("2000-04-01")),
                    term(var("sp")),
                    term(date("2000-05-01")),
                ),
            ),
            require(eq(term(var("n")), term(dec("0")))),
        ],
    );
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("a zero span is an error at evaluation");
    assert!(format!("{err}").contains("positive span"), "got: {err}");
}

#[test]
fn every_slot_kind_mismatch_is_refused() {
    use morpholog_core::ValueExpr;
    use morpholog_core::ir_builder::{invariant, program};
    // One wrong slot per row: decimal anchor, date span, decimal
    // position - the whole (Date, CalendarSpan, Date) contract.
    let cases: Vec<(&str, ValueExpr)> = vec![
        (
            "decimal anchor",
            period_index(term(dec("1")), term(span("P1Y")), term(date("2000-04-02"))),
        ),
        (
            "date span",
            period_index(
                term(date("2000-04-01")),
                term(date("2000-04-02")),
                term(date("2000-04-03")),
            ),
        ),
        (
            "decimal position",
            period_index(term(date("2000-04-01")), term(span("P1Y")), term(dec("1"))),
        ),
    ];
    for (label, expr) in cases {
        let p = program("bad_kinds")
            .invariants(vec![invariant("k", eq(expr, term(dec("0"))))])
            .build();
        let errs = p.validate().expect_err("a wrong slot kind cannot validate");
        assert!(
            errs.iter().any(|e| format!("{e}").contains("period_index")),
            "{label}: expected a period_index slot refusal, got {errs:?}"
        );
    }
}

#[test]
fn a_bare_variable_in_each_slot_refines_to_its_kind() {
    use morpholog_core::ir_builder::{assert_, let_, predicate, program, var};
    use morpholog_core::{ParamKind, PredicateArgKind, transformation_param_kinds};
    // Parameters whose only use is a date slot land on Date: each
    // slot refines a bare variable toward its contract.
    let p = program("refines")
        .predicates(vec![predicate("Out").decimal("v").build()])
        .transformations(vec![transformation(
            "probe",
            params(&["a", "at"]),
            vec![
                let_(
                    "n",
                    period_index(term(var("a")), term(span("P1Y")), term(var("at"))),
                ),
                assert_("Out", vec![var("n")]),
            ],
        )])
        .build();
    let validated = p.validated().expect("programme validates");
    let kinds = transformation_param_kinds(&validated, &"probe".into()).expect("kinds resolve");
    for name in ["a", "at"] {
        let kind = kinds
            .iter()
            .find(|(v, _)| v.as_str() == name)
            .map(|(_, k)| k.clone())
            .expect("is a parameter");
        assert_eq!(
            kind,
            ParamKind::Concrete(PredicateArgKind::Date),
            "`{name}` is used only in a period_index date slot"
        );
    }
}

#[test]
fn a_parameter_refined_by_the_span_slot_cannot_escape_the_expression() {
    use morpholog_core::ir_builder::{assert_, let_, predicate, program, var};
    // The span slot refines a bare parameter to CalendarSpan - and a
    // CalendarSpan parameter has no lawful argument vector, so the
    // programme is refused at the boundary, not at runtime.
    let p = program("escapes")
        .predicates(vec![predicate("Out").decimal("v").build()])
        .transformations(vec![transformation(
            "probe",
            params(&["sp"]),
            vec![
                let_(
                    "n",
                    period_index(
                        term(date("2000-04-01")),
                        term(var("sp")),
                        term(date("2026-01-01")),
                    ),
                ),
                assert_("Out", vec![var("n")]),
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
fn the_extractor_round_trips_through_the_formatter() {
    use morpholog_core::ir_builder::{invariant, program};
    let p = program("fmt")
        .invariants(vec![invariant(
            "idx",
            eq(
                period_index(
                    term(date("2000-04-01")),
                    term(span("P1Y")),
                    term(date("2026-07-01")),
                ),
                term(dec("26")),
            ),
        )])
        .build();
    let rendered = morpholog_core::format::format_program(&p);
    assert!(
        rendered.contains("period_index(@2000-04-01, span(P1Y), @2026-07-01)"),
        "{rendered}"
    );
}

/// Arity belongs to the builtin and is total. The surface fixes the
/// count per call form, so only hand-built IR can get it wrong -
/// validation refuses it by name, and the evaluator keeps its own
/// backstop for IR that never went through validation.
#[test]
fn a_builtin_called_with_the_wrong_arity_is_refused_at_both_tiers() {
    use morpholog_core::ir_builder::{call, invariant, program};
    use morpholog_core::{Builtin, EvalValue};

    let p = program("bad_arity")
        .invariants(vec![invariant(
            "k",
            eq(call(Builtin::Round, vec![term(dec("1"))]), term(dec("0"))),
        )])
        .build();
    let errs = p.validate().expect_err("one argument is not two");
    assert!(
        errs.iter()
            .any(|e| format!("{e}").contains("round takes 2 argument(s), got 1")),
        "got: {errs:?}"
    );

    // The evaluator's own guard, for IR that skipped validation.
    let err = morpholog_core::eval_builtin_for_test(
        Builtin::PeriodIndex,
        &[EvalValue::Decimal(1.into())],
    )
    .expect_err("three arguments are required");
    assert!(
        format!("{err}").contains("period_index takes 3 argument(s), got 1"),
        "got: {err}"
    );
}

/// `min`/`max` over the ordered kinds, evaluated - not merely accepted.
/// The trap this guards is a static domain wider than the evaluator's:
/// a programme that validates and then fails at runtime is worse than
/// one refused at authoring time.
#[test]
fn min_and_max_compute_over_every_ordered_kind() {
    use morpholog_core::ir_builder::{max, min};

    // Dates: the earlier and later of two.
    holds(eq(
        min(term(date("2026-04-01")), term(date("2026-03-31"))),
        term(date("2026-03-31")),
    ));
    holds(eq(
        max(term(date("2026-04-01")), term(date("2026-03-31"))),
        term(date("2026-04-01")),
    ));
    // Decimals keep working, both directions.
    holds(eq(min(term(dec("5")), term(dec("3"))), term(dec("3"))));
    holds(eq(max(term(dec("5")), term(dec("3"))), term(dec("5"))));
}
