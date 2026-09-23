//! Derived-head projection: heads that carry fewer variables than their
//! domain binds, and value lookups.
//!
//! - `subset_head_collapses_witnesses_and_orders_rows`: one row per distinct
//!   projected key tuple, in a fixed order. A derived value depends on the
//!   key bindings, never on which witness produced them.
//!
//! - `non_key_value_reference_refuses_at_both_tiers`: a value that names a
//!   variable the head does not carry is refused at authoring (naming both
//!   remedies) and at eval.
//!
//! - `positional_lookup_extracts_the_first_wildcard_only`: `value P(_, x, _)`
//!   extracts the FIRST wildcard. Only a named lookup can skip an earlier
//!   coordinate to read a later one.

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
        msg.contains("line") && msg.contains("head") && msg.contains("field: _"),
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

#[test]
fn a_kind_learned_inside_a_value_expression_still_reaches_the_output_check() {
    // The domain says `id: Any`, the value lookup refines it to Date, and the
    // output says Subject. The output key check must see the refined kind;
    // the domain alone says Any and would let the mismatch through.
    let derived = morpholog_core::DerivedClaim {
        predicate: "Output".into(),
        keys: vec!["id".into()],
        values: vec![morpholog_core::DerivedValue {
            name: "amount".to_string(),
            expr: b::value_of("Dated", vec![b::var("id"), b::wildcard()]),
        }],
        domain: b::claim("Source", vec![b::var("id")]),
    };
    let program = b::program("probe")
        .predicates(vec![
            b::predicate("Source").any("id").build(),
            b::predicate("Dated").date("id").decimal("amount").build(),
            b::predicate("Output")
                .subject("id")
                .decimal("amount")
                .build(),
        ])
        .derived_claims(vec![derived])
        .build();
    let errs = program
        .validate()
        .expect_err("Date-refined `id` cannot fill a Subject output key");
    assert!(
        errs.iter().any(|e| matches!(
            e,
            morpholog_core::ValidationError::ArgKindMismatch { position: 0, .. }
        )),
        "expected an output-key kind mismatch at position 0; got {errs:?}"
    );
}
