//! Tests for the control-matrix view (`inspect controls`): the
//! per-transformation preconditions and the invariant guarantees,
//! derived mechanically from a parsed programme.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    and, assert_, bind_one, claim, for_, implies, invariant, neq, not, params, predicate, program,
    require, var,
};
use morpholog_core::{controls, render_controls};

/// A miniature of the two-person rule: a decision gated on two
/// distinct verifications, with a bind, a require, and an invariant -
/// every control form the matrix renders.
fn mini() -> morpholog_core::Program {
    program("mini_controls")
        .predicates(vec![
            predicate("MatchRecorded")
                .subject("match")
                .subject("use_id")
                .build(),
            predicate("MatchVerified")
                .subject("match")
                .subject("verifier")
                .build(),
            predicate("Decision")
                .subject("decision")
                .subject("match")
                .build(),
        ])
        .invariants(vec![
            invariant(
                "decision_rests_on_two_distinct_verifiers",
                implies(
                    claim("Decision", vec![var("d"), var("m")]),
                    and(vec![
                        claim("MatchVerified", vec![var("m"), var("v1")]),
                        claim("MatchVerified", vec![var("m"), var("v2")]),
                        neq(var("v1"), var("v2")),
                    ]),
                ),
            ),
            invariant(
                "no_free_floating_decisions",
                not(and(vec![
                    claim("Decision", vec![var("d"), var("m")]),
                    not(claim("MatchRecorded", vec![var("m"), var("u")])),
                ])),
            ),
        ])
        .transformations(vec![morpholog_core::ir_builder::transformation(
            "decide",
            params(&["decision", "match"]),
            vec![
                bind_one(claim("MatchRecorded", vec![var("match"), var("u")])),
                require(and(vec![
                    claim("MatchVerified", vec![var("match"), var("v1")]),
                    claim("MatchVerified", vec![var("match"), var("v2")]),
                    neq(var("v1"), var("v2")),
                ])),
                assert_("Decision", vec![var("decision"), var("match")]),
            ],
        )])
        .build()
}

#[test]
fn matrix_carries_gates_in_body_order_with_consulted_predicates() {
    let p = mini();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
    let matrix = controls(&p);
    assert_eq!(matrix.program, "mini_controls");
    assert_eq!(matrix.transformations.len(), 1);
    let decide = &matrix.transformations[0];
    assert_eq!(decide.transformation, "decide");
    assert_eq!(decide.gates.len(), 2, "{decide:?}");
    // Body order: the bind first, then the require.
    assert_eq!(decide.gates[0].form, "bind");
    assert_eq!(decide.gates[0].consults, vec!["MatchRecorded"]);
    assert_eq!(decide.gates[1].form, "require");
    assert_eq!(decide.gates[1].consults, vec!["MatchVerified"]);
    assert!(
        decide.gates[1].condition.contains("v1 != v2"),
        "the distinctness condition renders in surface syntax: {}",
        decide.gates[1].condition
    );
    // The guarantees ride along, one per invariant; the not(...)
    // invariant names its forbidden state outright.
    assert_eq!(matrix.guarantees.len(), 2);
    assert!(matrix.guarantees[1].forbids.is_some());
}

#[test]
fn rendered_matrix_reads_as_the_auditor_view() {
    let rendered = render_controls(&controls(&mini()));
    for fragment in [
        "Controls for `mini_controls`",
        "Before each action (gates):",
        "decide may commit only when:",
        "exactly one claim matches MatchRecorded(match, u)",
        "consults: MatchVerified",
        "Always (invariants):",
        "decision_rests_on_two_distinct_verifiers",
        "forbids outright:",
    ] {
        assert!(
            rendered.contains(fragment),
            "`{fragment}` not in:\n{rendered}"
        );
    }
}

#[test]
fn a_gateless_transformation_renders_its_invariant_only_admission() {
    // A transformation with no require/bind preconditions is governed
    // by the invariants alone; the matrix says so rather than showing
    // an empty gate list silently.
    let p = program("open_admit")
        .predicates(vec![predicate("Note").subject("note").build()])
        .transformations(vec![morpholog_core::ir_builder::transformation(
            "jot",
            params(&["note"]),
            vec![assert_("Note", vec![var("note")])],
        )])
        .build();
    let matrix = controls(&p);
    assert!(matrix.transformations[0].gates.is_empty());
    let rendered = render_controls(&matrix);
    assert!(
        rendered.contains("no preconditions: admission is governed by the invariants alone"),
        "the gateless case is named, not blank:\n{rendered}"
    );
}

#[test]
fn gates_inside_a_for_body_are_not_lifted_as_preconditions() {
    // Doctrine pin: a `require` inside a `for` body is an iteration
    // condition, not an admission precondition, so the control matrix
    // does not surface it among the transformation's gates. Only the
    // top-level statements count. If this ever changes, the matrix
    // would start reporting per-item conditions as if they gated the
    // whole transformation - a misreading an auditor must not be given.
    let p = program("loops")
        .predicates(vec![
            predicate("Batch")
                .subject("batch")
                .collection("items")
                .build(),
            predicate("Seen").subject("item").build(),
            predicate("Allowed").subject("item").build(),
            predicate("Done").subject("batch").build(),
        ])
        .transformations(vec![morpholog_core::ir_builder::transformation(
            "process",
            params(&["batch", "items"]),
            vec![
                require(claim("Batch", vec![var("batch"), var("items")])),
                for_(
                    "item",
                    morpholog_core::ir_builder::term(var("items")),
                    vec![
                        require(claim("Allowed", vec![var("item")])),
                        assert_("Seen", vec![var("item")]),
                    ],
                ),
                assert_("Done", vec![var("batch")]),
            ],
        )])
        .build();
    let matrix = controls(&p);
    let process = &matrix.transformations[0];
    // Only the top-level require is a gate; the in-loop `Allowed`
    // check is not lifted.
    assert_eq!(process.gates.len(), 1, "{:?}", process.gates);
    assert_eq!(process.gates[0].consults, vec!["Batch"]);
    assert!(
        !process
            .gates
            .iter()
            .any(|g| g.consults.contains(&"Allowed".to_string())),
        "in-loop conditions must not surface as transformation gates: {:?}",
        process.gates
    );
}
