//! `if <proposition>:` with an optional `else:` in a transformation body: what it parses to,
//! how it formats and reparses, and that a mint inside a branch is named there.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::format::format_program;
use morpholog_core::{Prop, Stmt, Term};
use morpholog_surface::parse_program;

fn header(rest: &str) -> String {
    format!(
        "program conditional_test\n\n\
         predicate Flag(x: Subject)\n\
         predicate Then(x: Subject)\n\
         predicate Else(x: Subject)\n\
         predicate Made(item: Subject, x: Subject)\n\n{rest}"
    )
}

fn body(source: &str) -> Vec<Stmt> {
    let p = parse_program(&header(source)).expect("parses");
    p.transformations[0].body.clone()
}

#[test]
fn if_with_else_parses_to_both_branches() {
    let body = body(
        "transformation t(x):\n    if Flag(x):\n        admit Then(x)\n    else:\n        admit Else(x)\n",
    );
    let [
        Stmt::If {
            when,
            then,
            otherwise,
        },
    ] = body.as_slice()
    else {
        panic!("one if: {body:?}");
    };
    assert!(matches!(when, Prop::Claim { predicate, .. } if predicate.as_str() == "Flag"));
    assert!(matches!(&then[..], [Stmt::Assert(c)] if c.predicate.as_str() == "Then"));
    assert!(matches!(&otherwise[..], [Stmt::Assert(c)] if c.predicate.as_str() == "Else"));
}

#[test]
fn if_without_else_has_an_empty_otherwise_and_the_body_goes_on() {
    let body =
        body("transformation t(x):\n    if Flag(x):\n        admit Then(x)\n    admit Else(x)\n");
    assert_eq!(body.len(), 2);
    assert!(matches!(&body[0], Stmt::If { otherwise, .. } if otherwise.is_empty()));
    assert!(matches!(&body[1], Stmt::Assert(c) if c.predicate.as_str() == "Else"));
}

#[test]
fn a_nested_if_and_a_comparison_condition_parse() {
    let body = body(
        "transformation t(x, n):\n    if n > 0:\n        if Flag(x):\n            admit Then(x)\n        else:\n            admit Else(x)\n",
    );
    let [Stmt::If { then, .. }] = body.as_slice() else {
        panic!("one if: {body:?}");
    };
    assert!(matches!(&then[..], [Stmt::If { otherwise, .. }] if otherwise.len() == 1));
}

#[test]
fn the_formatter_prints_both_blocks_and_the_output_reparses() {
    let source = header(
        "transformation t(x, n):\n    if n > 0 and Flag(x):\n        let y = x\n        admit Then(y)\n    else:\n        admit Else(x)\n    if Flag(x):\n        admit Then(x)\n",
    );
    let p = parse_program(&source).unwrap();
    let formatted = format_program(&p);
    assert!(
        formatted.contains(
            "  if (n > 0) and Flag(x):\n    let y = x\n    admit Then(y)\n  else:\n    admit Else(x)\n  if Flag(x):\n    admit Then(x)\n"
        ),
        "{formatted}"
    );
    assert_eq!(parse_program(&formatted).unwrap(), p);
}

#[test]
fn a_mint_inside_a_branch_is_named_inside_it() {
    let body = body(
        "transformation t(x):\n    if Flag(x):\n        admit Made(new Subject(), x)\n    else:\n        admit Made(new Subject(), x)\n",
    );
    let [
        Stmt::If {
            then, otherwise, ..
        },
    ] = body.as_slice()
    else {
        panic!("one if: {body:?}");
    };
    assert!(matches!(&then[0], Stmt::LetNewSubject { name } if name.as_str() == "_subject_1"));
    assert!(matches!(&then[1], Stmt::Assert(c) if c.args[0] == Term::Var("_subject_1".into())));
    assert!(matches!(&otherwise[0], Stmt::LetNewSubject { name } if name.as_str() == "_subject_2"));
}

#[test]
fn an_else_with_no_if_and_an_empty_block_are_refused() {
    for source in [
        "transformation t(x):\n    admit Then(x)\n    else:\n        admit Else(x)\n",
        "transformation t(x):\n    if Flag(x):\n    admit Then(x)\n",
    ] {
        assert!(
            parse_program(&header(source)).is_err(),
            "should be refused: {source}"
        );
    }
}
