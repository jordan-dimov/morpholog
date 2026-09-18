//! Admission is case-local revalidation. A transition is admitted when
//! every case its effective delta could affect satisfies each invariant
//! afterwards; inherited dirt elsewhere does not block it; touching a
//! dirty case without repairing it still refuses; a transition that
//! changes nothing is admitted whatever the history holds. The
//! invariant itself keeps its whole-state meaning.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    assert_, claim, eq, implies, invariant, params, predicate, program, retract, sum, term,
    transformation, var, wildcard,
};
use morpholog_core::{
    Outcome, Program, RejectionReason, State, TraceEntry, TracedProposal, Transition,
    eval_invariant, propose, propose_with_trace,
};
use morpholog_test_support::{claim_instance, dec, subj, test_actor};

/// A ledger of entries whose lines must balance, with a posting that
/// adds one line and a retraction that removes one.
fn ledger() -> Program {
    program("ledger")
        .predicates(vec![
            predicate("Entry").subject("e").build(),
            predicate("Line")
                .subject("e")
                .subject("side")
                .decimal("dr")
                .decimal("cr")
                .build(),
        ])
        .invariants(vec![invariant(
            "balanced",
            implies(
                claim("Entry", vec![var("e")]),
                eq(
                    sum(
                        term(var("d")),
                        claim("Line", vec![var("e"), wildcard(), var("d"), wildcard()]),
                    ),
                    sum(
                        term(var("c")),
                        claim("Line", vec![var("e"), wildcard(), wildcard(), var("c")]),
                    ),
                ),
            ),
        )])
        .transformations(vec![
            transformation(
                "post",
                params(&["e", "side", "dr", "cr"]),
                vec![
                    assert_("Entry", vec![var("e")]),
                    assert_("Line", vec![var("e"), var("side"), var("dr"), var("cr")]),
                ],
            ),
            transformation(
                "unpost",
                params(&["e", "side", "dr", "cr"]),
                vec![retract(
                    "Line",
                    vec![var("e"), var("side"), var("dr"), var("cr")],
                )],
            ),
        ])
        .build()
}

fn transition(name: &str, args: Vec<morpholog_core::EvalValue>) -> Transition {
    Transition {
        transformation_name: name.into(),
        args,
        actor: test_actor(),
    }
}

/// History holds an entry that does not balance.
fn dirty() -> State {
    State::from_claims(vec![
        claim_instance("Entry", &[subj("legacy")]),
        claim_instance("Line", &[subj("legacy"), subj("cash"), dec(100), dec(0)]),
    ])
}

fn run(p: &Program, t: &Transition, state: &State) -> Outcome {
    let tf = p
        .transformations
        .iter()
        .find(|t0| t0.name == t.transformation_name)
        .unwrap();
    propose(tf, t, state, &p.invariants, &p.definitions).unwrap()
}

#[test]
fn inherited_dirt_blocks_only_the_transitions_that_touch_it() {
    let p = ledger();
    let state = dirty();
    // The rule itself says the state is unlawful.
    assert_eq!(
        eval_invariant(&p.invariants[0], &state, None, &[]),
        Ok(false)
    );

    // A balanced posting elsewhere is admitted.
    let fresh = transition("post", vec![subj("e1"), subj("cash"), dec(5), dec(5)]);
    assert!(matches!(run(&p, &fresh, &state), Outcome::Accepted { .. }));

    // Touching the dirty entry without repairing it is refused, and the
    // witness names that entry.
    let touch = transition("post", vec![subj("legacy"), subj("fee"), dec(1), dec(0)]);
    let Outcome::Rejected {
        reason: RejectionReason::Invariant { name, witness, .. },
    } = run(&p, &touch, &state)
    else {
        panic!("touching the dirty case refuses");
    };
    assert_eq!(name.as_str(), "balanced");
    assert_eq!(witness.len(), 1);
    assert_eq!(witness[0].var.as_str(), "e");
    assert_eq!(witness[0].value, subj("legacy"));

    // Repairing it is admitted.
    let repair = transition("post", vec![subj("legacy"), subj("rev"), dec(0), dec(100)]);
    assert!(matches!(run(&p, &repair, &state), Outcome::Accepted { .. }));
}

#[test]
fn a_bounded_refusal_never_blames_a_case_the_transition_did_not_reach() {
    let p = ledger();
    // Two dirty entries; the one touched sorts after the one left alone.
    let state = State::from_claims(vec![
        claim_instance("Entry", &[subj("a_legacy")]),
        claim_instance("Line", &[subj("a_legacy"), subj("cash"), dec(1), dec(0)]),
        claim_instance("Entry", &[subj("b_legacy")]),
        claim_instance("Line", &[subj("b_legacy"), subj("cash"), dec(1), dec(0)]),
    ]);
    let touch = transition("post", vec![subj("b_legacy"), subj("fee"), dec(2), dec(0)]);
    let Outcome::Rejected {
        reason: RejectionReason::Invariant { witness, .. },
    } = run(&p, &touch, &state)
    else {
        panic!("refuses");
    };
    assert_eq!(witness[0].value, subj("b_legacy"));
}

#[test]
fn a_transition_that_changes_nothing_is_admitted_whatever_the_history() {
    let p = ledger();
    let state = dirty();
    // Admitting what is already admitted, even on the dirty entry.
    let duplicate = transition("post", vec![subj("legacy"), subj("cash"), dec(100), dec(0)]);
    assert!(matches!(
        run(&p, &duplicate, &state),
        Outcome::Accepted { .. }
    ));
    // Retracting what is absent.
    let absent = transition(
        "unpost",
        vec![subj("legacy"), subj("ghost"), dec(1), dec(1)],
    );
    assert!(matches!(run(&p, &absent, &state), Outcome::Accepted { .. }));
}

#[test]
fn a_retract_and_readmit_in_one_delta_changes_nothing() {
    let mut p = ledger();
    p.transformations.push(transformation(
        "churn",
        params(&["e", "side", "dr", "cr"]),
        vec![
            retract("Line", vec![var("e"), var("side"), var("dr"), var("cr")]),
            assert_("Line", vec![var("e"), var("side"), var("dr"), var("cr")]),
        ],
    ));
    let state = dirty();
    let churn = transition(
        "churn",
        vec![subj("legacy"), subj("cash"), dec(100), dec(0)],
    );
    assert!(matches!(run(&p, &churn, &state), Outcome::Accepted { .. }));
}

#[test]
fn the_trace_records_only_the_obligations_evaluated() {
    let p = ledger();
    let state = dirty();
    let fresh = transition("post", vec![subj("e1"), subj("cash"), dec(5), dec(5)]);
    let tf = &p.transformations[0];
    let TracedProposal::Completed { trace, .. } =
        propose_with_trace(tf, &fresh, &state, &p.invariants, &p.definitions)
    else {
        panic!("completes");
    };
    let checks: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, TraceEntry::InvariantCheck { .. }))
        .collect();
    assert_eq!(
        checks.len(),
        1,
        "the touched obligation, evaluated and held"
    );
    assert!(matches!(
        checks[0],
        TraceEntry::InvariantCheck { held: true, .. }
    ));

    // A delta that touches nothing evaluates nothing.
    let duplicate = transition("post", vec![subj("legacy"), subj("cash"), dec(100), dec(0)]);
    let TracedProposal::Completed { trace, .. } =
        propose_with_trace(tf, &duplicate, &state, &p.invariants, &p.definitions)
    else {
        panic!("completes");
    };
    assert!(
        !trace
            .iter()
            .any(|e| matches!(e, TraceEntry::InvariantCheck { .. })),
        "nothing to evaluate: {trace:?}"
    );
}
