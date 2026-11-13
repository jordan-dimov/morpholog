//! `name: Kind` in a transformation's parameter list: what it parses to beside a bare
//! parameter, units, how it formats and reparses, and what is refused.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::format::format_program;
use morpholog_core::{PredicateArgKind, Unit};
use morpholog_surface::parse_program;

const HEADER: &str = "program declared\n\n\
    predicate Order(order: Subject)\n\
    predicate Closed(order: Subject)\n\
    predicate Paid(order: Subject, amount: Decimal[USD])\n\n";

fn parsed(rest: &str) -> morpholog_core::Program {
    parse_program(&format!("{HEADER}{rest}")).expect("parses")
}

#[test]
fn a_declared_kind_sits_beside_the_parameter_and_a_bare_one_is_none() {
    let p = parsed(
        "transformation close(order, force: Bool, paid: Decimal[USD], notes: Collection):\n    \
             require Order(order)\n    \
             require force = true or paid >= 0\n    \
             for n in notes:\n        admit Closed(n)\n    \
             admit Closed(order)\n",
    );
    let t = &p.transformations[0];
    assert_eq!(t.parameters.len(), 4);
    assert_eq!(
        t.declared_kinds,
        vec![
            None,
            Some(PredicateArgKind::Bool),
            Some(PredicateArgKind::Quantity(Unit::from("USD"))),
            Some(PredicateArgKind::Collection),
        ]
    );
    assert_eq!(
        t.declared_kind(&"force".into()),
        Some(&PredicateArgKind::Bool)
    );
    assert_eq!(t.declared_kind(&"order".into()), None);
}

#[test]
fn the_formatter_prints_the_declarations_and_the_output_reparses() {
    let p = parsed(
        "transformation close(order, force: Bool, paid: Decimal[USD]):\n    \
             require Order(order)\n    \
             require force = true or paid >= 0\n    \
             admit Closed(order)\n",
    );
    let formatted = format_program(&p);
    assert!(
        formatted.contains("transformation close(order, force: Bool, paid: Decimal[USD]):\n"),
        "{formatted}"
    );
    assert_eq!(parse_program(&formatted).unwrap(), p);
}

#[test]
fn an_unknown_kind_and_a_unit_on_a_non_decimal_are_refused() {
    for rest in [
        "transformation close(order, force: String):\n    admit Closed(order)\n",
        "transformation close(order, force: Bool[USD]):\n    admit Closed(order)\n",
    ] {
        assert!(
            parse_program(&format!("{HEADER}{rest}")).is_err(),
            "should be refused: {rest}"
        );
    }
}
