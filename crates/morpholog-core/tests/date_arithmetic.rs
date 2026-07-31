//! Civil-date arithmetic semantics: a calendar span shifts a date
//! (months first, day clamped to the destination month, then days),
//! and the difference of two dates is their signed count of actual
//! days. Pinned as tables, the clamping traps included - the shift is
//! neither reversible nor associative around clamped month ends, and
//! the non-associativity case is pinned on purpose. Plus the refusals:
//! every combination the matrix deliberately leaves out stays a type
//! error, and a span is expression-only - the storage and wire
//! boundaries each refuse it by name.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    add, assert_, date, dec, eq, params, require, span, sub, term, transformation, var,
};
use morpholog_core::{Outcome, Prop, State, ValueExpr};
use morpholog_test_support::{cal_span, propose_with_test_actor, test_actor};

fn holds(prop: Prop) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let outcome = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect("evaluates cleanly");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "expected the proposition to hold exactly: {outcome:?}"
    );
}

fn refuses(prop: Prop, fragments: &[&str]) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("expected a kernel error");
    let rendered = format!("{err}");
    for fragment in fragments {
        assert!(
            rendered.contains(fragment),
            "expected error containing {fragment:?}, got: {rendered}"
        );
    }
}

fn shifted(base: &str, span_text: &str) -> ValueExpr {
    add(term(date(base)), term(span(span_text)))
}

#[test]
fn a_span_shifts_a_date_months_then_days() {
    for (base, sp, expected) in [
        ("2026-03-15", "P3M", "2026-06-15"),
        ("2026-03-15", "P45D", "2026-04-29"),
        ("2026-03-15", "P1Y", "2027-03-15"),
        ("2026-03-15", "P2W", "2026-03-29"),
        // Mixed spans walk months first, then days: Jan 31 + P1M15D
        // clamps to Feb 28, then steps 15 days to Mar 15.
        ("2026-01-31", "P1M15D", "2026-03-15"),
        ("2026-03-15", "P0D", "2026-03-15"),
    ] {
        holds(eq(shifted(base, sp), term(date(expected))));
    }
}

#[test]
fn month_ends_clamp_to_the_destination_month() {
    for (base, sp, expected) in [
        ("2026-01-31", "P1M", "2026-02-28"),
        ("2028-01-31", "P1M", "2028-02-29"),
        ("2026-11-30", "P3M", "2027-02-28"),
        ("2026-08-31", "P1M", "2026-09-30"),
        // A leap day shifted a year lands on Feb 28.
        ("2028-02-29", "P1Y", "2029-02-28"),
        ("2028-02-29", "P12M", "2029-02-28"),
    ] {
        holds(eq(shifted(base, sp), term(date(expected))));
    }
}

#[test]
fn clamped_shifts_are_not_associative_and_the_trap_is_pinned() {
    // Two quarterly rolls from Nov 30 drift to the 28th; one direct
    // half-year shift keeps the 30th. A schedule defined hop-by-hop is
    // therefore NOT the schedule defined from an anchor - the worked
    // example teaches this; the kernel pins it.
    holds(eq(
        add(shifted("2026-11-30", "P3M"), term(span("P3M"))),
        term(date("2027-05-28")),
    ));
    holds(eq(shifted("2026-11-30", "P6M"), term(date("2027-05-30"))));
    // And the round trip does not return: Jan 31 forward a month and
    // back a month lands on Jan 28.
    holds(eq(
        sub(shifted("2026-01-31", "P1M"), term(span("P1M"))),
        term(date("2026-01-28")),
    ));
}

#[test]
fn equivalent_spellings_shift_identically() {
    for (a, b) in [("P1Y", "P12M"), ("P1W", "P7D"), ("P2Y6M", "P30M")] {
        holds(eq(shifted("2026-03-15", a), shifted("2026-03-15", b)));
    }
}

#[test]
fn subtracting_a_span_walks_the_same_way_negated() {
    for (base, sp, expected) in [
        ("2026-06-15", "P3M", "2026-03-15"),
        ("2026-03-31", "P1M", "2026-02-28"),
        ("2026-04-29", "P45D", "2026-03-15"),
    ] {
        holds(eq(
            sub(term(date(base)), term(span(sp))),
            term(date(expected)),
        ));
    }
}

#[test]
fn date_differences_are_signed_actual_days() {
    for (later, earlier, expected) in [
        ("2026-02-01", "2026-01-31", "1"),
        ("2026-01-31", "2026-02-01", "-1"),
        ("2026-01-31", "2026-01-31", "0"),
        // Across a leap February: 2028 has Feb 29.
        ("2028-03-01", "2028-02-01", "29"),
        ("2027-03-01", "2027-02-01", "28"),
        // A full leap year against a common year.
        ("2029-01-01", "2028-01-01", "366"),
        ("2027-01-01", "2026-01-01", "365"),
    ] {
        holds(eq(
            sub(term(date(later)), term(date(earlier))),
            term(dec(expected)),
        ));
    }
}

fn rejects(prop: Prop) {
    let t = transformation("probe", params(&[]), vec![require(prop)]);
    let outcome = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect("evaluates cleanly");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected the proposition to be false: {outcome:?}"
    );
}

#[test]
fn span_equality_compares_normalised_values() {
    // Spans have no ordered comparison, but equality is coherent over
    // the normalised (months, days) pair: a year IS twelve months.
    // Days never normalise into months, so P1M and P30D stay distinct.
    holds(eq(term(span("P1Y")), term(span("P12M"))));
    holds(eq(term(span("P1W")), term(span("P7D"))));
    rejects(eq(term(span("P1M")), term(span("P30D"))));
}

#[test]
fn one_span_can_cross_the_whole_calendar() {
    // The grammar bounds a component only by its representation; the
    // range check is the evaluator's, against the date the span is
    // applied to. Ten thousand and one years from the calendar's
    // floor is a lawful shift.
    holds(eq(
        shifted("-009999-01-01", "P10001Y"),
        term(date("0002-01-01")),
    ));
    // The same vast span applied where it cannot land refuses by
    // name, per date - the span itself was never the problem.
    refuses(
        eq(shifted("2026-01-01", "P500000M"), term(date("2026-01-01"))),
        &["leaves the calendar"],
    );
}

#[test]
fn shifts_off_the_calendar_are_out_of_range_by_name() {
    // The calendar is jiff's proleptic Gregorian range (year -9999 to
    // 9999); the surface can only spell four-digit years, but the
    // kernel's boundary is the representation's own.
    for (base, sp, add_it) in [
        ("9999-12-31", "P1M", true),
        ("9999-12-31", "P1D", true),
        ("-009999-01-01", "P1M", false),
        ("-009999-01-01", "P1D", false),
    ] {
        let expr = if add_it {
            add(term(date(base)), term(span(sp)))
        } else {
            sub(term(date(base)), term(span(sp)))
        };
        refuses(eq(expr, term(date("2026-01-01"))), &["leaves the calendar"]);
    }
}

#[test]
fn the_deliberately_missing_rules_stay_type_errors() {
    use morpholog_core::ir_builder::{div, duration, mul, timestamp as ts};
    // Exact time cannot shift a civil date: the category error.
    refuses(
        eq(
            add(term(date("2026-01-01")), term(duration("PT24H"))),
            term(date("2026-01-02")),
        ),
        &["no arithmetic rule"],
    );
    // A calendar shift of an instant needs a time zone the kernel
    // refuses to guess.
    refuses(
        eq(
            add(term(ts("2026-01-01T00:00:00Z")), term(span("P1M"))),
            term(ts("2026-02-01T00:00:00Z")),
        ),
        &["no arithmetic rule"],
    );
    // Spans do not combine with each other, and the matrix is
    // asymmetric: the span sits on the right of the date, never left.
    refuses(
        eq(add(term(span("P1M")), term(span("P1D"))), term(span("P1M"))),
        &["no arithmetic rule"],
    );
    refuses(
        eq(
            add(term(span("P1M")), term(date("2026-01-01"))),
            term(date("2026-02-01")),
        ),
        &["no arithmetic rule"],
    );
    // On a date pair only Sub has a meaning; the rest refuse naming
    // the one rule that exists.
    refuses(
        eq(
            add(term(date("2026-01-01")), term(date("2026-01-01"))),
            term(dec("0")),
        ),
        &["not defined for two dates", "only Sub"],
    );
    refuses(
        eq(
            div(term(date("2026-02-01")), term(date("2026-01-01"))),
            term(dec("1")),
        ),
        &["not defined for two dates", "only Sub"],
    );
    // On date-and-span only Add and Sub shift; Mul refuses.
    refuses(
        eq(
            mul(term(date("2026-01-01")), term(span("P1M"))),
            term(date("2026-02-01")),
        ),
        &["not defined for date and calendar span"],
    );
}

#[test]
fn a_span_cannot_be_admitted_into_a_claim() {
    // `let` may hold a span (it is an expression); admitting it is the
    // boundary that refuses.
    let t = transformation(
        "smuggle",
        params(&[]),
        vec![
            morpholog_core::ir_builder::let_("sp", term(span("P3M"))),
            assert_("Holds", vec![var("sp")]),
        ],
    );
    let err = propose_with_test_actor(&t, vec![], &State::default(), &[], &[])
        .expect_err("a span must not reach a claim");
    assert!(
        format!("{err}").contains("cannot be admitted into claim"),
        "got: {err}"
    );
}

#[test]
fn a_span_cannot_arrive_as_a_transition_argument() {
    // Even through an Any-kinded position or a collection element, the
    // argument gate refuses before anything can bind.
    use morpholog_core::propose;
    let t = transformation(
        "takes",
        params(&["x"]),
        vec![assert_("Holds", vec![var("x")])],
    );
    for smuggled in [
        cal_span("P3M"),
        morpholog_core::EvalValue::Collection(vec![cal_span("P3M")]),
    ] {
        let transition = morpholog_core::Transition {
            transformation_name: "takes".into(),
            args: vec![smuggled],
            actor: test_actor(),
        };
        let err = propose(&t, &transition, &State::default(), &[], &[])
            .expect_err("a span argument must be refused");
        assert!(
            format!("{err}").contains("cannot take a calendar span argument"),
            "got: {err}"
        );
    }
}

#[test]
fn a_span_kinded_declaration_is_refused_at_validation() {
    use morpholog_core::{ArgDecl, PredicateArgKind, PredicateDecl, Program};
    let program = Program {
        predicates: vec![PredicateDecl {
            name: "Holds".into(),
            args: vec![ArgDecl {
                name: "sp".to_string(),
                kind: PredicateArgKind::CalendarSpan,
            }],
            disciplines: vec![],
        }],
        ..Program::default()
    };
    let errors = program.validate().expect_err("declaration must be refused");
    assert!(
        errors
            .iter()
            .any(|e| format!("{e}").contains("expression-only")),
        "got: {errors:?}"
    );
}
