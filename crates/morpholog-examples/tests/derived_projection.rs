//! Probes for derived-head projection (the shape behind subset heads
//! and named value lookups).
//!
//! Each test pins a piece of today's behaviour that the projection
//! work builds on:
//!
//! - `subset_head_collapses_witnesses_and_orders_rows`: a head that
//!   carries fewer variables than its domain binds already enumerates
//!   one row per distinct projected key tuple, in deterministic order.
//!   For a fixed admitted state, a derived value depends on the
//!   projected key bindings, never on which witness produced the tuple.
//!
//! - `non_key_value_reference_refuses_at_both_tiers`: a value
//!   expression naming a variable the domain binds but the head does
//!   not carry refuses at authoring (naming both remedies) and at eval.
//!
//! - `positional_lookup_extracts_the_first_wildcard_only`: `value
//!   P(_, x, _)` extracts the FIRST wildcard position. There is no
//!   positional spelling that leaves an earlier coordinate
//!   unconstrained while extracting a later one - the gap the named
//!   extraction hole closes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{EvalError, State, enumerate_derived, ir_builder as b};
use morpholog_test_support::{claim_instance, dec, subj};

fn line_program_predicates() -> Vec<morpholog_core::PredicateDecl> {
    vec![
        b::predicate("Line")
            .subject("invoice")
            .subject("line")
            .decimal("amount")
            .build(),
    ]
}

fn line_state() -> State {
    State::from_claims(vec![
        claim_instance("Line", &[subj("i1"), subj("l1"), dec(10)]),
        claim_instance("Line", &[subj("i1"), subj("l2"), dec(5)]),
        claim_instance("Line", &[subj("i2"), subj("l3"), dec(7)]),
    ])
}

#[test]
fn subset_head_collapses_witnesses_and_orders_rows() {
    let derived = morpholog_core::DerivedClaim {
        predicate: "InvoiceTotal".into(),
        keys: vec!["invoice".into()],
        values: vec![morpholog_core::DerivedValue {
            name: "total".to_string(),
            expr: b::sum(
                b::term(b::var("a")),
                b::claim("Line", vec![b::var("invoice"), b::var("l"), b::var("a")]),
            ),
        }],
        domain: b::claim(
            "Line",
            vec![b::var("invoice"), b::var("line"), b::var("amount")],
        ),
    };
    let mut predicates = line_program_predicates();
    predicates.push(
        b::predicate("InvoiceTotal")
            .subject("invoice")
            .decimal("total")
            .build(),
    );
    let program = b::program("probe")
        .predicates(predicates)
        .derived_claims(vec![derived.clone()])
        .build();
    program
        .validate()
        .expect("a subset head is lawful: no head-totality rule exists");

    let rows = enumerate_derived(&derived, &line_state(), &[]).expect("enumerate should succeed");
    assert_eq!(
        rows,
        vec![
            claim_instance("InvoiceTotal", &[subj("i1"), dec(15)]),
            claim_instance("InvoiceTotal", &[subj("i2"), dec(7)]),
        ],
        "one row per distinct projected key, witnesses collapsed, deterministic order"
    );
}

#[test]
fn non_key_value_reference_refuses_at_both_tiers() {
    let derived = morpholog_core::DerivedClaim {
        predicate: "InvoiceLineEcho".into(),
        keys: vec!["invoice".into()],
        values: vec![morpholog_core::DerivedValue {
            name: "which_line".to_string(),
            expr: b::term(b::var("line")),
        }],
        domain: b::claim(
            "Line",
            vec![b::var("invoice"), b::var("line"), b::var("amount")],
        ),
    };
    let mut predicates = line_program_predicates();
    predicates.push(
        b::predicate("InvoiceLineEcho")
            .subject("invoice")
            .subject("which_line")
            .build(),
    );
    let program = b::program("probe")
        .predicates(predicates)
        .derived_claims(vec![derived.clone()])
        .build();

    let errs = program
        .validate()
        .expect_err("a value expression may reference head keys only");
    let msg = errs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    assert!(
        msg.contains("line") && msg.contains("head") && msg.contains("field name"),
        "the refusal names the variable and both remedies, got: {msg}"
    );

    let eval_err = enumerate_derived(&derived, &line_state(), &[])
        .expect_err("eval remains the second tier for hand-built IR");
    assert!(matches!(eval_err, EvalError::UnboundVariable(_)));
}

#[test]
fn positional_lookup_extracts_the_first_wildcard_only() {
    let derived = morpholog_core::DerivedClaim {
        predicate: "SheetPeriod".into(),
        keys: vec!["sheet".into()],
        values: vec![morpholog_core::DerivedValue {
            name: "extracted".to_string(),
            expr: b::value_of("Sheet", vec![b::wildcard(), b::var("sheet"), b::wildcard()]),
        }],
        domain: b::claim(
            "Sheet",
            vec![b::var("period_end"), b::var("sheet"), b::var("rate")],
        ),
    };
    let state = State::from_claims(vec![claim_instance(
        "Sheet",
        &[subj("march"), subj("s1"), dec(42)],
    )]);

    let rows = enumerate_derived(&derived, &state, &[]).expect("enumerate should succeed");
    assert_eq!(
        rows,
        vec![claim_instance("SheetPeriod", &[subj("s1"), subj("march")])],
        "the first wildcard is the hole: position 0 (period_end) is extracted, \
         never the rate behind it"
    );
}
