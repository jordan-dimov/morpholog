//! Exact arithmetic at the edges the ordinary suites do not reach, found
//! by the mutation census (#359): sums whose decimals carry different
//! scales, a sum that fits only once its trailing zeros are dropped, a
//! duration sum at the most negative span, `max` and `min` over
//! quantities, rounding a tiny value to a huge quantum, and the claims a
//! failed gate reports as missing.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::{EvalValue, Program, State};
use morpholog_test_support::{claim_instance, dec_str, qty, subj};

const PROGRAMME: &str = "program exact_values
predicate Book(book: Subject)
predicate Line(book: Subject, line: Subject, amount: Decimal)
predicate Wait(book: Subject, wait: Subject, span: Duration)
predicate Load(book: Subject, load: Subject, mass: Decimal[kg])
predicate Spare(book: Subject, load: Subject, mass: Decimal[kg])
predicate Figure(book: Subject, figure: Decimal)
predicate Step(book: Subject, step: Decimal)
predicate Total(book: Subject, total: Decimal)
predicate Waited(book: Subject, total: Duration)
predicate Heaviest(book: Subject, mass: Decimal[kg])
predicate Lightest(book: Subject, mass: Decimal[kg])
predicate Rounded(book: Subject, rounded: Decimal)

derived Total(book):
    over Book(book)
    value total = sum(a | Line(book, _, a))

derived Waited(book):
    over Book(book)
    value total = sum(s | Wait(book, _, s))

derived Heaviest(book):
    over Book(book)
    value mass = max(sum(m | Load(book, _, m)), sum(m | Spare(book, _, m)))

derived Lightest(book):
    over Book(book)
    value mass = min(sum(m | Load(book, _, m)), sum(m | Spare(book, _, m)))

derived Rounded(book):
    over Book(book)
    value rounded = round(value Figure(book, _), value Step(book, _))

transformation check(book, limit):
    require above_limit: Line(book, _, a) and a > limit
    admit Book(book)
";

fn program() -> Program {
    morpholog_surface::parse_program(PROGRAMME).expect("fixture parses")
}

/// The one value `derived` computes for book `b1` in a state of `claims`
/// and the book.
fn derive(derived: &str, claims: Vec<morpholog_core::ClaimInstance>) -> Result<EvalValue, String> {
    let program = program();
    let d = program
        .derived_claims
        .iter()
        .find(|d| d.predicate.as_str() == derived)
        .expect("derived claim");
    let mut claims = claims;
    claims.push(claim_instance("Book", &[subj("b1")]));
    morpholog_test_support::enumerate_derived(&program, d, &State::from_claims(claims))
        .map(|rows| rows[0].args[1].clone())
        .map_err(|e| e.to_string())
}

fn line(id: &str, amount: &str) -> morpholog_core::ClaimInstance {
    claim_instance("Line", &[subj("b1"), subj(id), dec_str(amount)])
}

#[test]
fn a_sum_of_decimals_at_two_scales_is_exact_whichever_comes_first() {
    // The running total widens to the larger scale, or the next value
    // does: both branches, each from a scale other than zero.
    for claims in [
        vec![line("l1", "0.5"), line("l2", "0.25")],
        vec![line("l1", "0.25"), line("l2", "0.5")],
    ] {
        assert_eq!(derive("Total", claims), Ok(dec_str("0.75")));
    }
}

#[test]
fn a_sum_fits_once_its_trailing_zeros_are_dropped() {
    // Two values at scale 28 whose total, 10, holds 29 digits at that
    // scale: past what a decimal holds, until the zeros go.
    let five = "5.0000000000000000000000000000";
    assert_eq!(
        derive("Total", vec![line("l1", five), line("l2", five)]),
        Ok(dec_str("10"))
    );
}

#[test]
fn a_duration_sum_reaches_the_most_negative_span() {
    // Floor division puts this total one second below the least whole
    // second, which the carry brings back.
    let min = EvalValue::Duration(jiff::SignedDuration::MIN);
    assert_eq!(
        derive(
            "Waited",
            vec![claim_instance(
                "Wait",
                &[subj("b1"), subj("w1"), min.clone()]
            )]
        ),
        Ok(min)
    );
}

#[test]
fn max_and_min_hold_quantities_of_one_unit_and_refuse_two() {
    let load = |id: &str, mass: &str, unit: &str| {
        claim_instance("Load", &[subj("b1"), subj(id), qty(mass, unit)])
    };
    let spare = |id: &str, mass: &str, unit: &str| {
        claim_instance("Spare", &[subj("b1"), subj(id), qty(mass, unit)])
    };
    let claims = vec![
        load("a", "2", "kg"),
        load("b", "3", "kg"),
        spare("c", "4", "kg"),
    ];
    assert_eq!(derive("Heaviest", claims.clone()), Ok(qty("5", "kg")));
    assert_eq!(derive("Lightest", claims), Ok(qty("4", "kg")));
    // Stored in another unit than declared: the two sums are of
    // different units, which neither `max` nor `min` orders.
    let mixed = vec![load("a", "2", "t"), spare("c", "4", "kg")];
    assert!(derive("Heaviest", mixed.clone()).is_err());
    assert!(derive("Lightest", mixed).is_err());
}

#[test]
fn a_tiny_value_rounds_to_zero_at_a_huge_quantum() {
    // The quantum's units at the value's scale overflow an i128, so the
    // exact path decides: the nearest multiple is zero.
    let claims = vec![
        claim_instance(
            "Figure",
            &[subj("b1"), dec_str("0.0000000000000000000000000001")],
        ),
        claim_instance(
            "Step",
            &[subj("b1"), dec_str("79228162514264337593543950335")],
        ),
    ];
    assert_eq!(derive("Rounded", claims), Ok(dec_str("0")));
}

#[test]
fn a_gate_that_found_its_claim_reports_none_missing() {
    // `Line(b1, _, 3)` matched; the comparison failed. Nothing is missing.
    let program = program();
    let t = program
        .transformations
        .iter()
        .find(|t| t.name.as_str() == "check")
        .expect("transformation");
    let transition = morpholog_test_support::test_transition(t, vec![subj("b1"), dec_str("5")]);
    let state = State::from_claims(vec![line("l1", "3")]);
    let explanation = morpholog_test_support::explain(
        &program,
        &transition,
        &state,
        &mut morpholog_test_support::fresh(),
    );
    let json = serde_json::to_value(&explanation).expect("serialises");
    let gate = &json["verdict"]["rejected"];
    assert_eq!(gate["rule"], "above_limit", "{json}");
    assert_eq!(
        gate["directly_missing_claims"],
        serde_json::json!([]),
        "{json}"
    );
}
