//! Tests for the control-matrix view (`inspect controls`): the
//! per-transformation preconditions and the invariant guarantees,
//! derived mechanically from a parsed programme.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    and, assert_, bind_one, claim, implies, invariant, neq, not, params, predicate, program,
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
