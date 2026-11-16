//! Evaluation at shapes the ordinary suites do not reach, found by the
//! mutation census (#359): a `for` whose repeated variable is its
//! pattern's only test, a value lookup keyed on one field and filtered on another, a
//! recursive condition that calls another condition, and a period index
//! across the whole calendar.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::{ClaimInstance, EvalValue, Program, State};
use morpholog_test_support::{claim_instance, dec, subj};

const PROGRAMME: &str = "program evaluation_edges
predicate Link(source: Subject, target: Subject)
predicate Closed(node: Subject)
predicate Loop(node: Subject)
predicate Price(item: Subject, currency: Subject, amount: Decimal)
predicate Item(item: Subject)
predicate UsdPrice(item: Subject, amount: Decimal)
predicate Reaches(source: Subject, target: Subject)
predicate Book(book: Subject)
predicate Opens(book: Subject, on: Date)
predicate Closes(book: Subject, on: Date)
predicate Periods(book: Subject, index: Decimal)


-- Mark every link to itself: the repeated variable is the pattern's
-- only test.
transformation mark_loops():
    for Link(n, n):
        admit Loop(n)

derived UsdPrice(item):
    over Item(item)
    value amount = value Price(item, #usd, _)

-- Each step ends at an open node, a condition of its own.
define open(n):
    not Closed(n)

define reaches(a, b):
    (Link(a, b) and open(b)) or (Link(a, m) and open(m) and reaches(m, b))

derived Reaches(source, target):
    over Link(source, _) and Link(_, target) and reaches(source, target)

derived Periods(book):
    over Book(book)
    value index = period_index(value Opens(book, _), span(P1D), value Closes(book, _))
";

fn program() -> Program {
    morpholog_surface::parse_program(PROGRAMME).expect("fixture parses")
}

fn rows(derived: &str, claims: Vec<ClaimInstance>) -> Vec<Vec<EvalValue>> {
    let program = program();
    let d = program
        .derived_claims
        .iter()
        .find(|d| d.predicate.as_str() == derived)
        .expect("derived claim");
    let mut rows: Vec<Vec<EvalValue>> =
        morpholog_test_support::enumerate_derived(&program, d, &State::from_claims(claims))
            .expect("enumerates")
            .into_iter()
            .map(|c| c.args)
            .collect();
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

#[test]
fn a_repeated_variable_alone_decides_what_a_for_visits() {
    let program = program();
    let mark_loops = program
        .transformations
        .iter()
        .find(|t| t.name.as_str() == "mark_loops")
        .expect("transformation");
    let state = State::from_claims(vec![
        claim_instance("Link", &[subj("a"), subj("b")]),
        claim_instance("Link", &[subj("c"), subj("c")]),
    ]);
    let outcome = morpholog_test_support::propose(
        &program,
        &morpholog_test_support::test_transition(mark_loops, vec![]),
        &state,
        &mut morpholog_test_support::fresh(),
    )
    .expect("decides");
    let morpholog_core::Outcome::Accepted {
        asserted_claims, ..
    } = outcome
    else {
        panic!("refused: {outcome:?}");
    };
    assert_eq!(asserted_claims, vec![claim_instance("Loop", &[subj("c")])]);
}

#[test]
fn a_lookup_keyed_on_one_field_keeps_to_its_other_fields() {
    let claims = vec![
        claim_instance("Item", &[subj("i1")]),
        claim_instance("Price", &[subj("i1"), subj("eur"), dec(3)]),
        claim_instance("Price", &[subj("i1"), subj("usd"), dec(5)]),
        // More prices in dollars than prices of i1, so the lookup scans
        // i1's prices and must still hold each to the currency.
        claim_instance("Price", &[subj("i2"), subj("usd"), dec(7)]),
        claim_instance("Price", &[subj("i3"), subj("usd"), dec(9)]),
    ];
    assert_eq!(rows("UsdPrice", claims), vec![vec![subj("i1"), dec(5)]]);
}

#[test]
fn a_recursive_condition_reads_the_condition_it_calls() {
    // a -> b -> c, with c closed: only a reaches b.
    let claims = vec![
        claim_instance("Link", &[subj("a"), subj("b")]),
        claim_instance("Link", &[subj("b"), subj("c")]),
        claim_instance("Closed", &[subj("c")]),
    ];
    assert_eq!(rows("Reaches", claims), vec![vec![subj("a"), subj("b")]]);
}

#[test]
fn a_period_index_spans_the_whole_calendar() {
    // From the first day the calendar holds to the last, one day apart:
    // the search for the index must bracket every day between.
    let (min, max) = (jiff::civil::Date::MIN, jiff::civil::Date::MAX);
    let claims = vec![
        claim_instance("Book", &[subj("b1")]),
        claim_instance("Opens", &[subj("b1"), EvalValue::Date(min)]),
        claim_instance("Closes", &[subj("b1"), EvalValue::Date(max)]),
    ];
    let days = (max - min).get_days();
    assert_eq!(
        rows("Periods", claims),
        vec![vec![subj("b1"), dec(i64::from(days))]]
    );
}
