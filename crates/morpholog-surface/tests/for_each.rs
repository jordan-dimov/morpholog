//! `for <claim pattern>:` in a transformation body: what it parses to beside `for x in`,
//! named fields and wildcards, how it formats and reparses, and what is refused.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::format::format_program;
use morpholog_core::{Stmt, Term};
use morpholog_surface::parse_program;

fn header(rest: &str) -> String {
    format!(
        "program for_each_test\n\n\
         predicate Line(line: Subject, order: Subject, quantity: Decimal)\n\
         predicate Seen(line: Subject)\n\
         predicate Made(item: Subject, line: Subject)\n\n{rest}"
    )
}

fn body(source: &str) -> Vec<Stmt> {
    let p = parse_program(&header(source)).expect("parses");
    p.transformations[0].body.clone()
}

#[test]
fn a_positional_pattern_parses_to_a_for_over_the_predicate() {
    let body =
        body("transformation t(order):\n    for Line(line, order, _):\n        admit Seen(line)\n");
    let [
        Stmt::ForEach {
            predicate,
            args,
            body,
        },
    ] = body.as_slice()
    else {
        panic!("one for: {body:?}");
    };
    assert_eq!(predicate.as_str(), "Line");
    assert_eq!(
        args,
        &vec![
            Term::Var("line".into()),
            Term::Var("order".into()),
            Term::Wildcard
        ]
    );
    assert!(matches!(&body[..], [Stmt::Assert(c)] if c.predicate.as_str() == "Seen"));
}

#[test]
fn a_named_pattern_with_a_rest_resolves_through_the_declaration() {
    let body = body(
        "transformation t(order):\n    for Line(order: order, line: line, ..):\n        admit Seen(line)\n",
    );
    let [Stmt::ForEach { args, .. }] = body.as_slice() else {
        panic!("one for: {body:?}");
    };
    assert_eq!(
        args,
        &vec![
            Term::Var("line".into()),
            Term::Var("order".into()),
            Term::Wildcard
        ]
    );
}

#[test]
fn a_for_over_a_collection_still_parses_beside_it() {
    let body = body(
        "transformation t(order, lines):\n    for line in lines:\n        admit Seen(line)\n    for Line(line, order, _):\n        admit Seen(line)\n",
    );
    assert!(matches!(&body[0], Stmt::For { binding, .. } if binding.as_str() == "line"));
    assert!(matches!(&body[1], Stmt::ForEach { .. }));
}

#[test]
fn the_formatter_prints_the_pattern_and_the_output_reparses() {
    let source = header(
        "transformation t(order):\n    for Line(line, order, quantity):\n        require positive: quantity > 0\n        admit Made(new Subject(), line)\n",
    );
    let p = parse_program(&source).unwrap();
    let formatted = format_program(&p);
    assert!(
        formatted.contains(
            "  for Line(line, order, quantity):\n    require positive: quantity > 0\n    let _subject_1 = new Subject()\n    admit Made(_subject_1, line)\n"
        ),
        "{formatted}"
    );
    assert_eq!(parse_program(&formatted).unwrap(), p);
}

#[test]
fn a_mint_in_the_pattern_an_unknown_field_and_an_empty_body_are_refused() {
    for source in [
        "transformation t(order):\n    for Line(new Subject(), order, _):\n        admit Seen(order)\n",
        "transformation t(order):\n    for Line(order: order, lien: line, ..):\n        admit Seen(line)\n",
        "transformation t(order):\n    for Line(line, order, _):\n    admit Seen(order)\n",
    ] {
        assert!(
            parse_program(&header(source)).is_err(),
            "should be refused: {source}"
        );
    }
}

#[test]
fn a_pattern_over_an_undeclared_predicate_is_refused_by_validation() {
    let p = parse_program(&header(
        "transformation t(order):\n    for Lien(line, order, _):\n        admit Seen(line)\n",
    ))
    .unwrap();
    assert!(p.validate().is_err(), "an undeclared predicate");
}
