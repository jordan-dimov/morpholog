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
fn a_gate_front_loads_the_invariant_it_pre_checks() {
    let matrix = controls(&mini());
    let decide = &matrix.transformations[0];

    // The require gate (body order: after the bind) demands the two
    // distinct verifications the standing invariant enforces, so it
    // front-loads that invariant through the shared MatchVerified.
    let require_gate = &decide.gates[1];
    assert_eq!(require_gate.form, "require");
    assert_eq!(require_gate.front_loads.len(), 1, "{require_gate:?}");
    let link = &require_gate.front_loads[0];
    assert_eq!(link.invariant, "decision_rests_on_two_distinct_verifiers");
    // Both sides of the correspondence: the admitted predicate that puts
    // the invariant in play, and the shared consequent predicate.
    assert_eq!(link.triggered_by, vec!["Decision"]);
    assert_eq!(link.shared, vec!["MatchVerified"]);
    // The failure mode is rendered mechanically as `A and not (C)`, and
    // is present even though this implication-shaped invariant has no
    // `Guarantee::forbids` (only the `not(..)` shape populates that).
    assert!(matrix.guarantees[0].forbids.is_none());
    assert!(
        link.failure_shape.contains("Decision"),
        "{}",
        link.failure_shape
    );
    assert!(
        link.failure_shape.contains("and not ("),
        "{}",
        link.failure_shape
    );

    // The bind looks up MatchRecorded - disjoint from the invariant's
    // consequent (MatchVerified) - so it front-loads nothing.
    assert!(
        decide.gates[0].front_loads.is_empty(),
        "{:?}",
        decide.gates[0]
    );
}

#[test]
fn a_shared_predicate_with_unrelated_arguments_still_links_as_syntactic() {
    // The relation is predicate-name overlap, not entailment: a gate and
    // an invariant consequent that both mention `Standing` link even when
    // the arguments are unrelated. That is honest only because the surface
    // calls it a shared predicate / front-loads, never a proof. This test
    // pins the no-overclaim behaviour, not a desirable precise link.
    let p = program("syntactic_overlap")
        .predicates(vec![
            predicate("Thing").subject("t").build(),
            predicate("Standing").subject("x").build(),
        ])
        .invariants(vec![invariant(
            "thing_needs_standing",
            implies(
                claim("Thing", vec![var("t")]),
                claim("Standing", vec![var("t")]),
            ),
        )])
        .transformations(vec![morpholog_core::ir_builder::transformation(
            "make_thing",
            params(&["t", "other"]),
            vec![
                require(claim("Standing", vec![var("other")])),
                assert_("Thing", vec![var("t")]),
            ],
        )])
        .build();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
    let gate = &controls(&p).transformations[0].gates[0];
    assert_eq!(
        gate.front_loads.len(),
        1,
        "predicate overlap links even with unrelated args"
    );
    assert_eq!(gate.front_loads[0].triggered_by, vec!["Thing"]);
    assert_eq!(gate.front_loads[0].shared, vec!["Standing"]);
}

#[test]
fn rendered_matrix_shows_the_front_loads_link_and_failure_shape() {
    let rendered = render_controls(&controls(&mini()));
    for fragment in [
        "front-loads invariant `decision_rests_on_two_distinct_verifiers`",
        "triggered by: Decision",
        "shared: MatchVerified",
        "failure shape:",
    ] {
        assert!(
            rendered.contains(fragment),
            "`{fragment}` not in:\n{rendered}"
        );
    }
}

#[test]
fn the_control_matrix_is_deterministic() {
    // Links derive from sorted sets and declaration-ordered walks, so two
    // runs over the same programme are identical - what a compliance
    // mapping cited rule by rule must be able to rely on.
    assert_eq!(controls(&mini()), controls(&mini()));
}

#[test]
fn front_line_coverage_names_the_invariant_side() {
    let matrix = controls(&mini());
    let inv = matrix
        .front_line_coverage
        .iter()
        .find(|i| i.invariant == "decision_rests_on_two_distinct_verifiers")
        .expect("the implication invariant is in the front-line coverage");
    assert_eq!(inv.front_loaded_by[0].transformation, "decide");
    assert_eq!(inv.front_loaded_by[0].form, "require");
    // The not(...) invariant is outside the relation's domain - not listed.
    assert!(
        !matrix
            .front_line_coverage
            .iter()
            .any(|i| i.invariant == "no_free_floating_decisions"),
        "{matrix:?}"
    );
    // Rendered prose surfaces the section.
    let rendered = render_controls(&matrix);
    assert!(rendered.contains("Front-line coverage for authored implication-shaped invariants"));
    assert!(rendered.contains("front-loaded by:"));
}

#[test]
fn partial_coverage_of_a_multi_implication_invariant_stays_visible() {
    // One invariant, two implications: a gate front-loads the first, none
    // the second. Per-implication granularity must show one front-loaded
    // and one backstop row - not a single misleading "covered" entry.
    let p = program("partial")
        .predicates(vec![
            predicate("A").subject("x").build(),
            predicate("B").subject("x").build(),
            predicate("C").subject("x").build(),
            predicate("D").subject("x").build(),
        ])
        .invariants(vec![invariant(
            "k",
            and(vec![
                implies(claim("A", vec![var("x")]), claim("B", vec![var("x")])),
                implies(claim("C", vec![var("y")]), claim("D", vec![var("y")])),
            ]),
        )])
        .transformations(vec![morpholog_core::ir_builder::transformation(
            "t",
            params(&["x"]),
            vec![
                require(claim("B", vec![var("x")])),
                assert_("A", vec![var("x")]),
                assert_("C", vec![var("x")]),
            ],
        )])
        .build();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
    let rows: Vec<_> = controls(&p)
        .front_line_coverage
        .into_iter()
        .filter(|i| i.invariant == "k")
        .collect();
    assert_eq!(rows.len(), 2, "one row per implication: {rows:?}");
    assert_eq!(
        rows.iter()
            .filter(|i| !i.front_loaded_by.is_empty())
            .count(),
        1,
        "A=>B is front-loaded: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|i| i.front_loaded_by.is_empty() && !i.triggered_by_transformations.is_empty())
            .count(),
        1,
        "C=>D is a backstop: {rows:?}"
    );
}

#[test]
fn a_dormant_implication_is_distinguished_from_a_backstop() {
    // An authored implication whose antecedent no transformation admits is
    // dormant (untriggered), not a backstop gap - the surface must not make
    // it sound as urgent as a triggerable-but-unguarded rule.
    let p = program("dormant")
        .predicates(vec![
            predicate("E").subject("x").build(),
            predicate("F").subject("x").build(),
        ])
        .invariants(vec![invariant(
            "m",
            implies(claim("E", vec![var("x")]), claim("F", vec![var("x")])),
        )])
        .transformations(vec![morpholog_core::ir_builder::transformation(
            "g",
            params(&["x"]),
            vec![assert_("F", vec![var("x")])],
        )])
        .build();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
    let cov = controls(&p).front_line_coverage;
    let m = cov
        .iter()
        .find(|i| i.invariant == "m")
        .expect("m is analysable");
    assert!(m.front_loaded_by.is_empty(), "{m:?}");
    assert!(
        m.triggered_by_transformations.is_empty(),
        "no transformation admits E, so m is dormant: {m:?}"
    );
}

#[test]
fn two_invariants_with_the_same_failure_shape_keep_separate_front_loaders() {
    // The inversion keys on (invariant, failure_shape), not the shape
    // alone. Two invariants with identical bodies render the same failure
    // shape; one gate front-loads both. Shape-only keying would collect
    // the gate ref twice and list a duplicate on each row - this pins one
    // ref per invariant.
    let p = program("same_shape")
        .predicates(vec![
            predicate("A").subject("x").build(),
            predicate("B").subject("x").build(),
        ])
        .invariants(vec![
            invariant(
                "i1",
                implies(claim("A", vec![var("x")]), claim("B", vec![var("x")])),
            ),
            invariant(
                "i2",
                implies(claim("A", vec![var("x")]), claim("B", vec![var("x")])),
            ),
        ])
        .transformations(vec![morpholog_core::ir_builder::transformation(
            "t",
            params(&["x"]),
            vec![
                require(claim("B", vec![var("x")])),
                assert_("A", vec![var("x")]),
            ],
        )])
        .build();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
    let cov = controls(&p).front_line_coverage;
    for name in ["i1", "i2"] {
        let row = cov
            .iter()
            .find(|i| i.invariant == name)
            .unwrap_or_else(|| panic!("{name} present"));
        assert_eq!(
            row.front_loaded_by.len(),
            1,
            "exactly one front-loader, not a duplicate from the other invariant: {row:?}"
        );
        assert_eq!(row.front_loaded_by[0].transformation, "t");
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
