//! `define name(params) = (<value>)`: what it parses to beside `define name(params): <prop>`,
//! where a call to it parses, how it formats and reparses, and what the parser refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::format_program;
use morpholog_core::{DefinitionBody, Stmt, ValueExpr};
use morpholog_surface::parse_program;

const HEADER: &str = "program figures\n\n\
    predicate Stock(item: Subject, quantity: Decimal)\n\
    predicate Allocation(allocation: Subject, item: Subject, quantity: Decimal)\n\
    predicate Allocated(item: Subject, quantity: Decimal)\n\n";

fn parsed(rest: &str) -> morpholog_core::Program {
    parse_program(&format!("{HEADER}{rest}")).expect("parses")
}

#[test]
fn a_figure_parses_to_a_value_body_and_a_condition_to_a_proposition_body() {
    let p = parsed(
        "define allocated(item) = (sum(q | Allocation(_, item, q)))\n\n\
         define stocked(item):\n    Stock(item, _)\n",
    );
    assert_eq!(p.definitions.len(), 2);
    let figure = &p.definitions[0];
    assert_eq!(figure.name.as_str(), "allocated");
    assert_eq!(figure.parameters.len(), 1);
    assert!(matches!(
        &figure.body,
        DefinitionBody::Value(ValueExpr::Sum { .. })
    ));
    assert!(matches!(&p.definitions[1].body, DefinitionBody::Prop(_)));
}

#[test]
fn a_call_parses_wherever_a_value_stands() {
    let p = parsed(
        "define allocated(item) = (sum(q | Allocation(_, item, q)))\n\n\
         define headroom(item, on_hand) = (on_hand - allocated(item))\n\n\
         invariant fits:\n    Stock(item, on_hand) implies allocated(item) <= on_hand\n\n\
         transformation allocate(allocation, item, quantity):\n    \
             bind Stock(item, on_hand)\n    \
             let reserved = allocated(item)\n    \
             require fits: quantity <= headroom(item, on_hand)\n    \
             admit Allocation(allocation, item, quantity)\n\n\
         derived Allocated(item):\n    over Stock(item: item, ..)\n    value quantity = allocated(item)\n",
    );
    // Inside another figure's body, the call is an operand.
    assert!(matches!(
        &p.definitions[1].body,
        DefinitionBody::Value(ValueExpr::Arith { right, .. })
            if matches!(right.as_ref(), ValueExpr::Defined { name, args } if name.as_str() == "allocated" && args.len() == 1)
    ));
    // In a `let`, the whole value.
    let body = &p.transformations[0].body;
    assert!(matches!(
        &body[1],
        Stmt::Let { value: ValueExpr::Defined { name, .. }, .. } if name.as_str() == "allocated"
    ));
    // In a derived value clause.
    assert!(matches!(
        &p.derived_claims[0].values[0].expr,
        ValueExpr::Defined { name, .. } if name.as_str() == "allocated"
    ));
}

#[test]
fn the_formatter_prints_the_figure_and_the_output_reparses() {
    let p = parsed(
        "define allocated(item) = (sum(q | Allocation(_, item, q)))\n\n\
         define headroom(item, on_hand) = (on_hand - allocated(item))\n\n\
         define stocked(item):\n    Stock(item, _)\n\n\
         transformation t(item, on_hand, quantity):\n    \
             require fits: quantity <= headroom(item, on_hand)\n    \
             admit Stock(item, quantity)\n",
    );
    let formatted = format_program(&p);
    assert!(
        formatted.contains("define allocated(item) = (sum(q | Allocation(_, item, q)))\n"),
        "{formatted}"
    );
    assert!(
        formatted.contains("define headroom(item, on_hand) = (on_hand - allocated(item))\n"),
        "{formatted}"
    );
    assert!(formatted.contains("define stocked(item):\n"), "{formatted}");
    assert!(
        formatted.contains("quantity <= headroom(item, on_hand)"),
        "{formatted}"
    );
    assert_eq!(parse_program(&formatted).unwrap(), p);
}

#[test]
fn a_figure_without_parentheses_a_mint_argument_and_an_empty_body_are_refused() {
    for rest in [
        "define allocated(item) = sum(q | Allocation(_, item, q))\n",
        "define allocated(item) = ()\n",
        "define tagged(item) = (allocated(new Subject()))\n",
    ] {
        assert!(
            parse_program(&format!("{HEADER}{rest}")).is_err(),
            "should be refused: {rest}"
        );
    }
}
