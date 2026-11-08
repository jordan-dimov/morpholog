//! Where a body read loses its keys, as `check -v` reports it: the
//! statement, the named gate it sits in, and the chain of definitions it
//! went through, outermost first. The analysis is the loader's own, so the
//! report and `ReadPlan::of` must agree on which predicates are whole.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ReadFilter, ReadPlan, WholeRead};
use morpholog_surface::parse_program;

const SOURCE: &str = "\
program nested_reads
predicate Limit(org: Subject, cap: Decimal)
predicate Holding(org: Subject, item: Subject, qty: Decimal)
predicate Flag(item: Subject)
define held(item, qty):
    Holding(_, item, qty)
define flagged_holding(item, qty):
    held(item, qty) and Flag(item)
transformation review(org, items):
    require within_limit: forall cap in Limit(org, cap): sum(q | flagged_holding(i, q)) <= cap
    require sum(q | held(i, q)) >= 0
    for i in items:
        require is_flagged: Flag(i)
        require Holding(org, i, _)
";

fn read(predicate: &str, statement: usize, rule: Option<&str>, through: &[&str]) -> WholeRead {
    WholeRead {
        predicate: predicate.into(),
        statement,
        rule: rule.map(Into::into),
        through: through.iter().map(|d| (*d).into()).collect(),
    }
}

#[test]
fn a_whole_read_names_its_gate_and_every_definition_it_went_through() {
    let program = parse_program(SOURCE).expect("parses");
    let review = program.transformation("review").unwrap();
    let found = ReadPlan::whole_reads(review, &program.definitions);
    assert_eq!(
        found,
        vec![
            // Two definitions deep, inside the named gate.
            read(
                "Holding",
                1,
                Some("within_limit"),
                &["flagged_holding", "held"]
            ),
            read("Flag", 1, Some("within_limit"), &["flagged_holding"]),
            // An unnamed gate after a named one carries no name. The
            // loop's binder ranges over a parameter, so both reads inside
            // the loop are keyed and neither is listed.
            read("Holding", 2, None, &["held"]),
        ]
    );

    // The loader's plan, built without the records, agrees.
    let plan = ReadPlan::of(review, &program.definitions);
    let whole: Vec<&str> = plan
        .reads
        .iter()
        .filter(|(_, f)| **f == ReadFilter::Whole)
        .map(|(p, _)| p.as_str())
        .collect();
    assert_eq!(whole, vec!["Flag", "Holding"]);
    assert!(matches!(plan.reads[&"Limit".into()], ReadFilter::Keyed(_)));
}
